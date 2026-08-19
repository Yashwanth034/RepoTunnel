# MCP contract

RepoTunnel exposes one stable MCP tool surface over the managed public connection. The tool contract is a compatibility boundary: internal implementation, storage, UI, and activity-history changes should not rename tools or change required parameter meaning unless a deliberate connector migration is planned.

## Workspace and files

Existing workspace inspection, safe file editing, change history, sandbox verification, and Git tools remain compatible with earlier RepoTunnel builds.

## Live terminal and processes

The live execution contract remains:

- `run_terminal_command`
- `list_terminal_history`
- `start_process`
- `list_processes`
- `read_process_output`
- `stop_process`
- `restart_process`

One-shot commands use the real approved workspace and host environment. Persistent processes are used for development servers, watchers, workers, and other commands that must survive later MCP calls.

## Application launching

The launcher contract is intentionally compact:

- `list_launchable_applications`
- `launch_target`
- `list_launch_history`

`launch_target` accepts one of three stable kinds: `url`, `workspace_path`, or `application`. URL and workspace-path targets may optionally select an application returned by `list_launchable_applications`.

## Browser automation

The browser contract uses one mutation tool and separate observation tools:

- `list_automation_browsers`
- `get_browser_status`
- `list_browser_tabs`
- `browser_action`
- `browser_inspect_page`
- `browser_take_screenshot`
- `get_browser_diagnostics`
- `list_browser_history`

`browser_action` supports the stable actions `start`, `stop`, `open_tab`, `activate_tab`, `close_tab`, `navigate`, `click`, `type`, `scroll`, and `reload`.

Browser screenshots are returned as MCP image content. The managed browser uses RepoTunnel's isolated automation profile rather than the user's ordinary browser profile.

## Monitoring

The monitoring contract is:

- `get_monitoring_status`
- `set_workspace_monitoring`
- `get_monitoring_snapshot`
- `list_monitoring_file_events`

`get_monitoring_snapshot` is the preferred compact observation tool after starting a development workflow because it combines managed process state/output tails, listening ports and dev-server correlation, recent terminal results, browser state and diagnostics, and recent project-file changes.


## Team Mode compatibility expansion

Team Mode was RepoTunnel's first deliberate MCP contract expansion after the 48-tool workspace/runtime surface was frozen. The original 48 tool names and parameter meanings remain unchanged. Team Mode adds two coordination tools, and the repository/external-file workflow adds two narrowly scoped workspace-bootstrap/security tools, bringing the public surface to **52 tools**:

- `team_status` — read the persistent shared two-agent session, including goal, success criteria, roles, task ownership, review handoffs, discussion, file/folder claims, phase, per-criterion verification evidence, and progress. It also supports a bounded long-poll (`after_revision` + `wait_seconds`, maximum 30 seconds) so an active agent can wait for the other AI to update shared state without immediately ending its coordination turn.
- `team_action` — the single agent mutation surface for creating/joining a persistent A/B team, heartbeat, shared messages, distinct task creation/claim/update, explicit task handoff, success-criterion verification evidence, task-scoped path claims, phase changes, and completion of the **current work request**. Completing a request keeps the same Team active. New human work received in either AI chat is registered in the same Team by posting a decision message beginning `USER REQUEST:`. Team pause/end remain user-controlled desktop actions.
- `clone_repository` — clone a human-supplied GitHub repository into `~/Projects` and register/reuse that checkout as an approved workspace without exposing GitHub credentials to the AI.
- `request_external_file` — require the user to choose one external file through RepoTunnel's native picker, then either read bounded UTF-8 content once or import the selected file into an explicit workspace-relative destination. The AI cannot nominate or browse an arbitrary host path.

Team Mode coordination state is stored by RepoTunnel outside the project folder; it does not create `PLAN.md`, `STATUS.md`, or other coordination files inside the user's repository. While Team Mode is active/paused, RepoTunnel's normal MCP file mutation tools require the caller to be bound to a joined team role. During active work, file mutations additionally require the caller to own an in-progress task and hold matching task-scoped path claims; this prevents a reviewer/supporting AI from silently duplicating the owner's implementation. Explicit `handoff_task` transfers ownership when needed. Shell commands and browser side effects cannot be path-analyzed reliably, so the agents must still respect claims when using those tools. Normal workspace security, AI Auto/AI Review behavior, history and global Pause AI protections remain in force.

Because adding tools changes MCP discovery, clients that cache an older RepoTunnel tool snapshot may need their RepoTunnel app/connector refreshed or recreated once to discover the 52-tool surface.

## AI mode behavior

AI Auto is intentionally non-interruptive. File changes, real terminal commands, managed process starts, application launches, and browser mutations execute without local confirmation when the project is configured for AI Auto.

AI Review may queue mutating actions for local Accept/Reject. MCP cannot approve its own queued Review actions.

Monitoring and other observation tools do not require approval.

Pause AI is the emergency master stop. MCP tools must stop when AI access is paused, and RepoTunnel stops managed active execution/browser work through its existing emergency-stop path.

## Compatibility rule

New internal capabilities should first be composed behind the existing tools when practical. Adding or changing MCP tools should be treated as an explicit compatibility event, not as a routine implementation detail.

## Unified AI activity history

RepoTunnel groups MCP activity internally by the existing `traceparent` request trace. File inspection/changes, terminal and verification commands, managed processes, launcher/browser work, Git actions, monitoring observations, and monitored project-file changes can therefore appear as one request-level activity in the desktop History UI.

This journal is an internal implementation detail and does not add an MCP tool or change any public MCP parameter schema. File-changing request groups link to the existing version record so Previous/Next/Restore remain version-driven; observation-only or execution-only requests are visible without creating fake restore points.
