use axum::http::{request::Parts, HeaderMap};
use rmcp::{
    handler::server::{tool::Extension, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::Serialize;
use std::{collections::BTreeMap, path::Path};
use tauri::{AppHandle, Manager};

use crate::{
    activity,
    app_state::AppState,
    browser, changes, continuity, desktop_control, execution,
    external_access::{self, ExternalFileAction},
    filesystem, git, integrations, launcher,
    models::{
        ActivityKind, ActivityStatus, BrowserScreenshot, CommandPolicy, TeamMessageKind, TeamPhase,
        TeamTaskStatus, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy,
    },
    monitoring, project_index, project_memory, project_setup, repository, secret_guard,
    storage::load_workspaces,
    team, terminal, workflow,
};

const SERVER_INSTRUCTIONS: &str = "RepoTunnel provides access only to user-approved local workspaces. If the human explicitly asks to create a new project from scratch, use create_project; if the human explicitly gives you a GitHub repository link or owner/repository shorthand that is not local yet, use clone_repository to clone it into the user's Projects folder and approve that checkout automatically. Never create or clone a project the human did not explicitly request. Start with list_workspaces, then call get_resume_snapshot for the chosen workspace. Resume v2 is the authoritative small continuation brief: it derives live Git/activity/process facts automatically and flags older semantic memory when it is stale. Call get_project_memory only when the brief says deeper semantic context is needed, then get_project_setup before get_workflow_readiness so you can detect setup/dev commands without making the human explain them. When get_project_setup reports setupNeeded=true, use its exact setupCommand through RepoTunnel terminal execution when policy allows instead of asking the human to install dependencies manually. Use inspect_project and read/search tools to understand the current code before editing. File tools are strictly workspace-relative: never try to browse host files through terminal commands or guessed absolute paths. If a human-supplied external file is needed, request_external_file opens a native RepoTunnel file picker so the user explicitly chooses what may be read once or imported into the workspace. Prefer patch_file for targeted file changes and respect read-only workspaces. RepoTunnel has two command paths: discovered sandbox presets are disposable/offline verification, while run_terminal_command and managed-process tools operate on the real approved workspace with network access inside RepoTunnel's OS filesystem sandbox. AI terminal/process commands do not receive the normal host environment or general home-directory access; credential-like environment variables are rejected and output is redacted. Narrow GitHub Actions commands may use the authenticated host gh CLI without exposing its credential files. Use start_process for dev servers/watchers and for any build, test, install, conversion, or verification likely to run longer than about 60 seconds; it returns immediately while the job continues independently of the MCP request. Poll long work with read_process_output/list_processes/get_monitoring_snapshot instead of holding one tool call open. For long multi-step work, use project memory only for semantic context that RepoTunnel cannot infer from tools: the human's current goal, important decisions/constraints, and intended next step. Update it at the start of a meaningful new work request and when those semantic facts change. RepoTunnel Continuity records factual edits/tests/process/Git progress automatically, so never copy raw logs or transient tool output into project memory. After any connector reconnect, ChatGPT turn interruption, app restart, or transport interruption, do not restart work from the beginning: call get_resume_snapshot for the active workspace first, then continue from its persisted memory, running-process/output, recent terminal/change/activity, monitoring, and Team state without repeating already-applied mutations. Use launch_target for structured desktop launching. For native desktop-app troubleshooting, prefer AI Workspace when the human wants ChatGPT to work without interrupting their real desktop: use ai_workspace_session action=start with an allowed application, call ai_workspace_inspect before pointer work to get exact isolated window IDs and bounds, use ai_workspace_take_screenshot for visual grounding, and send input with ai_workspace_action. When several consecutive actions are already grounded, prefer ai_workspace_sequence so RepoTunnel can execute them in one bounded request; use its wait steps for short title/window transitions instead of inserting unnecessary screenshots between every action. Keep ai_workspace_action as the reliable single-step fallback. Prefer window_id plus window-relative coordinates over whole-display coordinates; use screenshots to verify meaningful state changes rather than re-guessing geometry after every action. AI Workspace runs one GUI app at a time on a separate virtual display and requires the same locally enabled project-level Desktop permission. Use normal Desktop Control only when interaction with an already-running real desktop app is specifically needed: call list_desktop_applications, inspect_desktop_app before semantic actions, prefer element IDs over coordinate fallback, and use desktop_take_screenshot when visual grounding is necessary. RepoTunnel itself remains excluded and sensitive credential/password typing is blocked. For browser testing, discover an automation browser, start it with browser_action, then navigate/click/type/reload with browser_action and verify with browser_inspect_page, browser_take_screenshot, and get_browser_diagnostics. If the human refers to a visually selected element as “this”, “this button”, “change this”, or similar, call get_visual_selection first and use its selector/text/HTML as the grounded UI target. Project monitoring is read-only observation and can be enabled with set_workspace_monitoring; get_monitoring_snapshot combines processes, terminal output tails, listeners, browser state/errors, and recent file changes. Team Mode lets two MCP-connected AIs coordinate on one project through one persistent A/B team, shared discussion, distinct task ownership, enforced cross-review, dependencies, explicit handoffs, and task-scoped file/folder claims. The A/B identities join once and remain attached until the user explicitly ends the Team in the desktop app. If a team session is active, call team_status with the assigned agent ID and join first. RepoTunnel enforces a coordination barrier: BOTH AIs must be joined before planning begins; each posts one concise plan, each creates one distinct initial implementation task, and both confirm the split before implementation unlocks. Both AIs then code different scopes in parallel, cross-review each other, discuss/fix review findings through the task owner, and verify the result. Never race ahead alone or duplicate the other engineer's implementation. Claim only one active implementation task at a time with its edit paths, and use handoff_task when primary ownership must move. Reviewers inspect/test and send feedback rather than silently editing the owner's task. Normal MCP file mutations require the caller to own an in-progress task and hold a matching task-scoped path claim. Interactive managed-browser mutations use a Team resource lease: claim `@browser` with team_action lock_paths before clicking/typing/navigating, and release it when done so the other engineer cannot collide in the same shared tab. When the human gives either AI new product work after a request is finished, the receiving AI must post a decision message beginning exactly `USER REQUEST:` followed by the human's request; RepoTunnel reopens the same Team for a new work cycle without a new session or kickoff. team_action complete completes only the current work request after cross-review and verification; it does not end the Team. Team pause/end remain user-controlled from the desktop app. In AI Auto, file changes, live terminal commands, managed processes, launcher actions, and browser mutations execute without local approval. In AI Review, mutating actions may return queued=true and wait for local Accept/Reject; MCP cannot approve pending review actions. Before claiming a fix is complete, run appropriate builds/tests and inspect their actual results, including browser diagnostics when UI behavior matters. For Git work, inspect git_status and git_diff before consequential Git actions. Use RepoTunnel Git stage/commit tools instead of raw git add/git commit; the internal secret guard blocks credential-like content before it can be staged or committed. AI Auto is autonomous inside the approved project, but it is not standing permission to push: call a git push terminal command with user_requested_push=true only when the human explicitly asked to push the current work. Never claim an edit, command, process, launch, browser action, test, stage, or commit completed unless the returned state confirms it. For any active multi-step request, do not voluntarily stop midway after partial work: keep using the available RepoTunnel tools until the requested work is completed, blocked on a real human decision, or you have produced the final requested report. In Team Mode, an engineer that finishes its own scope must remain attached, long-poll team_status while waiting when useful, respond to review/verification work, and wait for the teammate rather than treating its turn as Team completion. If any tool reports that AI access is paused, stop immediately; Pause AI is the user's emergency master stop.";

#[derive(Clone)]
pub(crate) struct RepoTunnelMcp {
    app: AppHandle,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceSummary {
    id: String,
    name: String,
    access_mode: WorkspaceAccessMode,
    change_policy: WorkspaceChangePolicy,
    command_policy: CommandPolicy,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CloneRepositoryParams {
    /// GitHub repository explicitly provided by the human, as owner/repository or https://github.com/owner/repository.
    repository: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CreateProjectParams {
    /// New local project name explicitly requested by the human. RepoTunnel creates it inside ~/Projects.
    name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ProjectMemoryUpdateParams {
    /// Approved workspace whose persistent project memory should be updated.
    workspace_id: String,
    /// Concise current project description/context.
    summary: String,
    /// Current user/product goals worth carrying into later AI sessions.
    goals: Vec<String>,
    /// Important architecture/product decisions already made.
    decisions: Vec<String>,
    /// Stable user preferences or constraints for this project.
    preferences: Vec<String>,
    /// Useful unfinished work or next steps.
    next_steps: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ExternalFileActionParam {
    Read,
    Import,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ExternalFileAccessParams {
    /// Approved project that needs the external file.
    workspace_id: String,
    /// read opens a user picker and returns UTF-8 content once; import copies the selected file into destination_path.
    action: ExternalFileActionParam,
    /// Short explanation shown in the native approval picker.
    reason: Option<String>,
    /// Workspace-relative destination required for action=import. RepoTunnel never overwrites an existing entry.
    destination_path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WorkspaceIdParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ProjectSnapshotParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Maximum filtered tree entries to return. Values are clamped to 100..25000.
    entry_limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WorkspacePathParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Path relative to the workspace root. Use an empty string for the workspace root.
    relative_path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchFilesParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// File or folder path relative to the workspace root. Use an empty string to search the whole workspace.
    relative_path: String,
    /// Case-insensitive text to find. The query cannot be empty.
    query: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FileContentParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// File path relative to the workspace root.
    relative_path: String,
    /// Complete UTF-8 text content for the file.
    content: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PatchFileParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Existing file path relative to the workspace root.
    relative_path: String,
    /// Exact text expected to appear exactly once in the current file.
    expected: String,
    /// Text that replaces the expected text. May be empty to remove the expected text.
    replacement: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct CreateDirectoryParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// New folder path relative to the workspace root.
    relative_path: String,
    /// When true, create missing parent folders as needed.
    recursive: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RenameEntryParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Existing file or folder path relative to the workspace root.
    relative_path: String,
    /// New basename only. Do not include path separators.
    new_name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct MoveEntryParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Existing file or folder path relative to the workspace root.
    source_path: String,
    /// New full path relative to the same workspace root.
    destination_path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DeleteEntryParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Existing file or folder path relative to the workspace root. The workspace root itself cannot be deleted.
    relative_path: String,
    /// Required for deleting non-empty folders. Has no effect for files.
    recursive: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WorkspaceCommandParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Exact preset ID returned by list_command_presets. Arbitrary shell commands are not accepted.
    preset_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListCommandsParams {
    /// Optional workspace ID. Omit to list recent command records across approved projects.
    workspace_id: Option<String>,
    /// Maximum records to return. RepoTunnel clamps this to 1..100.
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TerminalCommandParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Arbitrary shell command to run in the real approved workspace.
    command: String,
    /// Optional workspace-relative working directory. Omit or use an empty string for the project root.
    cwd: Option<String>,
    /// Optional timeout in seconds for this one-shot command. RepoTunnel clamps it to 1..43200 (12 hours); persistent processes use start_process and do not inherit this one-shot timeout.
    timeout_seconds: Option<u64>,
    /// Optional environment-variable overrides applied only to this command. Credential-like variable names are rejected for AI commands.
    env: Option<BTreeMap<String, String>>,
    /// Set true only when the human explicitly instructed you to push the current work to GitHub/the configured Git remote. AI Auto does not imply push permission.
    user_requested_push: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ManagedProcessStartParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Arbitrary shell command for the persistent process, such as a development server.
    command: String,
    /// Optional workspace-relative working directory. Omit or use an empty string for the project root.
    cwd: Option<String>,
    /// Optional human-readable label for the process.
    label: Option<String>,
    /// Optional environment-variable overrides applied only to this process.
    env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListProcessesParams {
    /// Optional workspace ID. Omit to inspect managed processes across approved projects.
    workspace_id: Option<String>,
    /// Maximum records to return. RepoTunnel clamps this to 1..100.
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ProcessIdParams {
    /// Managed process ID returned by start_process or list_processes.
    process_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ProcessOutputParams {
    /// Managed process ID returned by start_process or list_processes.
    process_id: String,
    /// Byte offset for incremental stdout reads. Omit for the beginning.
    stdout_offset: Option<u64>,
    /// Byte offset for incremental stderr reads. Omit for the beginning.
    stderr_offset: Option<u64>,
    /// Maximum bytes to return from each stream. RepoTunnel clamps this to 1..65536.
    max_bytes: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct StopProcessParams {
    /// Managed process ID returned by start_process or list_processes.
    process_id: String,
    /// When true, stop immediately; otherwise RepoTunnel first attempts a graceful process-group stop.
    force: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum LaunchTargetKindParam {
    Url,
    WorkspacePath,
    Application,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct LaunchTargetParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Target type: url, workspace_path, or application.
    kind: LaunchTargetKindParam,
    /// URL, workspace-relative path, or application ID depending on kind. Use an empty string for the project root when kind=workspace_path.
    target: String,
    /// Optional application ID returned by list_launchable_applications when opening a URL or workspace path with a specific app. Omit to use the desktop default.
    application_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct IntegrationActionParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// One of: android-studio, unity, blender, godot, docker.
    integration_id: String,
    /// Exact allowlisted action returned by list_deep_integrations for this integration.
    action: String,
    /// Optional workspace-relative target. Android Studio accepts a project folder; Blender run_script/render accepts the required file.
    target: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AiWorkspaceSessionParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// One of status, start, or stop.
    action: String,
    /// Application ID returned by list_launchable_applications. Required only for action=start.
    application_id: Option<String>,
    /// Optional workspace-relative project file or folder opened inside the isolated app session. Productivity files can be opened directly in a detected Word/Excel/PowerPoint, Writer/Calc/Impress, or Pages/Numbers/Keynote app.
    target: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AiWorkspaceFrameParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Maximum returned image width. RepoTunnel clamps this to the virtual screen width.
    max_width: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AiWorkspaceInspectParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AiWorkspaceActionParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// One of activate, click, key, type, or scroll.
    action: String,
    /// Optional AI Workspace window ID returned by ai_workspace_inspect. When supplied, click/scroll coordinates are relative to that exact isolated window.
    window_id: Option<String>,
    /// Horizontal position from 0..1 for click/scroll inside the isolated virtual display or selected window.
    x_ratio: Option<f64>,
    /// Vertical position from 0..1 for click/scroll inside the isolated virtual display.
    y_ratio: Option<f64>,
    /// Click count 1..3 for action=click.
    click_count: Option<u8>,
    /// Safe shortcut for action=key, such as Ctrl+S, Enter, Escape, or F5.
    shortcut: Option<String>,
    /// Text for action=type. Credential/authentication windows are blocked.
    text: Option<String>,
    /// Horizontal scroll delta.
    delta_x: Option<i32>,
    /// Vertical scroll delta.
    delta_y: Option<i32>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct AiWorkspaceSequenceStep {
    /// One of activate, click, key, type, scroll, or wait.
    operation: String,
    /// Optional per-step window override. Omit to inherit the sequence window_id.
    window_id: Option<String>,
    /// Horizontal position from 0..1 for click/scroll.
    x_ratio: Option<f64>,
    /// Vertical position from 0..1 for click/scroll.
    y_ratio: Option<f64>,
    /// Click count 1..3 for click.
    count: Option<u8>,
    /// Safe shortcut for key.
    shortcut: Option<String>,
    /// Text for type. Text is never echoed in completed sequence results.
    text: Option<String>,
    /// Horizontal scroll delta.
    delta_x: Option<i32>,
    /// Vertical scroll delta.
    delta_y: Option<i32>,
    /// Optional bounded delay for wait, clamped to 0..2000 ms.
    wait_ms: Option<u64>,
    /// Optional wait-condition timeout, clamped to 0..5000 ms.
    timeout_ms: Option<u64>,
    /// Wait until the active isolated window title contains this text.
    title_contains: Option<String>,
    /// Wait until at least this many isolated windows exist.
    window_count_at_least: Option<usize>,
    /// Wait until no more than this many isolated windows exist.
    window_count_at_most: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AiWorkspaceSequenceParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Optional default isolated window ID inherited by steps that omit windowId.
    window_id: Option<String>,
    /// Ordered fast-path actions. RepoTunnel accepts 1..64 bounded steps per request.
    steps: Vec<AiWorkspaceSequenceStep>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DesktopInspectParams {
    /// Approved project whose local desktop permission grants apply.
    workspace_id: String,
    /// Running desktop application ID returned by list_desktop_applications.
    application_id: String,
    /// Maximum semantic UI elements to return. RepoTunnel clamps this to 20..800.
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DesktopActionParams {
    /// Approved project whose local desktop permission grants apply.
    workspace_id: String,
    /// Enabled application ID returned by list_desktop_applications.
    application_id: String,
    /// One of activate, click, type, key, or scroll.
    action: String,
    /// Semantic element ID returned by inspect_desktop_app. Required for type; preferred for click.
    element_id: Option<String>,
    /// Optional app-owned window ID returned by inspect_desktop_app.
    window_id: Option<String>,
    /// Text for action=type. Password/credential fields are blocked by RepoTunnel.
    text: Option<String>,
    /// Replace existing field contents for action=type. Defaults to false.
    clear_first: Option<bool>,
    /// Safe keyboard shortcut for action=key, such as Ctrl+S, Escape, Enter, or F5.
    shortcut: Option<String>,
    /// Window-relative horizontal ratio 0..1 for screenshot-grounded fallback click/scroll.
    x_ratio: Option<f64>,
    /// Window-relative vertical ratio 0..1 for screenshot-grounded fallback click/scroll.
    y_ratio: Option<f64>,
    /// Horizontal wheel delta for action=scroll.
    delta_x: Option<i32>,
    /// Vertical wheel delta for action=scroll.
    delta_y: Option<i32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct DesktopScreenshotParams {
    /// Approved project whose local desktop permission grants apply.
    workspace_id: String,
    /// Enabled application ID returned by list_desktop_applications.
    application_id: String,
    /// Optional app-owned window ID; omit to capture the first current window.
    window_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListActivityParams {
    /// Optional workspace ID. Omit to list recent records across approved projects.
    workspace_id: Option<String>,
    /// Maximum records to return. RepoTunnel clamps this to 1..100.
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BrowserActionParam {
    Start,
    Stop,
    OpenTab,
    ActivateTab,
    CloseTab,
    Navigate,
    Click,
    Type,
    Scroll,
    Reload,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BrowserActionParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Browser mutation to perform. Fields not used by the selected action are ignored.
    action: BrowserActionParam,
    /// Browser application ID returned by list_automation_browsers. Required for action=start.
    application_id: Option<String>,
    /// Browser tab ID returned by list_browser_tabs. Required for activate_tab, close_tab, navigate, click, type, scroll, and reload.
    tab_id: Option<String>,
    /// HTTP/HTTPS URL. Required for open_tab and navigate.
    url: Option<String>,
    /// CSS selector. Required for click and type.
    selector: Option<String>,
    /// Text to enter. Required for type. Completed browser history records do not retain this text.
    text: Option<String>,
    /// For type, clear the target field before entering text. Defaults to false.
    clear_first: Option<bool>,
    /// Horizontal scroll delta in CSS pixels. Used only by scroll. Defaults to 0.
    delta_x: Option<i32>,
    /// Vertical scroll delta in CSS pixels. Used only by scroll. Defaults to 600 when both deltas are omitted.
    delta_y: Option<i32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BrowserInspectParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Browser tab ID returned by list_browser_tabs.
    tab_id: String,
    /// Optional CSS selector. Omit to inspect the page document/body.
    selector: Option<String>,
    /// Maximum text/HTML characters to return. RepoTunnel clamps this internally.
    max_chars: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BrowserScreenshotParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Browser tab ID returned by list_browser_tabs.
    tab_id: String,
    /// True for a full-page capture, false for the current viewport. Defaults to false.
    full_page: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BrowserDiagnosticsParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// Optional tab ID. Omit to return diagnostics across the managed browser session.
    tab_id: Option<String>,
    /// Maximum console entries and network failures to return per category. RepoTunnel clamps this internally.
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct WorkspaceMonitoringParams {
    /// ID returned by list_workspaces for the approved project.
    workspace_id: String,
    /// True to persistently enable project-file monitoring, false to disable it. Monitoring is observational and does not edit project files.
    enabled: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct MonitoringFileEventsParams {
    /// Optional workspace ID. Omit to list recent monitoring file events across approved projects.
    workspace_id: Option<String>,
    /// Maximum events to return. RepoTunnel clamps this to 1..200.
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListChangesParams {
    /// Optional workspace ID. Omit to list recent changes across approved projects.
    workspace_id: Option<String>,
    /// Maximum records to return. RepoTunnel clamps this to 1..100.
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TeamStatusParams {
    /// Team session ID returned by team_action action=create_session or the RepoTunnel Team UI. Omit only when workspace_id is supplied to load the latest session for that project.
    session_id: Option<String>,
    /// Approved workspace ID. Used to find the latest Team Mode session when session_id is omitted.
    workspace_id: Option<String>,
    /// Optional assigned Team Mode agent ID. Supplying it makes the snapshot include a role-specific recommended next action.
    agent_id: Option<String>,
    /// Optional revision already seen by the caller. With wait_seconds, team_status waits until the session revision becomes newer or the wait expires.
    after_revision: Option<u64>,
    /// Optional long-poll wait in seconds, clamped to 0..30. Useful while an active agent is waiting for the other AI to post a handoff/review/update without ending its collaboration loop.
    wait_seconds: Option<u64>,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TeamActionParam {
    CreateSession,
    Join,
    Heartbeat,
    PostMessage,
    CreateTask,
    ClaimTask,
    HandoffTask,
    UpdateTask,
    VerifyCriterion,
    LockPaths,
    ReleasePaths,
    SetPhase,
    Complete,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TeamMessageKindParam {
    Plan,
    Progress,
    Question,
    Review,
    Decision,
    Handoff,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TeamTaskStatusParam {
    Todo,
    InProgress,
    Review,
    Blocked,
    Done,
    Cancelled,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TeamPhaseParam {
    Planning,
    Executing,
    Reviewing,
    Verifying,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TeamActionParams {
    /// Coordination mutation to perform. Fields unrelated to the selected action are ignored.
    action: TeamActionParam,
    /// Approved workspace ID. Required only for create_session.
    workspace_id: Option<String>,
    /// Team session ID. Required for every action except create_session.
    session_id: Option<String>,
    /// Assigned Team Mode agent ID. Required for agent-originated actions such as join, heartbeat, messages, tasks, claims, locks, and phase changes.
    agent_id: Option<String>,
    /// Optional label describing the connected client/model, used only when joining (for example "ChatGPT" or "Gemini CLI").
    client_label: Option<String>,
    /// High-level project goal. Required for create_session.
    goal: Option<String>,
    /// Explicit completion criteria. Required for create_session; agents must verify these before completing the session.
    success_criteria: Option<Vec<String>>,
    /// Display name for agent A when creating a session.
    agent_a_name: Option<String>,
    /// Persistent role/instructions for agent A when creating a session.
    agent_a_role: Option<String>,
    /// Display name for agent B when creating a session.
    agent_b_name: Option<String>,
    /// Persistent role/instructions for agent B when creating a session.
    agent_b_role: Option<String>,
    /// Message category for post_message.
    message_kind: Option<TeamMessageKindParam>,
    /// Shared-board message text for post_message, or optional ownership-transfer context for handoff_task.
    message: Option<String>,
    /// Existing task ID for claim_task/handoff_task/update_task, or optional task association for post_message/lock_paths.
    task_id: Option<String>,
    /// Other joined agent ID that receives primary ownership for handoff_task.
    target_agent_id: Option<String>,
    /// Task title for create_task.
    title: Option<String>,
    /// Task description for create_task.
    description: Option<String>,
    /// Task priority from 1 (low) to 5 (high). Defaults to 3.
    priority: Option<u8>,
    /// Task IDs that must be done before a new task can be claimed.
    depends_on: Option<Vec<String>>,
    /// New task state for update_task.
    task_status: Option<TeamTaskStatusParam>,
    /// Implementation/review result attached to update_task.
    result: Option<String>,
    /// Zero-based success-criterion index for verify_criterion.
    criterion_index: Option<usize>,
    /// Concrete build/test/browser/manual evidence proving the selected success criterion for verify_criterion.
    evidence: Option<String>,
    /// Explanation attached when task_status=blocked.
    blocked_reason: Option<String>,
    /// Optional other-agent ID assigned as reviewer when moving a task to review.
    reviewer_agent_id: Option<String>,
    /// Workspace-relative files/folders to claim/release. claim_task requires at least one path. lock_paths requires task_id and only extends claims for the caller's owned in-progress task.
    paths: Option<Vec<String>>,
    /// Lock lifetime in seconds. RepoTunnel clamps this to 30..3600 seconds.
    lock_ttl_seconds: Option<u64>,
    /// New collaboration phase for set_phase.
    phase: Option<TeamPhaseParam>,
    /// Required evidence-oriented summary for complete.
    completion_summary: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GitDiffParams {
    /// ID returned by list_workspaces for the approved Git repository.
    workspace_id: String,
    /// True for the staged/index diff; false for unstaged working-tree changes.
    staged: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GitLogParams {
    /// ID returned by list_workspaces for the approved Git repository.
    workspace_id: String,
    /// Maximum commits to return. RepoTunnel clamps this to 1..50.
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GitStageParams {
    /// ID returned by list_workspaces for the approved Git repository.
    workspace_id: String,
    /// Exact workspace-relative file paths to stage. RepoTunnel accepts 1..100 paths.
    paths: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GitCommitParams {
    /// ID returned by list_workspaces for the approved Git repository.
    workspace_id: String,
    /// Commit message. RepoTunnel commits currently staged changes only.
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GitRestoreParams {
    /// ID returned by list_workspaces for the approved Git repository.
    workspace_id: String,
    /// Tracked UTF-8 text file path relative to the workspace root.
    relative_path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ListGitActionsParams {
    /// Optional workspace ID. Omit to list recent Git action requests across approved projects.
    workspace_id: Option<String>,
    /// Maximum records to return. RepoTunnel clamps this to 1..100.
    limit: Option<usize>,
}

fn valid_trace_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn trace_edit_group_id(headers: &HeaderMap) -> Option<String> {
    let traceparent = headers.get("traceparent")?.to_str().ok()?.trim();
    let mut parts = traceparent.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let parent_id = parts.next()?;
    let flags = parts.next()?;

    if parts.next().is_some()
        || !valid_trace_hex(version, 2)
        || version.eq_ignore_ascii_case("ff")
        || !valid_trace_hex(trace_id, 32)
        || trace_id.bytes().all(|byte| byte == b'0')
        || !valid_trace_hex(parent_id, 16)
        || parent_id.bytes().all(|byte| byte == b'0')
        || !valid_trace_hex(flags, 2)
    {
        return None;
    }

    Some(format!("trace-{trace_id}"))
}

fn request_edit_group_id(parts: &Parts) -> Option<String> {
    trace_edit_group_id(&parts.headers)
}

fn request_client_key(parts: &Parts) -> Option<String> {
    if let Some(session_id) = parts
        .headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let bounded = session_id.chars().take(220).collect::<String>();
        return Some(format!("mcp-session:{bounded}"));
    }
    request_edit_group_id(parts).map(|trace| format!("request:{trace}"))
}

fn record_observation(
    app: &AppHandle,
    workspace: &Workspace,
    trace_group_id: Option<&str>,
    kind: ActivityKind,
    action: &str,
    summary: impl Into<String>,
    detail: Option<String>,
) {
    let _ = activity::record(
        app,
        workspace,
        trace_group_id,
        kind,
        action,
        summary.into(),
        detail,
        ActivityStatus::Observed,
        None,
    );
}

fn monitoring_file_detail(events: &[crate::models::MonitoringFileEvent]) -> Option<String> {
    if events.is_empty() {
        return None;
    }
    let mut lines = events
        .iter()
        .take(20)
        .map(|event| format!("{:?}: {}", event.kind, event.path))
        .collect::<Vec<_>>();
    if events.len() > lines.len() {
        lines.push(format!("…and {} more", events.len() - lines.len()));
    }
    Some(lines.join("\n"))
}

fn ensure_ai_access(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    if state.ai_access_paused() {
        return Err("AI access is paused in RepoTunnel. The user must resume AI access locally before MCP tools can access project information or perform project actions.".to_string());
    }
    Ok(())
}

fn approved_workspace(app: &AppHandle, workspace_id: &str) -> Result<Workspace, String> {
    ensure_ai_access(app)?;

    load_workspaces(app)?
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
        .ok_or_else(|| "That project is not approved in RepoTunnel.".to_string())
}

fn success_result<T: Serialize>(value: T) -> CallToolResult {
    match serde_json::to_string(&serde_json::json!({
        "ok": true,
        "result": value,
    })) {
        Ok(content) => CallToolResult::success(vec![ContentBlock::text(content)]),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(format!(
            "RepoTunnel could not serialize the tool result: {error}"
        ))]),
    }
}

fn error_result(message: impl Into<String>) -> CallToolResult {
    let message = message.into();
    let content = serde_json::to_string(&serde_json::json!({
        "ok": false,
        "error": message,
    }))
    .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"RepoTunnel operation failed.\"}".to_string());

    CallToolResult::error(vec![ContentBlock::text(content)])
}

async fn run_filesystem_task<T, F>(task: F) -> Result<CallToolResult, McpError>
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    Ok(match tokio::task::spawn_blocking(task).await {
        Ok(Ok(value)) => success_result(value),
        Ok(Err(message)) => error_result(message),
        Err(error) => error_result(format!(
            "The local filesystem task could not complete: {error}"
        )),
    })
}

async fn run_browser_screenshot_task<F>(task: F) -> Result<CallToolResult, McpError>
where
    F: FnOnce() -> Result<BrowserScreenshot, String> + Send + 'static,
{
    const MAX_MCP_SCREENSHOT_BYTES: u64 = 8 * 1024 * 1024;
    Ok(match tokio::task::spawn_blocking(task).await {
        Ok(Ok(screenshot)) if screenshot.size_bytes <= MAX_MCP_SCREENSHOT_BYTES => {
            let metadata = serde_json::json!({
                "ok": true,
                "result": {
                    "id": screenshot.id,
                    "tabId": screenshot.tab_id,
                    "createdAt": screenshot.created_at,
                    "mimeType": screenshot.mime_type.clone(),
                    "sizeBytes": screenshot.size_bytes,
                    "fullPage": screenshot.full_page,
                }
            });
            let text = serde_json::to_string(&metadata)
                .unwrap_or_else(|_| "{\"ok\":true}".to_string());
            CallToolResult::success(vec![
                ContentBlock::text(text),
                ContentBlock::image(screenshot.data_base64, screenshot.mime_type),
            ])
        }
        Ok(Ok(screenshot)) => error_result(format!(
            "The screenshot is {} bytes, which exceeds RepoTunnel's 8 MiB MCP image limit. Retry with full_page=false or inspect the page DOM instead.",
            screenshot.size_bytes
        )),
        Ok(Err(message)) => error_result(message),
        Err(error) => error_result(format!("The browser screenshot task could not complete: {error}")),
    })
}

fn required_text(value: Option<String>, field: &str, action: &str) -> Result<String, String> {
    let value = value.unwrap_or_default();
    if value.trim().is_empty() {
        Err(format!("{field} is required for browser action {action}."))
    } else {
        Ok(value)
    }
}

impl RepoTunnelMcp {
    pub(crate) fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[tool_router]
impl RepoTunnelMcp {
    #[tool(
        description = "Create a new empty local project only when the human explicitly asks to create a project from scratch. RepoTunnel creates a new folder inside ~/Projects, refuses to overwrite an existing folder, and immediately registers it as an approved workspace so normal file tools can build the project from chat."
    )]
    async fn create_project(
        &self,
        Parameters(params): Parameters<CreateProjectParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            repository::create_and_register(&app, &params.name)
        })
        .await
    }

    #[tool(
        description = "Detect the approved project's framework, package manager, dependency readiness, safe setup command when preparation is needed, and likely dev command/URL. Use this instead of asking the human which package manager or dev command a project uses.",
        annotations(read_only_hint = true)
    )]
    async fn get_project_setup(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            project_setup::detect(&workspace)
        })
        .await
    }

    #[tool(
        description = "Read RepoTunnel's persistent project memory for an approved project. It contains concise project context, goals, important decisions, user preferences/constraints, and next steps stored outside the repository so a later AI session can resume without rediscovering everything.",
        annotations(read_only_hint = true)
    )]
    async fn get_project_memory(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            project_memory::get(&app, &workspace)
        })
        .await
    }

    #[tool(
        description = "Get RepoTunnel Continuity Resume v2 after a ChatGPT turn limit, connector reconnect, app restart, or other interruption. It returns a small authoritative brief built from live Git/activity/process state, a bounded semantic-context preview, and compact durable milestones. Factual state always wins over stale saved memory; use get_project_memory only when deeper context is actually needed.",
        annotations(read_only_hint = true)
    )]
    async fn get_resume_snapshot(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            continuity::resume_snapshot(&app, &workspace)
        })
        .await
    }

    #[tool(
        description = "Update RepoTunnel's persistent project memory for the approved project after meaningful decisions/progress. Keep it concise and factual; do not store secrets, credentials, raw logs, or temporary chatter. This memory lives in RepoTunnel app data, not in the user's repository.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn update_project_memory(
        &self,
        Parameters(params): Parameters<ProjectMemoryUpdateParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            project_memory::update(
                &app,
                &workspace,
                params.summary,
                params.goals,
                params.decisions,
                params.preferences,
                params.next_steps,
            )
        })
        .await
    }

    #[tool(
        description = "Clone a GitHub repository explicitly supplied by the human and register the checkout as an approved RepoTunnel workspace. Accepts owner/repository or an HTTPS github.com repository URL. Clones into ~/Projects using the machine's existing Git/GitHub authentication, never stores credentials, never overwrites an existing unrelated folder, and works for both normal single-AI use and Team Mode bootstrap."
    )]
    async fn clone_repository(
        &self,
        Parameters(params): Parameters<CloneRepositoryParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            repository::clone_and_register(&app, &params.repository)
        })
        .await
    }

    #[tool(
        description = "Request access to one file outside the approved workspace without exposing the rest of the host filesystem. RepoTunnel always opens a native local file picker: the user must explicitly select the file, and cancelling denies access. action=read returns bounded UTF-8 content once without revealing the absolute host path. action=import copies the selected regular file to an explicit workspace-relative destination after secret scanning; the AI can then work on it using normal workspace tools. Sensitive credential files and symlinks remain blocked. Works in normal mode and Team Mode."
    )]
    async fn request_external_file(
        &self,
        Parameters(params): Parameters<ExternalFileAccessParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let action = match params.action {
                ExternalFileActionParam::Read => ExternalFileAction::Read,
                ExternalFileActionParam::Import => {
                    let destination = params.destination_path.as_deref().ok_or_else(|| {
                        "destination_path is required for action=import.".to_string()
                    })?;
                    team::assert_paths_available(
                        &app,
                        &workspace.id,
                        &[destination.to_string()],
                        client_key.as_deref(),
                    )?;
                    ExternalFileAction::Import
                }
            };
            let result = external_access::request_file(
                &app,
                &workspace,
                action,
                params.reason.as_deref(),
                params.destination_path.as_deref(),
            )?;
            let status = if result.approved {
                ActivityStatus::Succeeded
            } else {
                ActivityStatus::Rejected
            };
            let summary = if result.approved {
                match action {
                    ExternalFileAction::Read => {
                        "User approved one-time external file reading".to_string()
                    }
                    ExternalFileAction::Import => format!(
                        "User approved external file import to {}",
                        result.imported_path.as_deref().unwrap_or("project")
                    ),
                }
            } else {
                "User cancelled external file access".to_string()
            };
            let _ = activity::record(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Files,
                "externalFileAccess",
                summary,
                result.source_name.clone(),
                status,
                None,
            );
            Ok(result)
        })
        .await
    }

    #[tool(
        description = "List the local projects the user has explicitly approved in RepoTunnel. Use this first when you need a workspace ID. Returns project names, IDs, access modes, write policies, and command policies, but does not expose absolute local paths.",
        annotations(read_only_hint = true)
    )]
    async fn list_workspaces(&self) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let summaries = load_workspaces(&app)?
                .into_iter()
                .map(|workspace| WorkspaceSummary {
                    id: workspace.id,
                    name: workspace.name,
                    access_mode: workspace.access_mode,
                    change_policy: workspace.change_policy,
                    command_policy: workspace.command_policy,
                })
                .collect::<Vec<_>>();
            Ok(summaries)
        })
        .await
    }

    #[tool(
        description = "Inspect an approved codebase using RepoTunnel's smart project index. Returns a filtered project tree plus file counts, detected languages, common manifests, binary/large-file counts, and ignore statistics. Respects .gitignore/.ignore rules and skips generated dependency/build folders.",
        annotations(read_only_hint = true)
    )]
    async fn inspect_project(
        &self,
        Parameters(params): Parameters<ProjectSnapshotParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let snapshot =
                project_index::project_snapshot(&workspace, params.entry_limit.unwrap_or(600))?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Files,
                "inspectProject",
                format!(
                    "Inspected project · {} indexed entries",
                    snapshot.entries.len()
                ),
                None,
            );
            Ok(snapshot)
        })
        .await
    }

    #[tool(
        description = "Preflight the complete AI development workflow for an approved project. Reports whether project inspection, safe editing, sandboxed verification, and Git completion are currently available and explains any limitations. Call this before starting a multi-step bug fix or feature task.",
        annotations(read_only_hint = true)
    )]
    async fn get_workflow_readiness(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            Ok(workflow::readiness(&workspace))
        })
        .await
    }

    #[tool(
        description = "List relevant files and folders at a workspace-relative path. RepoTunnel omits protected secrets, ignored entries, and generated dependency/build folders from directory discovery.",
        annotations(read_only_hint = true)
    )]
    async fn list_directory(
        &self,
        Parameters(params): Parameters<WorkspacePathParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let entries = filesystem::list_directory(&workspace, &params.relative_path)?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Files,
                "listDirectory",
                format!(
                    "Listed {} · {} entries",
                    if params.relative_path.is_empty() {
                        "."
                    } else {
                        &params.relative_path
                    },
                    entries.len()
                ),
                None,
            );
            Ok(entries)
        })
        .await
    }

    #[tool(
        description = "Read an existing UTF-8 text file inside an approved workspace. Use this before editing a file so changes are based on current content.",
        annotations(read_only_hint = true)
    )]
    async fn read_file(
        &self,
        Parameters(params): Parameters<WorkspacePathParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let file = filesystem::read_file(&workspace, &params.relative_path)?;
            if let Some(kind) = secret_guard::detect_secret(file.content.as_bytes()) {
                return Err(format!(
                    "RepoTunnel withheld '{}' because its text appears to contain {kind}. Secrets are never returned to an AI through MCP; remove or replace the credential before asking the AI to edit this file.",
                    params.relative_path
                ));
            }
            record_observation(
                &app, &workspace, trace_group_id.as_deref(), ActivityKind::Files, "readFile",
                format!("Read {}", params.relative_path), Some(format!("{} bytes", file.size)),
            );
            Ok(file)
        })
        .await
    }

    #[tool(
        description = "Search accessible UTF-8 project text for a case-insensitive query, returning bounded path/line/column previews.",
        annotations(read_only_hint = true)
    )]
    async fn search_files(
        &self,
        Parameters(params): Parameters<SearchFilesParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let mut matches =
                filesystem::search_files(&workspace, &params.relative_path, &params.query)?;
            for item in &mut matches {
                item.preview = secret_guard::redact_text(&item.preview);
            }
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Files,
                "searchFiles",
                format!(
                    "Searched for ‘{}’ · {} matches",
                    params.query,
                    matches.len()
                ),
                Some(format!(
                    "Scope: {}",
                    if params.relative_path.is_empty() {
                        "."
                    } else {
                        &params.relative_path
                    }
                )),
            );
            Ok(matches)
        })
        .await
    }

    #[tool(
        description = "Create a new UTF-8 text file. In review mode this queues a diff for local approval; in automatic mode it applies immediately with history and an undo point. Check applied and queued in the result: Review mode returns applied=false, queued=true until the user acts locally."
    )]
    async fn create_file(
        &self,
        Parameters(params): Parameters<FileContentParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let edit_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            team::assert_paths_available(
                &app,
                &workspace.id,
                std::slice::from_ref(&params.relative_path),
                client_key.as_deref(),
            )?;
            let outcome = changes::create_file(
                &app,
                &workspace,
                params.relative_path,
                params.content,
                edit_group_id.as_deref(),
            )?;
            let _ = activity::record_change_outcome(
                &app,
                &workspace,
                edit_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "Replace the complete contents of an existing UTF-8 text file. Prefer patch_file for targeted edits. Review-mode writes are queued locally; automatic-mode writes are backed up and applied immediately. Check applied and queued in the result: Review mode returns applied=false, queued=true until the user acts locally."
    )]
    async fn write_file(
        &self,
        Parameters(params): Parameters<FileContentParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let edit_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            team::assert_paths_available(
                &app,
                &workspace.id,
                std::slice::from_ref(&params.relative_path),
                client_key.as_deref(),
            )?;
            let outcome = changes::write_file(
                &app,
                &workspace,
                params.relative_path,
                params.content,
                edit_group_id.as_deref(),
            )?;
            let _ = activity::record_change_outcome(
                &app,
                &workspace,
                edit_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "Apply a targeted exact-context edit. This is the preferred code-edit tool. The preview is queued for local approval in review mode or applied with backup/history in automatic mode. Check applied in the result."
    )]
    async fn patch_file(
        &self,
        Parameters(params): Parameters<PatchFileParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let edit_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            team::assert_paths_available(
                &app,
                &workspace.id,
                std::slice::from_ref(&params.relative_path),
                client_key.as_deref(),
            )?;
            let outcome = changes::patch_file(
                &app,
                &workspace,
                params.relative_path,
                params.expected,
                params.replacement,
                edit_group_id.as_deref(),
            )?;
            let _ = activity::record_change_outcome(
                &app,
                &workspace,
                edit_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "Create a new folder inside an approved workspace. Review-mode changes require local approval; automatic-mode changes are recorded and applied immediately."
    )]
    async fn create_directory(
        &self,
        Parameters(params): Parameters<CreateDirectoryParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let edit_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            team::assert_paths_available(
                &app,
                &workspace.id,
                std::slice::from_ref(&params.relative_path),
                client_key.as_deref(),
            )?;
            let outcome = changes::create_directory(
                &app,
                &workspace,
                params.relative_path,
                params.recursive,
                edit_group_id.as_deref(),
            )?;
            let _ = activity::record_change_outcome(
                &app,
                &workspace,
                edit_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "Rename an existing file or folder within its current parent folder. This goes through RepoTunnel change review/history and never overwrites an existing destination."
    )]
    async fn rename_entry(
        &self,
        Parameters(params): Parameters<RenameEntryParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let edit_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let destination = Path::new(&params.relative_path)
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(&params.new_name)
                .to_string_lossy()
                .replace('\\', "/");
            team::assert_paths_available(
                &app,
                &workspace.id,
                &[params.relative_path.clone(), destination],
                client_key.as_deref(),
            )?;
            let outcome = changes::rename_entry(
                &app,
                &workspace,
                params.relative_path,
                params.new_name,
                edit_group_id.as_deref(),
            )?;
            let _ = activity::record_change_outcome(
                &app,
                &workspace,
                edit_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "Move an existing file or folder to another relative path in the same approved workspace. This goes through RepoTunnel review/history and never overwrites an existing destination."
    )]
    async fn move_entry(
        &self,
        Parameters(params): Parameters<MoveEntryParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let edit_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            team::assert_paths_available(
                &app,
                &workspace.id,
                &[params.source_path.clone(), params.destination_path.clone()],
                client_key.as_deref(),
            )?;
            let outcome = changes::move_entry(
                &app,
                &workspace,
                params.source_path,
                params.destination_path,
                edit_group_id.as_deref(),
            )?;
            let _ = activity::record_change_outcome(
                &app,
                &workspace,
                edit_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "DESTRUCTIVE: Request deletion of an existing file or folder. File deletions can receive an undo point; recursive directory deletions are recorded but may not be safely undoable. In review mode deletion waits for local approval."
    )]
    async fn delete_entry(
        &self,
        Parameters(params): Parameters<DeleteEntryParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let edit_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            team::assert_paths_available(
                &app,
                &workspace.id,
                std::slice::from_ref(&params.relative_path),
                client_key.as_deref(),
            )?;
            let outcome = changes::delete_entry(
                &app,
                &workspace,
                params.relative_path,
                params.recursive,
                edit_group_id.as_deref(),
            )?;
            let _ = activity::record_change_outcome(
                &app,
                &workspace,
                edit_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "Inspect metadata for an accessible workspace-relative file or folder. This does not modify the project.",
        annotations(read_only_hint = true)
    )]
    async fn get_file_info(
        &self,
        Parameters(params): Parameters<WorkspacePathParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let info = filesystem::file_info(&workspace, &params.relative_path)?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Files,
                "fileInfo",
                format!("Inspected {}", params.relative_path),
                None,
            );
            Ok(info)
        })
        .await
    }

    #[tool(
        description = "Report whether RepoTunnel's native OS command sandbox is available. AI command execution is refused when the required platform sandbox is unavailable.",
        annotations(read_only_hint = true)
    )]
    async fn get_execution_status(&self) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            Ok(execution::execution_status())
        })
        .await
    }

    #[tool(
        description = "List the safe build/test/check/lint command presets RepoTunnel discovered for an approved project. Only these preset IDs can be requested; there is no generic shell or arbitrary command string.",
        annotations(read_only_hint = true)
    )]
    async fn list_command_presets(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            execution::list_presets(&workspace)
        })
        .await
    }

    #[tool(
        description = "Request execution of one exact command preset returned by list_command_presets. Commands run with network disabled in a disposable project copy inside RepoTunnel's native OS sandbox, so command side effects are discarded. Depending on the project's command policy, the request either queues for local approval, runs automatically, or is blocked."
    )]
    async fn run_command(
        &self,
        Parameters(params): Parameters<WorkspaceCommandParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let outcome = execution::request_command(&app, &workspace, &params.preset_id)?;
            let _ = activity::record_sandbox_command(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "List recent sandboxed command records and their pending/running/completed/failed/rejected/timed-out status. This tool cannot approve or reject a pending command.",
        annotations(read_only_hint = true)
    )]
    async fn list_command_history(
        &self,
        Parameters(params): Parameters<ListCommandsParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            execution::list_history(
                &app,
                params.workspace_id.as_deref(),
                params.limit.unwrap_or(30),
            )
        })
        .await
    }

    #[tool(
        description = "Run a short one-shot shell command with write access to the approved workspace and network access, but without general access to the user's home directory or host filesystem. RepoTunnel uses an OS sandbox and a sanitized environment for AI commands, redacts credential-like output, and refuses to fall back to unrestricted host access if the sandbox is unavailable. Safe GitHub Actions inspection commands are narrowly passed through to the authenticated gh CLI. Git push is allowed only when user_requested_push=true AND the human explicitly requested the current work be pushed; AI Auto removes approval popups but never grants standing push permission. In AI Review the command may queue for local Accept/Reject. For dev servers/watchers and for any build/test/install/verification likely to exceed about 60 seconds, use start_process instead so the MCP request returns immediately; then poll with read_process_output/list_processes. This prevents client/request timeouts from interrupting long work."
    )]
    async fn run_terminal_command(
        &self,
        Parameters(params): Parameters<TerminalCommandParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            git::validate_ai_terminal_git_command(
                &workspace,
                &params.command,
                params.user_requested_push.unwrap_or(false),
            )?;
            let outcome = terminal::request_terminal_command(
                &app,
                &workspace,
                params.command,
                params.cwd,
                params.timeout_seconds,
                params.env.unwrap_or_default(),
                true,
                params.user_requested_push.unwrap_or(false),
            )?;
            let _ = activity::record_terminal_outcome(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "List recent real-workspace terminal commands, including pending review requests and final exit/output status. Use this to verify a queued AI Review command after the user acts on it locally.",
        annotations(read_only_hint = true)
    )]
    async fn list_terminal_history(
        &self,
        Parameters(params): Parameters<ListCommandsParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            terminal::list_terminal_history(
                &app,
                params.workspace_id.as_deref(),
                params.limit.unwrap_or(30),
            )
        })
        .await
    }

    #[tool(
        description = "Start a persistent process inside the approved workspace security sandbox, suitable for development servers, watchers, and workers. The AI process can write the project and use the network but cannot browse the user's home directory or host filesystem; credential-like environment overrides are rejected and returned output is redacted. In AI Auto it starts immediately. In AI Review it may queue for local Accept/Reject."
    )]
    async fn start_process(
        &self,
        Parameters(params): Parameters<ManagedProcessStartParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            git::validate_ai_terminal_git_command(&workspace, &params.command, false)?;
            let outcome = terminal::request_process_start(
                &app,
                &workspace,
                params.command,
                params.cwd,
                params.label,
                params.env.unwrap_or_default(),
                true,
            )?;
            let _ = activity::record_process_outcome(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "List RepoTunnel-managed persistent processes with running/exited/stopped/failed state, PID when attached, exit status, restart count, and command metadata.",
        annotations(read_only_hint = true)
    )]
    async fn list_processes(
        &self,
        Parameters(params): Parameters<ListProcessesParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let records = terminal::list_processes(
                &app,
                params.workspace_id.as_deref(),
                params.limit.unwrap_or(50),
            )?;
            for record in &records {
                activity::sync_process(&app, record);
            }
            if let Some(workspace_id) = params.workspace_id.as_deref() {
                let workspace = approved_workspace(&app, workspace_id)?;
                record_observation(
                    &app,
                    &workspace,
                    trace_group_id.as_deref(),
                    ActivityKind::Process,
                    "listProcesses",
                    format!("Inspected managed processes · {} records", records.len()),
                    Some(format!(
                        "{} running",
                        records
                            .iter()
                            .filter(|record| matches!(
                                record.status,
                                crate::models::ManagedProcessStatus::Running
                            ))
                            .count()
                    )),
                );
            }
            Ok(records)
        })
        .await
    }

    #[tool(
        description = "Read bounded stdout/stderr from a managed persistent process. Supply the returned next offsets on later calls to monitor only new output. This also refreshes and returns the current process state.",
        annotations(read_only_hint = true)
    )]
    async fn read_process_output(
        &self,
        Parameters(params): Parameters<ProcessOutputParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let process = terminal::get_process(&app, &params.process_id)?;
            let workspace = approved_workspace(&app, &process.workspace_id)?;
            let output = terminal::read_process_output(
                &app,
                &params.process_id,
                params.stdout_offset.unwrap_or(0),
                params.stderr_offset.unwrap_or(0),
                params.max_bytes.unwrap_or(64 * 1024),
            )?;
            if let Ok(updated) = terminal::get_process(&app, &params.process_id) {
                activity::sync_process(&app, &updated);
            }
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Process,
                "readOutput",
                format!("Read process output · {}", process.label),
                Some(format!(
                    "stdout {} chars · stderr {} chars · status {:?}",
                    output.stdout.chars().count(),
                    output.stderr.chars().count(),
                    output.status
                )),
            );
            Ok(output)
        })
        .await
    }

    #[tool(
        description = "Stop a RepoTunnel-managed persistent process and its process group. By default RepoTunnel attempts a graceful stop before forcing termination. This never requires an extra confirmation in AI Auto."
    )]
    async fn stop_process(
        &self,
        Parameters(params): Parameters<StopProcessParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let existing = terminal::get_process(&app, &params.process_id)?;
            let workspace = approved_workspace(&app, &existing.workspace_id)?;
            let record =
                terminal::stop_process(&app, &params.process_id, params.force.unwrap_or(false))?;
            activity::sync_process(&app, &record);
            let _ = activity::record_process_record(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                "stopProcess",
                &record,
            );
            Ok(record)
        })
        .await
    }

    #[tool(
        description = "Restart a previously started RepoTunnel-managed process using the same command, workspace-relative working directory, label, and environment overrides. The process keeps the same RepoTunnel process ID and increments restartCount."
    )]
    async fn restart_process(
        &self,
        Parameters(params): Parameters<ProcessIdParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let existing = terminal::get_process(&app, &params.process_id)?;
            let workspace = approved_workspace(&app, &existing.workspace_id)?;
            let record = terminal::restart_process(&app, &workspace, &existing.id)?;
            activity::sync_process(&app, &record);
            let _ = activity::record_process_record(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                "restartProcess",
                &record,
            );
            Ok(record)
        })
        .await
    }

    #[tool(
        description = "List desktop applications RepoTunnel can launch directly, including detected browsers, editors, file managers, and terminals. Returns stable application IDs for launch_target.",
        annotations(read_only_hint = true)
    )]
    async fn list_launchable_applications(&self) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            Ok(launcher::list_applications())
        })
        .await
    }

    #[tool(
        description = "List the five optional deep local integrations (Android Studio, Unity, Blender, Godot, Docker) for an approved project, including whether each app is detected, whether the human enabled ChatGPT access locally, and the exact allowlisted actions available. This is read-only; MCP cannot enable its own integrations.",
        annotations(read_only_hint = true)
    )]
    async fn list_deep_integrations(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            integrations::list(&app, &workspace.id)
        })
        .await
    }

    #[tool(
        description = "Run one bounded action through a deep local integration that the human explicitly enabled in RepoTunnel. Call list_deep_integrations first and use only an action it returns. RepoTunnel refuses disabled/unavailable integrations, keeps targets inside the approved project, and routes commands through the project's command policy. Editing project files still uses the normal RepoTunnel file tools."
    )]
    async fn integration_action(
        &self,
        Parameters(params): Parameters<IntegrationActionParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let result = integrations::run_action(
                &app,
                &workspace,
                &params.integration_id,
                &params.action,
                params.target.as_deref(),
            )?;
            if let Some(command) = result.command.as_ref() {
                let _ = activity::record_terminal_outcome(
                    &app,
                    &workspace,
                    trace_group_id.as_deref(),
                    command,
                );
            }
            if let Some(launch) = result.launch.as_ref() {
                let _ = activity::record_launch_record(
                    &app,
                    &workspace,
                    trace_group_id.as_deref(),
                    &launch.launch,
                );
            }
            Ok(result)
        })
        .await
    }

    #[tool(
        description = "Manage RepoTunnel AI Workspace, an isolated virtual desktop that lets ChatGPT operate one permitted GUI application without stealing the human's real desktop focus. action=status reads state; action=start launches an allowed application into the isolated display and optionally opens a workspace-relative project folder; action=stop terminates the app, window manager, and nested display together. The human must enable the project-level Desktop permission first."
    )]
    async fn ai_workspace_session(
        &self,
        Parameters(params): Parameters<AiWorkspaceSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let state = app.state::<AppState>();
            match params.action.as_str() {
                "status" => state.ai_workspace.status(&app, &workspace.id),
                "start" => {
                    let application_id = params.application_id.as_deref().ok_or_else(|| {
                        "AI Workspace action=start requires application_id from list_launchable_applications.".to_string()
                    })?;
                    state
                        .ai_workspace
                        .start(&app, &workspace, application_id, params.target.as_deref())
                }
                "stop" => state.ai_workspace.stop(&app, &workspace.id),
                _ => Err("AI Workspace session action must be status, start, or stop.".to_string()),
            }
        })
        .await
    }

    #[tool(
        description = "Inspect the isolated AI Workspace window list and exact bounds. Use the returned window IDs before visual pointer work so click/scroll coordinates can be relative to the intended dialog or application window instead of the whole virtual desktop.",
        annotations(read_only_hint = true)
    )]
    async fn ai_workspace_inspect(
        &self,
        Parameters(params): Parameters<AiWorkspaceInspectParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let state = app.state::<AppState>();
            state.ai_workspace.inspect(&app, &workspace.id)
        })
        .await
    }

    #[tool(
        description = "Send input only to the isolated AI Workspace display. Supported actions: activate, click, key, type, scroll. Prefer a window_id from ai_workspace_inspect for click/scroll so normalized coordinates are relative to that exact isolated window; omit it only for full-screen fallback. Credential/authentication-window typing remains blocked."
    )]
    async fn ai_workspace_action(
        &self,
        Parameters(params): Parameters<AiWorkspaceActionParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let state = app.state::<AppState>();
            state.ai_workspace.action(
                &app,
                &workspace.id,
                &params.action,
                params.window_id.as_deref(),
                params.x_ratio,
                params.y_ratio,
                params.click_count,
                params.shortcut.as_deref(),
                params.text.as_deref(),
                params.delta_x,
                params.delta_y,
            )
        })
        .await
    }

    #[tool(
        description = "Run 1..64 already-grounded AI Workspace actions in one bounded fast-path request. Supports activate, click, key, type, scroll, and wait steps; wait can use a short delay, active-title condition, or isolated-window-count condition. Prefer this when several consecutive actions are already known because it avoids repeated MCP/helper startup round trips. The existing ai_workspace_action remains the reliable single-step fallback, and credential/authentication typing protections remain active."
    )]
    async fn ai_workspace_sequence(
        &self,
        Parameters(params): Parameters<AiWorkspaceSequenceParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let steps = params
                .steps
                .into_iter()
                .map(|step| {
                    serde_json::to_value(step).map_err(|error| {
                        format!("Could not encode AI Workspace sequence step: {error}")
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let state = app.state::<AppState>();
            state
                .ai_workspace
                .sequence(&app, &workspace.id, params.window_id.as_deref(), &steps)
        })
        .await
    }

    #[tool(
        description = "Capture the current isolated AI Workspace screen for visual grounding. This screenshot comes from RepoTunnel's nested virtual display, not the human's real desktop, so the human can keep using other applications while ChatGPT works.",
        annotations(read_only_hint = true)
    )]
    async fn ai_workspace_take_screenshot(
        &self,
        Parameters(params): Parameters<AiWorkspaceFrameParams>,
    ) -> Result<CallToolResult, McpError> {
        const MAX_MCP_SCREENSHOT_BYTES: u64 = 8 * 1024 * 1024;
        let app = self.app.clone();
        let result = tokio::task::spawn_blocking(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let state = app.state::<AppState>();
            state.ai_workspace.frame(
                &app,
                &workspace.id,
                None,
                params.max_width.unwrap_or(1440),
                true,
            )
        })
        .await;
        Ok(match result {
            Ok(Ok(frame)) if frame.size_bytes <= MAX_MCP_SCREENSHOT_BYTES && !frame.data_base64.is_empty() => {
                let metadata = serde_json::json!({
                    "ok": true,
                    "result": {
                        "sessionId": frame.session_id,
                        "mimeType": frame.mime_type.clone(),
                        "sizeBytes": frame.size_bytes,
                        "width": frame.width,
                        "height": frame.height,
                        "sourceWidth": frame.source_width,
                        "sourceHeight": frame.source_height,
                        "activeTitle": frame.active_title,
                    }
                });
                CallToolResult::success(vec![
                    ContentBlock::text(serde_json::to_string(&metadata).unwrap_or_else(|_| "{\"ok\":true}".to_string())),
                    ContentBlock::image(frame.data_base64, frame.mime_type),
                ])
            }
            Ok(Ok(frame)) => error_result(format!(
                "The AI Workspace screenshot is {} bytes or empty, so RepoTunnel refused to return it through MCP.",
                frame.size_bytes
            )),
            Ok(Err(error)) => error_result(error),
            Err(error) => error_result(format!("AI Workspace screenshot task failed: {error}")),
        })
    }

    #[tool(
        description = "List currently running desktop applications available to project-scoped Desktop Control. Shows whether the single local Desktop permission is enabled for this project and whether each app exposes a Linux accessibility tree. RepoTunnel itself is never included and MCP cannot enable the Desktop permission.",
        annotations(read_only_hint = true)
    )]
    async fn list_desktop_applications(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let mut applications = desktop_control::list(&app, &workspace.id)?;
            let state = app.state::<AppState>();
            let status = state.ai_workspace.status(&app, &workspace.id)?;
            if status.running {
                let enabled = desktop_control::is_enabled(&app, &workspace.id)?;
                let window_count = state
                    .ai_workspace
                    .inspect(&app, &workspace.id)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("windows")
                            .and_then(|items| items.as_array())
                            .map(Vec::len)
                    })
                    .unwrap_or(0);
                applications.push(desktop_control::DesktopControlApplication {
                    id: "ai-workspace".to_string(),
                    name: format!(
                        "AI Workspace · {}",
                        status
                            .application_name
                            .unwrap_or_else(|| "Application".to_string())
                    ),
                    running: true,
                    accessibility: false,
                    window_count,
                    enabled,
                    message: if enabled {
                        "Isolated AI Workspace control enabled for this project".to_string()
                    } else {
                        "Enable Desktop locally to control the isolated AI Workspace".to_string()
                    },
                });
                applications.sort_by(|left, right| {
                    left.name
                        .to_ascii_lowercase()
                        .cmp(&right.name.to_ascii_lowercase())
                });
            }
            Ok(applications)
        })
        .await
    }

    #[tool(
        description = "Inspect the semantic accessibility UI of one running desktop application while the human-enabled project-level Desktop permission is on. Returns stable short-lived element IDs, roles, labels, actions, states and bounds. Sensitive password/credential values are never returned. Inspect again after the UI changes before acting.",
        annotations(read_only_hint = true)
    )]
    async fn inspect_desktop_app(
        &self,
        Parameters(params): Parameters<DesktopInspectParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            if params.application_id == "ai-workspace" {
                if !desktop_control::is_enabled(&app, &workspace.id)? {
                    return Err("Desktop permission is off for this project.".to_string());
                }
                let state = app.state::<AppState>();
                let mut value = state.ai_workspace.inspect(&app, &workspace.id)?;
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "applicationId".to_string(),
                        serde_json::json!("ai-workspace"),
                    );
                    object.insert("name".to_string(), serde_json::json!("AI Workspace"));
                    object.insert("elements".to_string(), serde_json::json!([]));
                    object.insert("truncated".to_string(), serde_json::json!(false));
                }
                return Ok(value);
            }
            desktop_control::inspect(
                &app,
                &workspace.id,
                &params.application_id,
                params.limit.unwrap_or(300),
            )
        })
        .await
    }

    #[tool(
        description = "Perform one bounded UI action inside a desktop application while the human-enabled project-level Desktop permission is on. Supported actions: activate, click, type, key, scroll. Use activate to raise/focus the permitted app window before pointer or keyboard work. Prefer semantic element IDs from inspect_desktop_app. Blind typing is blocked; credential/password fields are blocked; coordinate fallback is window-relative and cannot leave the target app window; RepoTunnel can never control its own UI."
    )]
    async fn desktop_app_action(
        &self,
        Parameters(params): Parameters<DesktopActionParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            if params.application_id == "ai-workspace" {
                if !desktop_control::is_enabled(&app, &workspace.id)? {
                    return Err("Desktop permission is off for this project.".to_string());
                }
                if params.element_id.is_some() {
                    return Err("AI Workspace currently uses isolated-window coordinates rather than semantic element IDs.".to_string());
                }
                let state = app.state::<AppState>();
                if params.action == "type" && params.clear_first.unwrap_or(false) {
                    state.ai_workspace.action(
                        &app,
                        &workspace.id,
                        "key",
                        params.window_id.as_deref(),
                        None,
                        None,
                        None,
                        Some("Ctrl+A"),
                        None,
                        None,
                        None,
                    )?;
                }
                return state.ai_workspace.action(
                    &app,
                    &workspace.id,
                    &params.action,
                    params.window_id.as_deref(),
                    params.x_ratio,
                    params.y_ratio,
                    Some(1),
                    params.shortcut.as_deref(),
                    params.text.as_deref(),
                    params.delta_x,
                    params.delta_y,
                );
            }
            desktop_control::action(
                &app,
                &workspace.id,
                &params.application_id,
                &params.action,
                params.element_id.as_deref(),
                params.window_id.as_deref(),
                params.text.as_deref(),
                params.clear_first.unwrap_or(false),
                params.shortcut.as_deref(),
                params.x_ratio,
                params.y_ratio,
                params.delta_x,
                params.delta_y,
            )
        })
        .await
    }

    #[tool(
        description = "Capture one current window belonging to a desktop application while the human-enabled project-level Desktop permission is on and return it as PNG image content for visual grounding. The capture is limited to that target application's window, not the whole desktop.",
        annotations(read_only_hint = true)
    )]
    async fn desktop_take_screenshot(
        &self,
        Parameters(params): Parameters<DesktopScreenshotParams>,
    ) -> Result<CallToolResult, McpError> {
        const MAX_MCP_SCREENSHOT_BYTES: u64 = 8 * 1024 * 1024;
        let app = self.app.clone();
        let result = tokio::task::spawn_blocking(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            if params.application_id == "ai-workspace" {
                if !desktop_control::is_enabled(&app, &workspace.id)? {
                    return Err("Desktop permission is off for this project.".to_string());
                }
                let state = app.state::<AppState>();
                let frame = state.ai_workspace.frame(
                    &app,
                    &workspace.id,
                    params.window_id.as_deref(),
                    1440,
                    true,
                )?;
                return Ok(desktop_control::DesktopScreenshot {
                    application_id: "ai-workspace".to_string(),
                    window_id: params.window_id.unwrap_or_else(|| "active".to_string()),
                    mime_type: frame.mime_type,
                    size_bytes: frame.size_bytes,
                    width: frame.width,
                    height: frame.height,
                    data_base64: frame.data_base64,
                });
            }
            desktop_control::screenshot(
                &app,
                &workspace.id,
                &params.application_id,
                params.window_id.as_deref(),
            )
        })
        .await;
        Ok(match result {
            Ok(Ok(screenshot)) if screenshot.size_bytes <= MAX_MCP_SCREENSHOT_BYTES && !screenshot.data_base64.is_empty() => {
                let metadata = serde_json::json!({
                    "ok": true,
                    "result": {
                        "applicationId": screenshot.application_id,
                        "windowId": screenshot.window_id,
                        "mimeType": screenshot.mime_type.clone(),
                        "sizeBytes": screenshot.size_bytes,
                        "width": screenshot.width,
                        "height": screenshot.height,
                    }
                });
                CallToolResult::success(vec![
                    ContentBlock::text(serde_json::to_string(&metadata).unwrap_or_else(|_| "{\"ok\":true}".to_string())),
                    ContentBlock::image(screenshot.data_base64, screenshot.mime_type),
                ])
            }
            Ok(Ok(screenshot)) => error_result(format!(
                "The desktop screenshot is {} bytes or empty, so RepoTunnel refused to return it through MCP.",
                screenshot.size_bytes
            )),
            Ok(Err(message)) => error_result(message),
            Err(error) => error_result(format!("The desktop screenshot task could not complete: {error}")),
        })
    }

    #[tool(
        description = "Launch one structured desktop target for an approved project. kind=url opens an HTTP/HTTPS URL, kind=workspace_path opens a project-relative file/folder, and kind=application launches an allowed application ID. URL/path targets may optionally specify an application ID. In AI Auto the launch happens immediately with no confirmation; in AI Review it may queue for local Accept/Reject."
    )]
    async fn launch_target(
        &self,
        Parameters(params): Parameters<LaunchTargetParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let outcome = match params.kind {
                LaunchTargetKindParam::Url => launcher::request_open_url(
                    &app,
                    &workspace,
                    params.target,
                    params.application_id,
                ),
                LaunchTargetKindParam::WorkspacePath => launcher::request_open_workspace_path(
                    &app,
                    &workspace,
                    params.target,
                    params.application_id,
                ),
                LaunchTargetKindParam::Application => {
                    if params.application_id.is_some() {
                        return Err("application_id must be omitted when kind=application; put the application ID in target.".to_string());
                    }
                    launcher::request_launch_application(&app, &workspace, params.target)
                }
            }?;
            let _ = activity::record_launch_record(&app, &workspace, trace_group_id.as_deref(), &outcome.launch);
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "List recent structured application/URL/path launch records, including pending review actions and final launched/failed/rejected state.",
        annotations(read_only_hint = true)
    )]
    async fn list_launch_history(
        &self,
        Parameters(params): Parameters<ListActivityParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            launcher::list_history(
                &app,
                params.workspace_id.as_deref(),
                params.limit.unwrap_or(30),
            )
        })
        .await
    }

    #[tool(
        description = "List installed Chromium-family browsers that support RepoTunnel's isolated Chrome DevTools automation session. Returns stable browser IDs for browser_action action=start.",
        annotations(read_only_hint = true)
    )]
    async fn list_automation_browsers(&self) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            Ok(browser::list_applications())
        })
        .await
    }

    #[tool(
        description = "Get the managed browser-automation session status for an approved project, including running state, browser, PID, session ID, and active tab.",
        annotations(read_only_hint = true)
    )]
    async fn get_browser_status(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let status = browser::status(&app, &workspace);
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Browser,
                "status",
                format!(
                    "Checked browser state · {}",
                    if status.running { "running" } else { "stopped" }
                ),
                status.browser_name.clone(),
            );
            Ok(status)
        })
        .await
    }

    #[tool(
        description = "List tabs in the RepoTunnel-managed isolated browser session for an approved project, including tab IDs, titles, URLs, and active state.",
        annotations(read_only_hint = true)
    )]
    async fn list_browser_tabs(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let tabs = browser::list_tabs(&app, &workspace)?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Browser,
                "listTabs",
                format!("Inspected browser tabs · {} open", tabs.len()),
                tabs.iter()
                    .find(|tab| tab.active)
                    .map(|tab| format!("Active: {}", tab.url)),
            );
            Ok(tabs)
        })
        .await
    }

    #[tool(
        description = "Control RepoTunnel's isolated browser session with one stable action contract: start, stop, open_tab, activate_tab, close_tab, navigate, click, type, scroll, or reload. In AI Auto browser mutations execute immediately with no confirmation. In AI Review they may queue for local Accept/Reject. Use list_browser_tabs to obtain tab IDs."
    )]
    async fn browser_action(
        &self,
        Parameters(params): Parameters<BrowserActionParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            team::assert_browser_mutation_available(&app, &workspace.id, client_key.as_deref())?;
            let outcome = match params.action {
                BrowserActionParam::Start => {
                    let application_id =
                        required_text(params.application_id, "application_id", "start")?;
                    browser::request_start(&app, &workspace, &application_id)
                }
                BrowserActionParam::Stop => browser::request_stop(&app, &workspace),
                BrowserActionParam::OpenTab => {
                    let url = required_text(params.url, "url", "open_tab")?;
                    browser::request_open_tab(&app, &workspace, &url)
                }
                BrowserActionParam::ActivateTab => {
                    let tab_id = required_text(params.tab_id, "tab_id", "activate_tab")?;
                    browser::request_activate_tab(&app, &workspace, &tab_id)
                }
                BrowserActionParam::CloseTab => {
                    let tab_id = required_text(params.tab_id, "tab_id", "close_tab")?;
                    browser::request_close_tab(&app, &workspace, &tab_id)
                }
                BrowserActionParam::Navigate => {
                    let tab_id = required_text(params.tab_id, "tab_id", "navigate")?;
                    let url = required_text(params.url, "url", "navigate")?;
                    browser::request_navigate(&app, &workspace, &tab_id, &url)
                }
                BrowserActionParam::Click => {
                    let tab_id = required_text(params.tab_id, "tab_id", "click")?;
                    let selector = required_text(params.selector, "selector", "click")?;
                    browser::request_click(&app, &workspace, &tab_id, &selector)
                }
                BrowserActionParam::Type => {
                    let tab_id = required_text(params.tab_id, "tab_id", "type")?;
                    let selector = required_text(params.selector, "selector", "type")?;
                    let text = params
                        .text
                        .ok_or_else(|| "text is required for browser action type.".to_string())?;
                    browser::request_type(
                        &app,
                        &workspace,
                        &tab_id,
                        &selector,
                        &text,
                        params.clear_first.unwrap_or(false),
                    )
                }
                BrowserActionParam::Scroll => {
                    let tab_id = required_text(params.tab_id, "tab_id", "scroll")?;
                    let (delta_x, delta_y) = match (params.delta_x, params.delta_y) {
                        (None, None) => (0, 600),
                        (x, y) => (x.unwrap_or(0), y.unwrap_or(0)),
                    };
                    browser::request_scroll(&app, &workspace, &tab_id, delta_x, delta_y)
                }
                BrowserActionParam::Reload => {
                    let tab_id = required_text(params.tab_id, "tab_id", "reload")?;
                    browser::request_reload(&app, &workspace, &tab_id)
                }
            }?;
            let _ = activity::record_browser_record(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                &outcome.action,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "Inspect the current DOM/page content of a managed browser tab. Optionally target a CSS selector. Returns bounded text and HTML for reasoning about the rendered application.",
        annotations(read_only_hint = true)
    )]
    async fn browser_inspect_page(
        &self,
        Parameters(params): Parameters<BrowserInspectParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let inspection = browser::inspect_page(
                &app,
                &workspace,
                &params.tab_id,
                params.selector.as_deref(),
                params.max_chars.unwrap_or(32_000),
            )?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Browser,
                "inspectPage",
                format!("Inspected browser page · {}", inspection.title),
                Some(format!(
                    "{} · selector {}",
                    inspection.url,
                    inspection.selector.as_deref().unwrap_or("<page>")
                )),
            );
            Ok(inspection)
        })
        .await
    }

    #[tool(
        description = "Read the most recent element selected by the human from RepoTunnel's Live Preview. Use this before editing when the human says 'change this', 'this button', or otherwise refers to a visually selected UI element.",
        annotations(read_only_hint = true)
    )]
    async fn get_visual_selection(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            browser::get_visual_selection(&workspace.id)
        })
        .await
    }

    #[tool(
        description = "Capture the current managed browser tab as a PNG and return it as MCP image content so the assistant can visually inspect the UI. Set full_page=true only when needed; captures above 8 MiB are refused and should be retried as viewport screenshots.",
        annotations(read_only_hint = true)
    )]
    async fn browser_take_screenshot(
        &self,
        Parameters(params): Parameters<BrowserScreenshotParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_browser_screenshot_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let screenshot = browser::screenshot(
                &app,
                &workspace,
                &params.tab_id,
                params.full_page.unwrap_or(false),
            )?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Browser,
                "screenshot",
                format!(
                    "Captured {} screenshot",
                    if screenshot.full_page {
                        "full-page"
                    } else {
                        "viewport"
                    }
                ),
                Some(format!(
                    "{} bytes · tab {}",
                    screenshot.size_bytes, screenshot.tab_id
                )),
            );
            Ok(screenshot)
        })
        .await
    }

    #[tool(
        description = "Read recent console warnings/errors, JavaScript exceptions, failed network requests, and HTTP error responses captured continuously from the managed browser. Optionally filter to one tab.",
        annotations(read_only_hint = true)
    )]
    async fn get_browser_diagnostics(
        &self,
        Parameters(params): Parameters<BrowserDiagnosticsParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let diagnostics = browser::diagnostics(
                &app,
                &workspace,
                params.tab_id.as_deref(),
                params.limit.unwrap_or(50),
            )?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Browser,
                "diagnostics",
                format!(
                    "Checked browser diagnostics · {} console / {} network",
                    diagnostics.console_entries.len(),
                    diagnostics.network_failures.len()
                ),
                None,
            );
            Ok(diagnostics)
        })
        .await
    }

    #[tool(
        description = "List recent browser-control action records, including queued Review actions and final applied/failed/rejected state. Completed type actions do not retain typed text.",
        annotations(read_only_hint = true)
    )]
    async fn list_browser_history(
        &self,
        Parameters(params): Parameters<ListActivityParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            browser::list_history(
                &app,
                params.workspace_id.as_deref(),
                params.limit.unwrap_or(40),
            )
        })
        .await
    }

    #[tool(
        description = "Get project monitoring status for an approved workspace. Monitoring tracks filtered project-file changes and feeds the unified process/terminal/port/browser observation snapshot.",
        annotations(read_only_hint = true)
    )]
    async fn get_monitoring_status(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let status = monitoring::status(&app, &workspace);
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Monitoring,
                "status",
                format!(
                    "Checked project monitor · {}",
                    if status.running { "running" } else { "stopped" }
                ),
                None,
            );
            Ok(status)
        })
        .await
    }

    #[tool(
        description = "Persistently enable or disable read-only project monitoring for an approved workspace. Monitoring never edits project files and does not require a separate Review approval."
    )]
    async fn set_workspace_monitoring(
        &self,
        Parameters(params): Parameters<WorkspaceMonitoringParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let status = if params.enabled {
                monitoring::start_monitoring(&app, &workspace)?
            } else {
                monitoring::stop_monitoring(&app, &workspace)?
            };
            let _ = activity::record(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Monitoring,
                "setMonitoring",
                if params.enabled {
                    "Enabled project monitoring"
                } else {
                    "Disabled project monitoring"
                },
                None,
                ActivityStatus::Succeeded,
                None,
            );
            Ok(status)
        })
        .await
    }

    #[tool(
        description = "Get one combined observation snapshot for an approved workspace: monitoring state, running managed processes with output tails, listening ports/dev-server correlation, recent terminal results, browser tabs/console/network diagnostics, and recent project file changes.",
        annotations(read_only_hint = true)
    )]
    async fn get_monitoring_snapshot(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let snapshot = monitoring::snapshot(&app, &workspace)?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Monitoring,
                "snapshot",
                format!(
                    "Observed {} processes · {} ports · {} file events",
                    snapshot.processes.len(),
                    snapshot.ports.len(),
                    snapshot.file_events.len()
                ),
                Some(format!(
                    "Browser console: {} · network failures: {}",
                    snapshot.browser.console_entries.len(),
                    snapshot.browser.network_failures.len()
                )),
            );
            if !snapshot.file_events.is_empty() {
                record_observation(
                    &app,
                    &workspace,
                    trace_group_id.as_deref(),
                    ActivityKind::Files,
                    "monitoredFileChanges",
                    format!(
                        "Observed {} project file changes",
                        snapshot.file_events.len()
                    ),
                    monitoring_file_detail(&snapshot.file_events),
                );
            }
            Ok(snapshot)
        })
        .await
    }

    #[tool(
        description = "List recent project-file monitoring events (created, modified, deleted). Events are filtered through RepoTunnel's existing project-index/protected-path rules and are observational only.",
        annotations(read_only_hint = true)
    )]
    async fn list_monitoring_file_events(
        &self,
        Parameters(params): Parameters<MonitoringFileEventsParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            let workspace = if let Some(workspace_id) = params.workspace_id.as_deref() {
                Some(approved_workspace(&app, workspace_id)?)
            } else {
                None
            };
            let events = monitoring::list_file_events(
                &app,
                params.workspace_id.as_deref(),
                params.limit.unwrap_or(50),
            )?;
            if let Some(workspace) = workspace.as_ref() {
                record_observation(
                    &app,
                    workspace,
                    trace_group_id.as_deref(),
                    ActivityKind::Monitoring,
                    "fileEvents",
                    format!("Inspected project changes · {} events", events.len()),
                    None,
                );
                if !events.is_empty() {
                    record_observation(
                        &app,
                        workspace,
                        trace_group_id.as_deref(),
                        ActivityKind::Files,
                        "monitoredFileChanges",
                        format!("Observed {} project file changes", events.len()),
                        monitoring_file_detail(&events),
                    );
                }
            }
            Ok(events)
        })
        .await
    }

    #[tool(
        description = "Read the shared RepoTunnel Team Mode state for two-AI collaboration. Supply session_id to inspect a known team, or workspace_id to get the latest team session for that approved project. The snapshot includes assigned agent IDs/roles, goal and success criteria, task ownership/dependencies/review state, discussion messages, expiring file/folder claims, progress, and a recommended next action. Call this before team project work and after the other agent may have changed shared state. An active agent can pass after_revision plus wait_seconds to wait up to 30 seconds for the other AI to change the shared state instead of ending its turn immediately.",
        annotations(read_only_hint = true)
    )]
    async fn team_status(
        &self,
        Parameters(params): Parameters<TeamStatusParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            if let Some(session_id) = params.session_id.as_deref() {
                let snapshot = if let Some(after_revision) = params.after_revision {
                    team::wait_for_snapshot(
                        &app,
                        session_id,
                        params.agent_id.as_deref(),
                        after_revision,
                        params.wait_seconds.unwrap_or(0),
                    )?
                } else {
                    team::get_snapshot(&app, session_id, params.agent_id.as_deref())?
                };
                let workspace = approved_workspace(&app, &snapshot.session.workspace_id)?;
                let mut snapshot = snapshot;
                if let (Some(client_key), Some(agent_id)) =
                    (client_key.as_deref(), params.agent_id.as_deref())
                {
                    team::bind_client(&app, client_key, session_id, agent_id)?;
                    if snapshot
                        .session
                        .agents
                        .iter()
                        .any(|agent| agent.id == agent_id && agent.joined_at.is_some())
                    {
                        snapshot = team::heartbeat(&app, session_id, agent_id)?;
                    }
                }
                if params.after_revision.is_none() || params.wait_seconds.unwrap_or(0) == 0 {
                    record_observation(
                        &app,
                        &workspace,
                        trace_group_id.as_deref(),
                        ActivityKind::Team,
                        "teamStatus",
                        format!(
                            "Checked AI team · {:?} · {:?}",
                            snapshot.session.status, snapshot.session.phase
                        ),
                        Some(format!(
                            "{} open · {} done · {} blocked",
                            snapshot.progress.open_task_count,
                            snapshot.progress.done_task_count,
                            snapshot.progress.blocked_task_count
                        )),
                    );
                }
                return Ok(Some(snapshot));
            }

            let workspace_id = params.workspace_id.as_deref().ok_or_else(|| {
                "team_status requires either session_id or workspace_id.".to_string()
            })?;
            let workspace = approved_workspace(&app, workspace_id)?;
            let mut snapshot = team::latest_snapshot_for_workspace(
                &app,
                workspace_id,
                params.agent_id.as_deref(),
            )?;
            if let (Some(current), Some(client_key), Some(agent_id)) = (
                snapshot.as_ref(),
                client_key.as_deref(),
                params.agent_id.as_deref(),
            ) {
                let session_id = current.session.id.clone();
                let joined = current
                    .session
                    .agents
                    .iter()
                    .any(|agent| agent.id == agent_id && agent.joined_at.is_some());
                team::bind_client(&app, client_key, &session_id, agent_id)?;
                if joined {
                    snapshot = Some(team::heartbeat(&app, &session_id, agent_id)?);
                }
            }
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Team,
                "teamStatus",
                if snapshot.is_some() {
                    "Checked latest AI team session"
                } else {
                    "Checked AI team status · no session yet"
                },
                None,
            );
            Ok(snapshot)
        })
        .await
    }

    #[tool(
        description = "Coordinate two AIs through one persistent RepoTunnel Team attached to a project until the user explicitly ends it. create_session creates the A/B Team once; join/heartbeat keep stable agent identities. BOTH engineers must join before planning starts. During Planning each engineer posts one Plan message, each creates one distinct initial implementation task, then each posts a Decision confirming the split; RepoTunnel unlocks parallel implementation only after both confirmations. create_task/claim_task divide distinct work: one primary owner per implementation task, one active implementation task per agent, and duplicate open task titles are rejected. Both engineers must finish their own meaningful implementation contribution; review/testing alone cannot satisfy the two-engineer contribution requirement. handoff_task transfers ownership instead of allowing duplicate implementation. update_task enforces cross-review and requires concrete feedback when a reviewer sends work back for bugs/errors. verify_criterion records evidence. task-scoped path claims protect file edits; the reserved `@browser` claim serializes interactive managed-browser mutations so two AIs never type/click in the same shared browser simultaneously. After a completed work cycle, a new human request posted as `USER REQUEST:` reuses the SAME Team. complete closes only the CURRENT request; the Team stays active. MCP agents cannot end the Team itself; pause/end remain user-controlled in the desktop app."
    )]
    async fn team_action(
        &self,
        Parameters(params): Parameters<TeamActionParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        let client_key = request_client_key(&parts);
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;

            if matches!(params.action, TeamActionParam::CreateSession) {
                let workspace_id = params.workspace_id.as_deref().ok_or_else(|| "workspace_id is required for create_session.".to_string())?;
                let workspace = approved_workspace(&app, workspace_id)?;
                let snapshot = team::create_session(
                    &app,
                    &workspace,
                    params.goal.unwrap_or_default(),
                    params.success_criteria.unwrap_or_default(),
                    params.agent_a_name.unwrap_or_else(|| "Engineer A".to_string()),
                    params.agent_a_role.unwrap_or_else(|| "Plan, implement, test, debug, and review distinct product work as Engineer A. Avoid duplicate implementation, coordinate explicit handoffs, and verify the other agent's work.".to_string()),
                    params.agent_b_name.unwrap_or_else(|| "Engineer B".to_string()),
                    params.agent_b_role.unwrap_or_else(|| "Plan, implement, test, debug, and review distinct product work as Engineer B. Avoid duplicate implementation, coordinate explicit handoffs, and verify the other agent's work.".to_string()),
                )?;
                let _ = activity::record(
                    &app,
                    &workspace,
                    trace_group_id.as_deref(),
                    ActivityKind::Team,
                    "teamCreate",
                    "Created a two-agent Team Mode session",
                    Some(snapshot.session.goal.clone()),
                    ActivityStatus::Succeeded,
                    Some(snapshot.session.id.clone()),
                );
                return Ok(snapshot);
            }

            let session_id = params.session_id.as_deref().ok_or_else(|| "session_id is required for this Team Mode action.".to_string())?;
            let before = team::get_snapshot(&app, session_id, params.agent_id.as_deref())?;
            let workspace = approved_workspace(&app, &before.session.workspace_id)?;
            if let (Some(client_key), Some(agent_id)) = (client_key.as_deref(), params.agent_id.as_deref()) {
                team::bind_client(&app, client_key, session_id, agent_id)?;
            }

            let (snapshot, action_name, summary, status) = match params.action {
                TeamActionParam::CreateSession => unreachable!(),
                TeamActionParam::Join => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for join.".to_string())?;
                    let snapshot = team::join_agent(&app, session_id, agent_id, params.client_label)?;
                    let name = snapshot.session.agents.iter().find(|agent| agent.id == agent_id).map(|agent| agent.name.clone()).unwrap_or_else(|| "Agent".to_string());
                    (snapshot, "teamJoin", format!("{name} joined the AI team"), ActivityStatus::Succeeded)
                }
                TeamActionParam::Heartbeat => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for heartbeat.".to_string())?;
                    let snapshot = team::heartbeat(&app, session_id, agent_id)?;
                    return Ok(snapshot);
                }
                TeamActionParam::PostMessage => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for post_message.".to_string())?;
                    let kind = match params.message_kind.ok_or_else(|| "message_kind is required for post_message.".to_string())? {
                        TeamMessageKindParam::Plan => TeamMessageKind::Plan,
                        TeamMessageKindParam::Progress => TeamMessageKind::Progress,
                        TeamMessageKindParam::Question => TeamMessageKind::Question,
                        TeamMessageKindParam::Review => TeamMessageKind::Review,
                        TeamMessageKindParam::Decision => TeamMessageKind::Decision,
                        TeamMessageKindParam::Handoff => TeamMessageKind::Handoff,
                    };
                    let message = params.message.ok_or_else(|| "message is required for post_message.".to_string())?;
                    let snapshot = team::post_message(&app, session_id, agent_id, kind, message, params.task_id)?;
                    (snapshot, "teamMessage", "Posted AI-to-AI team message".to_string(), ActivityStatus::Succeeded)
                }
                TeamActionParam::CreateTask => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for create_task.".to_string())?;
                    let title = params.title.ok_or_else(|| "title is required for create_task.".to_string())?;
                    let snapshot = team::create_task(
                        &app,
                        session_id,
                        agent_id,
                        title,
                        params.description.unwrap_or_default(),
                        params.priority,
                        params.depends_on.unwrap_or_default(),
                    )?;
                    (snapshot, "teamCreateTask", "Created AI team task".to_string(), ActivityStatus::Succeeded)
                }
                TeamActionParam::ClaimTask => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for claim_task.".to_string())?;
                    let task_id = params.task_id.as_deref().ok_or_else(|| "task_id is required for claim_task.".to_string())?;
                    let snapshot = team::claim_task(
                        &app,
                        session_id,
                        agent_id,
                        task_id,
                        params.paths.unwrap_or_default(),
                        params.lock_ttl_seconds,
                    )?;
                    (snapshot, "teamClaimTask", "Claimed distinct AI team implementation task".to_string(), ActivityStatus::Succeeded)
                }
                TeamActionParam::HandoffTask => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for handoff_task.".to_string())?;
                    let task_id = params.task_id.as_deref().ok_or_else(|| "task_id is required for handoff_task.".to_string())?;
                    let target_agent_id = params.target_agent_id.as_deref().ok_or_else(|| "target_agent_id is required for handoff_task.".to_string())?;
                    let snapshot = team::handoff_task(
                        &app,
                        session_id,
                        agent_id,
                        task_id,
                        target_agent_id,
                        params.message,
                    )?;
                    (snapshot, "teamHandoffTask", "Transferred AI team task ownership".to_string(), ActivityStatus::Succeeded)
                }
                TeamActionParam::UpdateTask => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for update_task.".to_string())?;
                    let task_id = params.task_id.as_deref().ok_or_else(|| "task_id is required for update_task.".to_string())?;
                    let task_status = match params.task_status.ok_or_else(|| "task_status is required for update_task.".to_string())? {
                        TeamTaskStatusParam::Todo => TeamTaskStatus::Todo,
                        TeamTaskStatusParam::InProgress => TeamTaskStatus::InProgress,
                        TeamTaskStatusParam::Review => TeamTaskStatus::Review,
                        TeamTaskStatusParam::Blocked => TeamTaskStatus::Blocked,
                        TeamTaskStatusParam::Done => TeamTaskStatus::Done,
                        TeamTaskStatusParam::Cancelled => TeamTaskStatus::Cancelled,
                    };
                    let snapshot = team::update_task(
                        &app,
                        session_id,
                        agent_id,
                        task_id,
                        task_status,
                        params.result,
                        params.blocked_reason,
                        params.reviewer_agent_id,
                    )?;
                    let activity_status = ActivityStatus::Succeeded;
                    (snapshot, "teamUpdateTask", format!("Updated AI team task · {:?}", task_status), activity_status)
                }
                TeamActionParam::VerifyCriterion => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for verify_criterion.".to_string())?;
                    let criterion_index = params.criterion_index.ok_or_else(|| "criterion_index is required for verify_criterion.".to_string())?;
                    let evidence = params.evidence.ok_or_else(|| "evidence is required for verify_criterion.".to_string())?;
                    let snapshot = team::verify_criterion(&app, session_id, agent_id, criterion_index, evidence)?;
                    (snapshot, "teamVerifyCriterion", format!("Verified AI team success criterion {}", criterion_index + 1), ActivityStatus::Succeeded)
                }
                TeamActionParam::LockPaths => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for lock_paths.".to_string())?;
                    let snapshot = team::lock_paths(
                        &app,
                        session_id,
                        agent_id,
                        params.task_id,
                        params.paths.unwrap_or_default(),
                        params.lock_ttl_seconds,
                    )?;
                    (snapshot, "teamLockPaths", "Claimed project paths for AI team work".to_string(), ActivityStatus::Succeeded)
                }
                TeamActionParam::ReleasePaths => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for release_paths.".to_string())?;
                    let snapshot = team::release_paths(&app, session_id, agent_id, params.paths.unwrap_or_default())?;
                    (snapshot, "teamReleasePaths", "Released AI team path claims".to_string(), ActivityStatus::Succeeded)
                }
                TeamActionParam::SetPhase => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for set_phase.".to_string())?;
                    let phase = match params.phase.ok_or_else(|| "phase is required for set_phase.".to_string())? {
                        TeamPhaseParam::Planning => TeamPhase::Planning,
                        TeamPhaseParam::Executing => TeamPhase::Executing,
                        TeamPhaseParam::Reviewing => TeamPhase::Reviewing,
                        TeamPhaseParam::Verifying => TeamPhase::Verifying,
                    };
                    let snapshot = team::set_phase(&app, session_id, agent_id, phase)?;
                    (snapshot, "teamPhase", format!("Moved AI team to {:?}", phase), ActivityStatus::Succeeded)
                }
                TeamActionParam::Complete => {
                    let agent_id = params.agent_id.as_deref().ok_or_else(|| "agent_id is required for complete.".to_string())?;
                    let summary_text = params.completion_summary.ok_or_else(|| "completion_summary is required for complete.".to_string())?;
                    let snapshot = team::complete_work_cycle(&app, session_id, agent_id, summary_text)?;
                    (snapshot, "teamComplete", "Completed current AI Team work request · persistent Team remains active".to_string(), ActivityStatus::Succeeded)
                }
            };

            let detail = snapshot.recommended_action.clone();
            let _ = activity::record(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Team,
                action_name,
                summary,
                detail,
                status,
                Some(snapshot.session.id.clone()),
            );
            Ok(snapshot)
        })
        .await
    }

    #[tool(
        description = "Inspect Git status for an approved workspace whose .git directory is inside the workspace root. Returns branch/HEAD, ahead/behind counts, and staged/unstaged/untracked/conflicted paths. This does not modify Git.",
        annotations(read_only_hint = true)
    )]
    async fn git_status(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let status = git::repository_status(&workspace);
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Git,
                "status",
                format!(
                    "Checked Git status · {} changed paths",
                    status.changes.len()
                ),
                status
                    .branch
                    .as_ref()
                    .map(|branch| format!("Branch: {branch}")),
            );
            Ok(status)
        })
        .await
    }

    #[tool(
        description = "Read a bounded Git diff for the approved repository. Set staged=true for the index/staged diff or false for unstaged working-tree changes. External diff and textconv execution are disabled.",
        annotations(read_only_hint = true)
    )]
    async fn git_diff(
        &self,
        Parameters(params): Parameters<GitDiffParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let diff = git::diff(&workspace, params.staged)?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Git,
                "diff",
                if params.staged {
                    "Inspected staged Git diff"
                } else {
                    "Inspected working-tree Git diff"
                },
                Some(format!(
                    "{} chars{}",
                    diff.content.chars().count(),
                    if diff.truncated { " · truncated" } else { "" }
                )),
            );
            Ok(diff)
        })
        .await
    }

    #[tool(
        description = "List recent local Git commits for an approved repository without exposing author email addresses.",
        annotations(read_only_hint = true)
    )]
    async fn git_log(
        &self,
        Parameters(params): Parameters<GitLogParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let commits = git::recent_commits(&workspace, params.limit.unwrap_or(12))?;
            record_observation(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                ActivityKind::Git,
                "log",
                format!("Inspected Git log · {} commits", commits.len()),
                None,
            );
            Ok(commits)
        })
        .await
    }

    #[tool(
        description = "Stage 1 to 100 explicit workspace-relative files. RepoTunnel blocks protected paths, symlinks, directories, and files that use Git clean filters because filters may execute external programs. In AI Auto the validated staging action applies immediately. In AI Review it is queued for local Accept/Reject and MCP cannot approve it."
    )]
    async fn request_git_stage(
        &self,
        Parameters(params): Parameters<GitStageParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let action = git::request_stage(&app, &workspace, params.paths)?;
            let _ =
                activity::record_git_record(&app, &workspace, trace_group_id.as_deref(), &action);
            Ok(action)
        })
        .await
    }

    #[tool(
        description = "Commit the repository's currently staged changes. RepoTunnel records the exact staged diff and HEAD first. In AI Auto the validated commit applies immediately. In AI Review it is queued for local Accept/Reject and MCP cannot approve it. This tool never stages files automatically."
    )]
    async fn request_git_commit(
        &self,
        Parameters(params): Parameters<GitCommitParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let action = git::request_commit(&app, &workspace, params.message)?;
            let _ =
                activity::record_git_record(&app, &workspace, trace_group_id.as_deref(), &action);
            Ok(action)
        })
        .await
    }

    #[tool(
        description = "Request restoration of one tracked UTF-8 text file to its HEAD version through RepoTunnel's normal safe-editing/history layer. In AI Auto the validated restore applies immediately; in AI Review it waits for local Accept/Reject. Staged or conflicted changes are not silently discarded."
    )]
    async fn request_git_restore_file(
        &self,
        Parameters(params): Parameters<GitRestoreParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        let trace_group_id = request_edit_group_id(&parts);
        run_filesystem_task(move || {
            let workspace = approved_workspace(&app, &params.workspace_id)?;
            let outcome = git::request_restore_file(
                &app,
                &workspace,
                params.relative_path,
                trace_group_id.as_deref(),
            )?;
            let _ = activity::record_change_outcome(
                &app,
                &workspace,
                trace_group_id.as_deref(),
                &outcome,
            );
            Ok(outcome)
        })
        .await
    }

    #[tool(
        description = "List recent Git commit requests and their pending/applied/rejected/failed status. This tool cannot approve or reject pending Git actions.",
        annotations(read_only_hint = true)
    )]
    async fn list_git_history(
        &self,
        Parameters(params): Parameters<ListGitActionsParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            git::list_actions(
                &app,
                params.workspace_id.as_deref(),
                params.limit.unwrap_or(30),
            )
        })
        .await
    }

    #[tool(
        description = "List recent RepoTunnel change records and their pending/applied/rejected/undone/failed status. This is read-only and does not approve, reject, or undo changes.",
        annotations(read_only_hint = true)
    )]
    async fn list_change_history(
        &self,
        Parameters(params): Parameters<ListChangesParams>,
    ) -> Result<CallToolResult, McpError> {
        let app = self.app.clone();
        run_filesystem_task(move || {
            ensure_ai_access(&app)?;
            changes::list_changes(
                &app,
                params.workspace_id.as_deref(),
                params.limit.unwrap_or(30),
            )
        })
        .await
    }
}

#[tool_handler]
impl ServerHandler for RepoTunnelMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("repotunnel", env!("CARGO_PKG_VERSION")))
            .with_instructions(SERVER_INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    use super::{trace_edit_group_id, RepoTunnelMcp};

    #[test]
    fn derives_edit_group_from_traceparent() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-6a834f2e00000000a27259e7454a1be9-0123456789abcdef-00"
                .parse()
                .expect("valid header"),
        );

        assert_eq!(
            trace_edit_group_id(&headers).as_deref(),
            Some("trace-6a834f2e00000000a27259e7454a1be9")
        );
    }

    #[test]
    fn rejects_malformed_traceparent_for_grouping() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-00000000000000000000000000000000-0123456789abcdef-00"
                .parse()
                .expect("valid header"),
        );

        assert_eq!(trace_edit_group_id(&headers), None);
    }

    #[test]
    fn project_memory_update_declares_closed_world_safety_metadata() {
        let tools = RepoTunnelMcp::tool_router().list_all();
        let tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "update_project_memory")
            .expect("project-memory update tool");
        let annotations = tool.annotations.as_ref().expect("tool annotations");

        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(true));
        assert_eq!(annotations.idempotent_hint, Some(false));
        assert_eq!(annotations.open_world_hint, Some(false));
    }
}
