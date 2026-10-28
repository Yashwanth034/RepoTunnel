use std::{
    collections::HashMap,
    path::{Path, PathBuf},
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
    recovery_store, storage, terminal, versioning,
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

fn load_path(path: &Path) -> Result<Vec<ActivityGroup>, String> {
    Ok(
        recovery_store::load_json::<Vec<ActivityGroup>>(path, "AI activity history")?
            .value
            .unwrap_or_default(),
    )
}

fn load(app: &AppHandle) -> Result<Vec<ActivityGroup>, String> {
    load_path(&path(app)?)
}

fn save_path(path: &Path, groups: &[ActivityGroup]) -> Result<(), String> {
    recovery_store::save_json(path, &groups.to_vec(), "AI activity history")
}

fn save(app: &AppHandle, groups: &[ActivityGroup]) -> Result<(), String> {
    save_path(&path(app)?, groups)
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

pub(crate) fn latest_workspace_activity_at(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<u64, String> {
    let _guard = activity_lock()
        .lock()
        .map_err(|_| "RepoTunnel activity history is temporarily unavailable.".to_string())?;
    let groups = load(app)?
        .into_iter()
        .filter(|group| group.workspace_id == workspace_id)
        .collect::<Vec<_>>();
    Ok(continuity::latest_meaningful_activity_at(&groups))
}

/// Reconcile each project's expensive version and process histories once per
/// timeline request, not once per activity group. Workspace-specific reads
/// must not perform history I/O for unrelated projects.
fn reconcile_timeline_groups<FV, FP>(
    groups: &mut [ActivityGroup],
    workspace_id: Option<&str>,
    mut load_version_links: FV,
    mut load_processes: FP,
) -> bool
where
    FV: FnMut(&str) -> Option<HashMap<String, Vec<String>>>,
    FP: FnMut(&str) -> Vec<ManagedProcessRecord>,
{
    let mut version_cache: HashMap<String, Option<HashMap<String, Vec<String>>>> = HashMap::new();
    let mut process_cache: HashMap<String, Vec<ManagedProcessRecord>> = HashMap::new();
    let mut changed = false;

    for group in groups {
        if workspace_id.is_some_and(|id| id != group.workspace_id) {
            continue;
        }
        if let Some(trace_id) = group.trace_group_id.as_deref() {
            let links = version_cache
                .entry(group.workspace_id.clone())
                .or_insert_with(|| load_version_links(&group.workspace_id));
            if let Some(links) = links {
                let updated = links.get(trace_id).cloned().unwrap_or_default();
                if group.version_ids != updated {
                    group.version_ids = updated;
                    changed = true;
                }
            }
        }
        if group
            .events
            .iter()
            .any(|event| event.kind == ActivityKind::Process)
        {
            let processes = process_cache
                .entry(group.workspace_id.clone())
                .or_insert_with(|| load_processes(&group.workspace_id));
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
                if let Some(process) = process {
                    changed |= reconcile_process_event(event, process);
                }
            }
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
    let changed = reconcile_timeline_groups(
        &mut groups,
        workspace_id,
        |workspace| {
            let timeline = versioning::timeline(app, Some(workspace)).ok()?;
            let mut links: HashMap<String, Vec<String>> = HashMap::new();
            for record in timeline.records {
                if let Some(trace) = record.edit_group_id {
                    links.entry(trace).or_default().push(record.id);
                }
            }
            Some(links)
        },
        |workspace| terminal::list_processes(app, Some(workspace), 100).unwrap_or_default(),
    );
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
        BrowserActionStatus::Ambiguous => ActivityStatus::Failed,
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

pub(crate) fn sync_processes(app: &AppHandle, records: &[ManagedProcessRecord]) {
    if records.is_empty() {
        return;
    }
    let Ok(_guard) = activity_lock().lock() else {
        return;
    };
    let Ok(mut groups) = load(app) else {
        return;
    };

    let by_id = records
        .iter()
        .map(|record| (record.id.as_str(), record))
        .collect::<std::collections::HashMap<_, _>>();
    let mut changed = false;
    let mut affected_workspaces = std::collections::BTreeSet::new();

    for group in &mut groups {
        let mut group_changed = false;
        for event in &mut group.events {
            let Some(source_id) = event.source_id.as_deref() else {
                continue;
            };
            let Some(record) = by_id.get(source_id) else {
                continue;
            };
            if reconcile_process_event(event, record) {
                group.updated_at = group.updated_at.max(record.updated_at);
                group_changed = true;
                changed = true;
            }
        }
        if group_changed {
            affected_workspaces.insert(group.workspace_id.clone());
            refresh_version_links(app, group);
        }
    }

    if !changed {
        return;
    }

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
        .filter(|group| affected_workspaces.contains(&group.workspace_id))
        .cloned()
        .collect::<Vec<_>>();
    if save(app, &groups).is_err() {
        return;
    }
    drop(_guard);
    continuity::capture_activity_groups(app, &continuity_groups);
    let _ = app.emit("repotunnel://activity-updated", ());
}

pub(crate) fn sync_process(app: &AppHandle, record: &ManagedProcessRecord) {
    sync_processes(app, std::slice::from_ref(record));
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
        BrowserActionStatus::Ambiguous => ActivityStatus::Failed,
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
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{reconcile_process_event, reconcile_timeline_groups};
    use crate::models::{
        ActivityEvent, ActivityGroup, ActivityKind, ActivityStatus, ManagedProcessRecord,
        ManagedProcessStatus,
    };

    fn temp_store(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-activity-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn previous_path(path: &Path) -> PathBuf {
        let file_name = path.file_name().unwrap().to_string_lossy();
        path.with_file_name(format!(".{file_name}.previous"))
    }

    fn activity_group(id: &str, updated_at: u64) -> ActivityGroup {
        ActivityGroup {
            id: id.to_string(),
            workspace_id: "workspace-1".to_string(),
            workspace_name: "RepoTunnel".to_string(),
            trace_group_id: None,
            summary: id.to_string(),
            version_ids: Vec::new(),
            events: Vec::new(),
            created_at: updated_at,
            updated_at,
        }
    }

    #[test]
    fn workspace_timeline_reuses_history_per_project_and_skips_other_projects() {
        let mut first = activity_group("first", 4);
        first.trace_group_id = Some("trace-1".into());
        first.events.push(ActivityEvent {
            id: "event-1".into(),
            kind: ActivityKind::Process,
            action: "start".into(),
            summary: "process fixture".into(),
            detail: None,
            status: ActivityStatus::Running,
            source_id: None,
            created_at: 4,
            updated_at: 4,
        });
        let mut second = activity_group("second", 6);
        second.trace_group_id = Some("trace-2".into());
        second.events.push(ActivityEvent {
            id: "event-2".into(),
            kind: ActivityKind::Process,
            action: "start".into(),
            summary: "another fixture".into(),
            detail: None,
            status: ActivityStatus::Running,
            source_id: None,
            created_at: 6,
            updated_at: 6,
        });
        let mut unrelated = activity_group("unrelated", 8);
        unrelated.workspace_id = "other-project".into();
        unrelated.trace_group_id = Some("trace-3".into());
        unrelated.version_ids.push("keep-unrelated".into());

        let mut versions_loaded = 0;
        let mut processes_loaded = 0;
        let mut groups = vec![first, second, unrelated];
        let changed = reconcile_timeline_groups(
            &mut groups,
            Some("workspace-1"),
            |workspace| {
                assert_eq!(workspace, "workspace-1");
                versions_loaded += 1;
                Some(std::collections::HashMap::from([
                    ("trace-1".to_string(), vec!["version-1".to_string()]),
                    ("trace-2".to_string(), vec!["version-2".to_string()]),
                ]))
            },
            |workspace| {
                assert_eq!(workspace, "workspace-1");
                processes_loaded += 1;
                Vec::new()
            },
        );
        assert!(changed);
        assert_eq!(versions_loaded, 1, "version history should be loaded once");
        assert_eq!(processes_loaded, 1, "process history should be loaded once");
        assert_eq!(groups[0].version_ids, vec!["version-1"]);
        assert_eq!(groups[1].version_ids, vec!["version-2"]);
        assert_eq!(groups[2].version_ids, vec!["keep-unrelated"]);
    }

    #[test]
    fn workspace_timeline_does_not_load_unused_process_or_version_history() {
        let mut group = activity_group("read-only", 12);
        group.events.push(ActivityEvent {
            id: "event-read".into(),
            kind: ActivityKind::Files,
            action: "inspect".into(),
            summary: "read-only observation".into(),
            detail: None,
            status: ActivityStatus::Succeeded,
            source_id: None,
            created_at: 12,
            updated_at: 12,
        });
        let mut version_calls = 0;
        let mut process_calls = 0;
        let mut groups = vec![group];
        let changed = reconcile_timeline_groups(
            &mut groups,
            Some("workspace-1"),
            |_| {
                version_calls += 1;
                None
            },
            |_| {
                process_calls += 1;
                Vec::new()
            },
        );
        assert!(!changed);
        assert_eq!(version_calls, 0);
        assert_eq!(process_calls, 0);
    }

    #[test]
    fn activity_history_recovers_previous_good_copy() {
        let root = temp_store("previous-good");
        let path = root.join("activity-history.json");
        let first = vec![activity_group("first", 1)];
        let second = vec![activity_group("second", 2)];

        super::save_path(&path, &first).expect("save first activity history");
        super::save_path(&path, &second).expect("save second activity history");
        fs::write(&path, b"{truncated").expect("corrupt primary");

        let recovered = super::load_path(&path).expect("recover previous-good activity history");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, "first");
        assert!(previous_path(&path).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn activity_history_corrupt_primary_and_previous_still_returns_direct_api_error() {
        let root = temp_store("double-corrupt");
        let path = root.join("activity-history.json");
        fs::write(&path, b"{bad-primary").unwrap();
        fs::write(previous_path(&path), b"{bad-previous").unwrap();

        let error = super::load_path(&path).expect_err("both activity copies must fail");
        assert!(error.contains("AI activity history"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn activity_history_symlink_store_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = temp_store("symlink");
        let path = root.join("activity-history.json");
        let outside = root.join("outside.json");
        fs::write(&outside, b"[]").unwrap();
        symlink(&outside, &path).unwrap();

        let error = super::save_path(&path, &[activity_group("safe", 1)])
            .expect_err("symlink activity store must be rejected");
        assert!(error.contains("symbolic link"));
        let _ = fs::remove_dir_all(root);
    }

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
