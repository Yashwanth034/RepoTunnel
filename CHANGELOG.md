
## Managed public connection

- Embedded the official ngrok Rust SDK so users no longer need to run a separate ngrok terminal or point RepoTunnel at the developer's tunnel.
- Added one-time per-user ngrok authtoken setup with owner-only local credential storage on Unix.
- Saves the user's assigned public HTTPS endpoint and requests the same domain on future launches, keeping the ChatGPT MCP URL stable across RepoTunnel restarts.
- Automatically starts the local gateway and saved public tunnel on launch after first-time setup.
- Automatically retries the managed public forwarder after recoverable interruptions while preserving the saved ChatGPT-facing domain.
- Added public connection health, stable MCP URL copy, remote request count, last-remote-request timestamp, restart, and reset controls.
- Kept the compatibility-safe MCP tool schema, AI Auto/Review behavior, and trace-based version grouping unchanged.
- OpenAI Secure Tunnel remains available as an optional advanced connection path.

## Unreleased
- Final acceptance polish: all transient app notices now use compact 3.8-second toasts; checkpoint rows fit narrow content areas; Settings refresh confirms completion; Safety Scan lists safe ignored/generated examples without exposing protected paths.

### MCP proposal compatibility

- Restored the proven proposal-style MCP file tools without `begin_edit_group` or `edit_group_id` arguments.
- Added transport-level request grouping: compatible clients such as ChatGPT can keep the old MCP tool schema while RepoTunnel groups same-request mutations internally using the validated `traceparent` trace ID.
- Restored project-level AI Review and AI Auto policies: Review queues a local proposal; Auto applies compatible edits immediately.
- Added a compact pending-change review section for projects that intentionally use Review mode.
- Preserved local version snapshots for both automatic edits and edits applied from Review mode.
- Kept the newer version-history, checkpoint, safety, project, connection, and Git protections intact.
- Groups compatible same-request MCP mutations into one version internally without adding model-visible grouping arguments.

### Final workspace polish

- Made History and checkpoint controls responsive to the actual main content width so they no longer crowd or overflow beside the project rail.
- Clarified that one AI request maps to one saved version containing all files changed by that request.
- Expanded Safety Scan explanations for ignored/generated content and the remaining local approval gates.
- Improved Connect with managed-client identity/count, explicit workspace-access state, and clearer direct-client limitations.
- Updated Help copy to match automatic immediate writes plus reversible version history instead of the retired per-file Apply/Reject flow.

### Home safety controls

- Removed duplicate project-picker controls from the persistent project rail while keeping the Home project picker and Projects-page add action.
- Added local project checkpoints for the AI-accessible project state.
- Added an on-demand safety scan covering workspace boundaries, secret protection, write policy, sandbox availability, Git protection, and pending reviews.
- Added persistent Pause/Resume AI access; while paused, MCP tools are blocked until the user resumes access locally.

### Desktop interface

- Replaced the dashboard-style interface with a flatter native-feeling desktop workspace shell.
- Added compact Home, Projects, Review, Checks, Git, Connect, and Settings navigation with approved projects visible directly in the sidebar.
- Rebuilt Home around a centered gateway launch surface and a lightweight recent-activity list instead of statistic cards and hero panels.
- Flattened project, review, connection, diagnostic, and tool views into compact rows, inspectors, and separators with substantially less card chrome.
- Kept change diffs collapsed by default and surfaced only file-level summaries until the user chooses to inspect details.
- Clarified that OpenAI tunnel-client is optional and that compatible HTTPS MCP bridges can also expose the local gateway.

# Changelog

## 0.1.0 - Release candidate

Initial RepoTunnel release candidate.

### Included

- Explicit local workspace registration and revocation
- Backend-enforced read-only/read-write permissions
- Protected secret/private-key filtering and path/symlink escape defenses
- Streamable HTTP MCP gateway on loopback
- OpenAI Secure MCP Tunnel process integration
- Safe-edit review, diff, history, backups, and undo
- Smart project indexing and filtered search
- Bubblewrap-isolated build/test/check/lint presets
- Controlled Git status/diff/log/stage/commit/restore workflow
- End-to-end workflow readiness checks
- Persistent redacted runtime logs and panic records
- Graceful managed gateway/tunnel shutdown
- Linux launch-at-login support
- Runtime diagnostics panel
- Debian, RPM, and AppImage release configuration
- Release and live acceptance documentation

### Known release gate

The final compiled Linux application and real ChatGPT tunnel workflow must pass `docs/acceptance.md` before this release candidate should be promoted as a generally available build.

## Workspace safety and recovery polish

- Added checkpoint management in Review with compare, restore, delete, and automatic pre-restore recovery checkpoints.
- Added Apply all / Reject all actions when multiple AI changes are pending.
- Added expandable Safety Scan details for workspace boundaries, secret protection, write policy, sandbox, Git, and pending review state.
- Added persistent top-bar visibility when AI workspace access is paused.
- Added project pinning, recent-project ordering, and project removal controls in the project rail.
- Added MCP endpoint copy support and clearer managed connection status.
- Removed inactive/dead overflow controls and the duplicate bottom-left gateway status while retaining Settings and Help.

## Automatic version history

- Normal AI file edits now apply immediately instead of waiting for per-file approval.
- RepoTunnel captures automatic before/after project versions and groups edits by an explicit AI request edit-group ID, so one AI request remains one history step even when file writes are far apart in time.
- History supports Previous, Next, restore-any-version, and Original-state restore.
- Restoring an older version preserves later versions for forward navigation and future branches.
- Version restore temporarily blocks MCP workspace access and creates a recovery checkpoint first.
- Git Restore to HEAD now asks for confirmation inside the Git page.
- Project write-policy UI now describes automatic versioned edits instead of Apply/Reject review.
