use std::{
    fs,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use tauri::{path::BaseDirectory, AppHandle, Emitter, Manager};

use crate::{
    continuity,
    models::{
        ActivityEvent, ActivityGroup, ActivityKind, ActivityStatus, ActivityTimeline,
        BrowserActionRecord, BrowserActionStatus, ChangeOutcome, ChangeRecord, ChangeStatus,
        CommandOutcome, CommandRecord, CommandStatus, GitActionRecord, GitActionStatus,
        LaunchActionRecord, LaunchActionStatus, ManagedProcessOutcome, ManagedProcessRecord,
        ManagedProcessStatus, TerminalCommandOutcome, TerminalCommandRecord, TerminalCommandStatus,
        Workspace,
    },
    storage, terminal, versioning,
};

const ACTIVITY_HISTORY_FILE: &str = "activity-history.json";
const MAX_GROUPS: usize = 2_000;
const MAX_EVENTS_PER_GROUP: usize = 160;
const MAX_SUMMARY_CHARS: usize = 240;
const MAX_DETAIL_CHARS: usize = 4_000;
static ACTIVITY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn activity_lock() -> &'static Mutex<()> {
    ACTIVITY_LOCK.get_or_init(|| Mutex::new(()))
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

fn path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(ACTIVITY_HISTORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel activity storage: {error}"))
}

fn load(app: &AppHandle) -> Result<Vec<ActivityGroup>, String> {
    let path = path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read AI activity history: {error}"))?;
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("Saved AI activity history is invalid: {error}"))
}

fn save(app: &AppHandle, groups: &[ActivityGroup]) -> Result<(), String> {
    let path = path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel activity storage directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not prepare AI activity storage: {error}"))?;
    let text = serde_json::to_string_pretty(groups)
        .map_err(|error| format!("Could not serialize AI activity history: {error}"))?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(ACTIVITY_HISTORY_FILE);
    let temporary = parent.join(format!(".{file_name}.{}.tmp", new_id("save")));
    fs::write(&temporary, text)
        .map_err(|error| format!("Could not stage AI activity history: {error}"))?;
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Could not save AI activity history: {error}"));
    }
    Ok(())
}

fn group_has_inflight(group: &ActivityGroup) -> bool {
    group.events.iter().any(|event| {
        matches!(
            event.status,
            ActivityStatus::Pending | ActivityStatus::Running
        )
    })
}

fn prune_workspace_groups(
    groups: &mut Vec<ActivityGroup>,
    workspace_id: &str,
    limit: usize,
) -> usize {
    let before = groups
        .iter()
        .filter(|group| group.workspace_id == workspace_id)
        .count();

    let mut ordinary = groups
        .iter()
        .filter(|group| group.workspace_id == workspace_id && !group_has_inflight(group))
        .map(|group| (group.id.clone(), group.updated_at))
        .collect::<Vec<_>>();
    ordinary.sort_by_key(|(_, updated_at)| std::cmp::Reverse(*updated_at));
    let keep_ordinary = ordinary
        .into_iter()
        .take(limit)
        .map(|(id, _)| id)
        .collect::<std::collections::BTreeSet<_>>();

    groups.retain(|group| {
        group.workspace_id != workspace_id
            || group_has_inflight(group)
            || keep_ordinary.contains(&group.id)
    });

    let after = groups
        .iter()
        .filter(|group| group.workspace_id == workspace_id)
        .count();
    before.saturating_sub(after)
}

fn trim_global_groups(groups: &mut Vec<ActivityGroup>) {
    groups.sort_by_key(|group| group.updated_at);
    while groups.len() > MAX_GROUPS {
        let Some(index) = groups.iter().position(|group| !group_has_inflight(group)) else {
            break;
        };
        groups.remove(index);
    }
}

fn bounded(value: impl AsRef<str>, max_chars: usize) -> String {
    let value = value.as_ref();
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

fn refresh_version_links(app: &AppHandle, group: &mut ActivityGroup) {
    let Some(trace_group_id) = group.trace_group_id.as_deref() else {
        return;
    };
    if let Ok(timeline) = versioning::timeline(app, Some(&group.workspace_id)) {
        group.version_ids = timeline
            .records
            .into_iter()
            .filter(|record| record.edit_group_id.as_deref() == Some(trace_group_id))
            .map(|record| record.id)
            .collect();
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    kind: ActivityKind,
    action: impl Into<String>,
    summary: impl Into<String>,
    detail: Option<String>,
    status: ActivityStatus,
    source_id: Option<String>,
) -> Result<(), String> {
    let _guard = activity_lock()
        .lock()
        .map_err(|_| "RepoTunnel activity history is temporarily unavailable.".to_string())?;
    let now = now_millis();
    let action = bounded(action.into(), 80);
    let summary = bounded(summary.into(), MAX_SUMMARY_CHARS);
    let detail = detail.map(|value| bounded(value, MAX_DETAIL_CHARS));
    let mut groups = load(app)?;

    let index = trace_group_id.and_then(|trace| {
        groups.iter().position(|group| {
            group.workspace_id == workspace.id && group.trace_group_id.as_deref() == Some(trace)
        })
    });

    let event = ActivityEvent {
        id: new_id("activity-event"),
        kind,
        action,
        summary: summary.clone(),
        detail,
        status,
        source_id,
        created_at: now,
        updated_at: now,
    };

    if let Some(index) = index {
        let group = &mut groups[index];
        if group.events.len() >= MAX_EVENTS_PER_GROUP {
            group.events.remove(0);
        }
        group.events.push(event);
        group.updated_at = now;
        refresh_version_links(app, group);
    } else {
        let mut group = ActivityGroup {
            id: trace_group_id
                .map(|trace| format!("{}:{trace}", workspace.id))
                .unwrap_or_else(|| new_id("activity")),
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
            trace_group_id: trace_group_id.map(str::to_owned),
            summary,
            version_ids: Vec::new(),
            events: vec![event],
            created_at: now,
            updated_at: now,
        };
        refresh_version_links(app, &mut group);
        groups.push(group);
    }

    groups.sort_by_key(|group| group.updated_at);
    if let Ok(settings) = storage::load_history_settings(app) {
        if let Some(limit) = settings.version_history_limit {
            prune_workspace_groups(&mut groups, &workspace.id, limit);
        }
    }
    trim_global_groups(&mut groups);
    let continuity_groups = groups
        .iter()
        .filter(|group| group.workspace_id == workspace.id && group.updated_at == now)
        .cloned()
        .collect::<Vec<_>>();
    save(app, &groups)?;
    drop(_guard);
    continuity::capture_activity_groups(app, &continuity_groups);
    let _ = app.emit("repotunnel://activity-updated", ());
    Ok(())
}

pub(crate) fn update_source(
    app: &AppHandle,
    source_id: &str,
    status: ActivityStatus,
    detail: Option<String>,
) -> Result<(), String> {
    let _guard = activity_lock()
        .lock()
        .map_err(|_| "RepoTunnel activity history is temporarily unavailable.".to_string())?;
    let mut groups = load(app)?;
    let now = now_millis();
    let mut changed = false;
    let mut affected_workspaces = std::collections::BTreeSet::new();
    for group in &mut groups {
        let mut group_changed = false;
        for event in &mut group.events {
            if event.source_id.as_deref() == Some(source_id) {
                event.status = status;
                if let Some(detail) = detail.as_ref() {
                    event.detail = Some(bounded(detail, MAX_DETAIL_CHARS));
                }
                event.updated_at = now;
                group.updated_at = now;
                group_changed = true;
                changed = true;
            }
        }
        if group_changed {
            affected_workspaces.insert(group.workspace_id.clone());
            refresh_version_links(app, group);
        }
    }
    if changed {
        if let Ok(settings) = storage::load_history_settings(app) {
            if let Some(limit) = settings.version_history_limit {
                for workspace_id in &affected_workspaces {
                    prune_workspace_groups(&mut groups, workspace_id, limit);
                }
            }
        }
        trim_global_groups(&mut groups);
        let continuity_groups = groups
            .iter()
            .filter(|group| {
                affected_workspaces.contains(&group.workspace_id) && group.updated_at == now
            })
            .cloned()
            .collect::<Vec<_>>();
        save(app, &groups)?;
        drop(_guard);
        continuity::capture_activity_groups(app, &continuity_groups);
        let _ = app.emit("repotunnel://activity-updated", ());
    }
    Ok(())
}

fn reconcile_process_event(event: &mut ActivityEvent, process: &ManagedProcessRecord) -> bool {
    let mut changed = false;
    if event.source_id.as_deref() != Some(process.id.as_str()) {
        event.source_id = Some(process.id.clone());
        changed = true;
    }
    let status = process_status(process.status);
    let detail = process
        .error
        .clone()
        .or_else(|| process.pid.map(|pid| format!("PID: {pid}")));
    if event.status != status || event.detail != detail || event.updated_at != process.updated_at {
        event.status = status;
        event.detail = detail;
        // Preserve the factual process timestamp. A read/reconcile operation must never make
        // historical activity appear newer than work that actually happened later.
        event.updated_at = process.updated_at;
        changed = true;
    }
    changed
}

pub(crate) fn timeline(
    app: &AppHandle,
    workspace_id: Option<&str>,
) -> Result<ActivityTimeline, String> {
    let _guard = activity_lock()
        .lock()
        .map_err(|_| "RepoTunnel activity history is temporarily unavailable.".to_string())?;
    let mut groups = load(app)?;
    let mut changed = false;
    for group in &mut groups {
        refresh_version_links(app, group);
        let processes =
            terminal::list_processes(app, Some(&group.workspace_id), 100).unwrap_or_default();
        for event in &mut group.events {
            if event.kind != ActivityKind::Process {
                continue;
            }
            let process = event
                .source_id
                .as_deref()
                .and_then(|source_id| processes.iter().find(|process| process.id == source_id))
                .or_else(|| {
                    processes.iter().find(|process| {
                        event.summary
                            == format!("{} · {}", process.label, bounded(&process.command, 160))
                    })
                });
            let Some(process) = process else {
                continue;
            };
            changed |= reconcile_process_event(event, process);
        }
        let factual_updated_at = group
            .events
            .iter()
            .map(|event| event.updated_at)
            .max()
            .unwrap_or(group.created_at);
        if group.updated_at != factual_updated_at {
            group.updated_at = factual_updated_at;
            changed = true;
        }
    }
    if changed {
        save(app, &groups)?;
    }
    if let Some(workspace_id) = workspace_id {
        groups.retain(|group| group.workspace_id == workspace_id);
    }
    groups.sort_by_key(|group| std::cmp::Reverse(group.updated_at));
    Ok(ActivityTimeline { groups })
}

pub(crate) fn clear_workspace(app: &AppHandle, workspace_id: &str) -> Result<usize, String> {
    let _guard = activity_lock()
        .lock()
        .map_err(|_| "RepoTunnel activity history is temporarily unavailable.".to_string())?;
    let groups = load(app)?;
    let before = groups
        .iter()
        .filter(|group| group.workspace_id == workspace_id)
        .count();
    let mut retained = Vec::with_capacity(groups.len());
    for mut group in groups {
        if group.workspace_id != workspace_id {
            retained.push(group);
            continue;
        }
        group.events.retain(|event| {
            matches!(
                event.status,
                ActivityStatus::Pending | ActivityStatus::Running
            )
        });
        if group.events.is_empty() {
            continue;
        }
        group.version_ids.clear();
        group.summary = group.events[0].summary.clone();
        group.created_at = group
            .events
            .iter()
            .map(|event| event.created_at)
            .min()
            .unwrap_or(group.created_at);
        group.updated_at = group
            .events
            .iter()
            .map(|event| event.updated_at)
            .max()
            .unwrap_or(group.updated_at);
        retained.push(group);
    }
    let after = retained
        .iter()
        .filter(|group| group.workspace_id == workspace_id)
        .count();
    save(app, &retained)?;
    let _ = app.emit("repotunnel://activity-updated", ());
    Ok(before.saturating_sub(after))
}

pub(crate) fn apply_retention(
    app: &AppHandle,
    workspace_id: &str,
    limit: usize,
) -> Result<usize, String> {
    let _guard = activity_lock()
        .lock()
        .map_err(|_| "RepoTunnel activity history is temporarily unavailable.".to_string())?;
    let mut groups = load(app)?;
    let removed = prune_workspace_groups(&mut groups, workspace_id, limit);
    trim_global_groups(&mut groups);
    if removed > 0 {
        save(app, &groups)?;
        let _ = app.emit("repotunnel://activity-updated", ());
    }
    Ok(removed)
}

pub(crate) fn record_sandbox_command(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    outcome: &CommandOutcome,
) -> Result<(), String> {
    let status = match outcome.command.status {
        CommandStatus::Pending => ActivityStatus::Pending,
        CommandStatus::Running => ActivityStatus::Running,
        CommandStatus::Completed => ActivityStatus::Succeeded,
        CommandStatus::Failed | CommandStatus::TimedOut => ActivityStatus::Failed,
        CommandStatus::Rejected => ActivityStatus::Rejected,
        CommandStatus::Cancelled => ActivityStatus::Stopped,
    };
    record(
        app,
        workspace,
        trace_group_id,
        ActivityKind::Verification,
        "sandboxVerification",
        format!("Verified with {}", bounded(&outcome.command.command, 180)),
        output_detail(
            &outcome.command.stdout,
            &outcome.command.stderr,
            outcome.command.exit_code,
            outcome.command.error.as_deref(),
        ),
        status,
        Some(outcome.command.id.clone()),
    )
}

pub(crate) fn looks_like_verification(command: &str) -> bool {
    let value = command.to_ascii_lowercase();
    [
        " test",
        "test ",
        "npm test",
        "npm run test",
        "pnpm test",
        "yarn test",
        "pytest",
        "cargo test",
        "cargo check",
        "cargo clippy",
        "npm run build",
        "pnpm build",
        "yarn build",
        "npm run lint",
        "pnpm lint",
        "yarn lint",
        "typecheck",
        "tsc",
        "vite build",
        "cargo build",
    ]
    .iter()
    .any(|needle| value.contains(needle))
}

fn terminal_status(status: TerminalCommandStatus) -> ActivityStatus {
    match status {
        TerminalCommandStatus::Pending => ActivityStatus::Pending,
        TerminalCommandStatus::Running => ActivityStatus::Running,
        TerminalCommandStatus::Completed => ActivityStatus::Succeeded,
        TerminalCommandStatus::Failed | TerminalCommandStatus::TimedOut => ActivityStatus::Failed,
        TerminalCommandStatus::Rejected => ActivityStatus::Rejected,
    }
}

fn process_status(status: ManagedProcessStatus) -> ActivityStatus {
    match status {
        ManagedProcessStatus::Pending => ActivityStatus::Pending,
        ManagedProcessStatus::Running => ActivityStatus::Running,
        ManagedProcessStatus::Exited => ActivityStatus::Succeeded,
        ManagedProcessStatus::Stopped => ActivityStatus::Stopped,
        ManagedProcessStatus::Failed => ActivityStatus::Failed,
        ManagedProcessStatus::Rejected => ActivityStatus::Rejected,
    }
}

fn output_detail(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    error: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(exit_code) = exit_code {
        parts.push(format!("Exit code: {exit_code}"));
    }
    if !stdout.trim().is_empty() {
        parts.push(format!("stdout:\n{}", bounded(stdout.trim(), 1_600)));
    }
    if !stderr.trim().is_empty() {
        parts.push(format!("stderr:\n{}", bounded(stderr.trim(), 1_600)));
    }
    if let Some(error) = error.filter(|value| !value.trim().is_empty()) {
        parts.push(format!("Error: {}", bounded(error, 800)));
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

pub(crate) fn record_change_outcome(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    outcome: &ChangeOutcome,
) -> Result<(), String> {
    let status = match outcome.change.status {
        ChangeStatus::Pending => ActivityStatus::Pending,
        ChangeStatus::Applied => ActivityStatus::Succeeded,
        ChangeStatus::Rejected => ActivityStatus::Rejected,
        ChangeStatus::Undone => ActivityStatus::Stopped,
        ChangeStatus::Failed => ActivityStatus::Failed,
    };
    record(
        app,
        workspace,
        trace_group_id,
        ActivityKind::Files,
        format!("{:?}", outcome.change.operation),
        outcome.change.summary.clone(),
        outcome.change.diff.clone(),
        status,
        Some(outcome.change.id.clone()),
    )
}

pub(crate) fn record_terminal_outcome(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    outcome: &TerminalCommandOutcome,
) -> Result<(), String> {
    record_terminal_record(app, workspace, trace_group_id, &outcome.command)
}

pub(crate) fn record_terminal_record(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    command: &TerminalCommandRecord,
) -> Result<(), String> {
    let kind = if looks_like_verification(&command.command) {
        ActivityKind::Verification
    } else {
        ActivityKind::Terminal
    };
    record(
        app,
        workspace,
        trace_group_id,
        kind,
        "runCommand",
        format!("Ran {}", bounded(&command.command, 180)),
        output_detail(
            &command.stdout,
            &command.stderr,
            command.exit_code,
            command.error.as_deref(),
        ),
        terminal_status(command.status),
        Some(command.id.clone()),
    )
}

pub(crate) fn record_process_outcome(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    outcome: &ManagedProcessOutcome,
) -> Result<(), String> {
    record_process_record(
        app,
        workspace,
        trace_group_id,
        "startProcess",
        &outcome.process,
    )
}

pub(crate) fn record_process_record(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    action: &str,
    process: &ManagedProcessRecord,
) -> Result<(), String> {
    let detail = process
        .error
        .as_ref()
        .map(|error| format!("PID: {:?}\n{}", process.pid, error))
        .or_else(|| process.pid.map(|pid| format!("PID: {pid}")));
    record(
        app,
        workspace,
        trace_group_id,
        ActivityKind::Process,
        action,
        format!("{} · {}", process.label, bounded(&process.command, 160)),
        detail,
        process_status(process.status),
        Some(process.id.clone()),
    )
}

pub(crate) fn record_launch_record(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    launch: &LaunchActionRecord,
) -> Result<(), String> {
    let status = match launch.status {
        LaunchActionStatus::Pending => ActivityStatus::Pending,
        LaunchActionStatus::Launched => ActivityStatus::Succeeded,
        LaunchActionStatus::Failed => ActivityStatus::Failed,
        LaunchActionStatus::Rejected => ActivityStatus::Rejected,
    };
    record(
        app,
        workspace,
        trace_group_id,
        ActivityKind::Launcher,
        "launch",
        format!("Opened {}", bounded(&launch.target, 180)),
        launch.error.clone(),
        status,
        Some(launch.id.clone()),
    )
}

pub(crate) fn record_browser_record(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    action: &BrowserActionRecord,
) -> Result<(), String> {
    let status = match action.status {
        BrowserActionStatus::Pending => ActivityStatus::Pending,
        BrowserActionStatus::Applied => ActivityStatus::Succeeded,
        BrowserActionStatus::Failed => ActivityStatus::Failed,
        BrowserActionStatus::Rejected => ActivityStatus::Rejected,
    };
    record(
        app,
        workspace,
        trace_group_id,
        ActivityKind::Browser,
        format!("{:?}", action.kind),
        format!("Browser · {}", bounded(&action.target, 180)),
        action.error.clone().or_else(|| action.detail.clone()),
        status,
        Some(action.id.clone()),
    )
}

pub(crate) fn record_git_record(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    action: &GitActionRecord,
) -> Result<(), String> {
    let status = match action.status {
        GitActionStatus::Pending => ActivityStatus::Pending,
        GitActionStatus::Applied => ActivityStatus::Succeeded,
        GitActionStatus::Rejected => ActivityStatus::Rejected,
        GitActionStatus::Failed => ActivityStatus::Failed,
    };
    record(
        app,
        workspace,
        trace_group_id,
        ActivityKind::Git,
        format!("{:?}", action.kind),
        action.summary.clone(),
        action.error.clone().or_else(|| action.detail.clone()),
        status,
        Some(action.id.clone()),
    )
}

pub(crate) fn sync_change(app: &AppHandle, record: &ChangeRecord) {
    let status = match record.status {
        ChangeStatus::Pending => ActivityStatus::Pending,
        ChangeStatus::Applied => ActivityStatus::Succeeded,
        ChangeStatus::Rejected => ActivityStatus::Rejected,
        ChangeStatus::Undone => ActivityStatus::Stopped,
        ChangeStatus::Failed => ActivityStatus::Failed,
    };
    let _ = update_source(
        app,
        &record.id,
        status,
        record.error.clone().or_else(|| record.diff.clone()),
    );
}

pub(crate) fn sync_sandbox_command(app: &AppHandle, record: &CommandRecord) {
    let status = match record.status {
        CommandStatus::Pending => ActivityStatus::Pending,
        CommandStatus::Running => ActivityStatus::Running,
        CommandStatus::Completed => ActivityStatus::Succeeded,
        CommandStatus::Failed | CommandStatus::TimedOut => ActivityStatus::Failed,
        CommandStatus::Rejected => ActivityStatus::Rejected,
        CommandStatus::Cancelled => ActivityStatus::Stopped,
    };
    let _ = update_source(
        app,
        &record.id,
        status,
        output_detail(
            &record.stdout,
            &record.stderr,
            record.exit_code,
            record.error.as_deref(),
        ),
    );
}

pub(crate) fn sync_terminal(app: &AppHandle, record: &TerminalCommandRecord) {
    let _ = update_source(
        app,
        &record.id,
        terminal_status(record.status),
        output_detail(
            &record.stdout,
            &record.stderr,
            record.exit_code,
            record.error.as_deref(),
        ),
    );
}

pub(crate) fn sync_process(app: &AppHandle, record: &ManagedProcessRecord) {
    let detail = record
        .error
        .clone()
        .or_else(|| record.pid.map(|pid| format!("PID: {pid}")));
    let _ = update_source(app, &record.id, process_status(record.status), detail);
}

pub(crate) fn sync_launch(app: &AppHandle, record: &LaunchActionRecord) {
    let status = match record.status {
        LaunchActionStatus::Pending => ActivityStatus::Pending,
        LaunchActionStatus::Launched => ActivityStatus::Succeeded,
        LaunchActionStatus::Failed => ActivityStatus::Failed,
        LaunchActionStatus::Rejected => ActivityStatus::Rejected,
    };
    let _ = update_source(app, &record.id, status, record.error.clone());
}

pub(crate) fn sync_browser(app: &AppHandle, record: &BrowserActionRecord) {
    let status = match record.status {
        BrowserActionStatus::Pending => ActivityStatus::Pending,
        BrowserActionStatus::Applied => ActivityStatus::Succeeded,
        BrowserActionStatus::Failed => ActivityStatus::Failed,
        BrowserActionStatus::Rejected => ActivityStatus::Rejected,
    };
    let _ = update_source(
        app,
        &record.id,
        status,
        record.error.clone().or_else(|| record.detail.clone()),
    );
}

pub(crate) fn sync_git(app: &AppHandle, record: &GitActionRecord) {
    let status = match record.status {
        GitActionStatus::Pending => ActivityStatus::Pending,
        GitActionStatus::Applied => ActivityStatus::Succeeded,
        GitActionStatus::Rejected => ActivityStatus::Rejected,
        GitActionStatus::Failed => ActivityStatus::Failed,
    };
    let _ = update_source(
        app,
        &record.id,
        status,
        record.error.clone().or_else(|| record.detail.clone()),
    );
}

#[cfg(test)]
mod tests {
    use super::reconcile_process_event;
    use crate::models::{
        ActivityEvent, ActivityKind, ActivityStatus, ManagedProcessRecord, ManagedProcessStatus,
    };

    #[test]
    fn process_reconciliation_preserves_factual_timestamp() {
        let mut event = ActivityEvent {
            id: "event-old".to_string(),
            kind: ActivityKind::Process,
            action: "startProcess".to_string(),
            summary: "old gate".to_string(),
            detail: Some("stale detail".to_string()),
            status: ActivityStatus::Failed,
            source_id: Some("process-old".to_string()),
            created_at: 10,
            // Simulate the previous bug: a resume read had rejuvenated this historical event.
            updated_at: 10_000,
        };
        let process = ManagedProcessRecord {
            id: "process-old".to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            label: "old gate".to_string(),
            command: "cargo test --lib".to_string(),
            cwd: ".".to_string(),
            status: ManagedProcessStatus::Exited,
            pid: None,
            created_at: 10,
            started_at: Some(11),
            updated_at: 30,
            exited_at: Some(30),
            exit_code: Some(0),
            restart_count: 0,
            error: None,
        };

        assert!(reconcile_process_event(&mut event, &process));
        assert_eq!(event.status, ActivityStatus::Succeeded);
        assert_eq!(event.updated_at, 30);
        assert_eq!(event.detail, None);
        assert!(!reconcile_process_event(&mut event, &process));
    }
}
