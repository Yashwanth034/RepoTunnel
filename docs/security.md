# Security Model

## Default position

Access is denied unless a repository has been explicitly registered by the user.

## Workspace selection

Folder selection is handled through a dedicated Rust command using the native desktop dialog. The React frontend does not receive a general filesystem API or broad filesystem capability.

The selected path is canonicalized before it is stored. Duplicate canonical paths are rejected.

## Workspace boundary

Every approved workspace has a stable internal identifier and a canonical local root. MCP tools receive a workspace identifier and a workspace-relative path instead of arbitrary absolute paths.

Example:

```text
workspace_id: "workspace-123"
relative_path: "src/app.ts"
```

The backend resolves that request against the approved root and rejects anything that escapes it. If an AI needs a human-supplied file outside that root, it cannot provide or browse an arbitrary host path: `request_external_file` opens RepoTunnel's native file picker and access exists only for the exact file the user chooses. One-time reads return bounded UTF-8 content without the absolute path; imports copy the selected regular file into an explicit workspace-relative destination. Cancelling the picker denies access.

## Access modes

Each workspace can be switched between:

- `readWrite` — read and modification operations may proceed to the filesystem policy layer.
- `readOnly` — write operations are rejected by the Rust backend.

Changing this setting does not change operating-system permissions and never grants access outside the approved root.

## Path safety

The Rust access guard rejects:

- absolute paths
- `..` parent traversal
- root or platform prefix components
- symbolic links that resolve outside the approved workspace
- non-existing targets whose nearest existing ancestor resolves outside the workspace
- missing targets when an operation requires an existing path

The guard canonicalizes the workspace root and existing target or ancestor before approving access.

## Protected files

Common credential and private-key files are denied even when they exist inside an approved project. The protected set includes:

- `.env` and `.env.*`, except example/sample/template variants
- common SSH private-key filenames
- common credential JSON filenames
- package/tool credential files such as `.npmrc`, `.pypirc`, `.netrc`, `.git-credentials`, and `credentials.toml`
- `.pem`, `.key`, `.p12`, `.pfx`, `.jks`, and `.keystore` files

This policy is enforced in Rust and therefore applies to both desktop commands and MCP tools.

## MCP boundary

The local gateway binds only to `127.0.0.1` and exposes MCP through Streamable HTTP at `/mcp`.

Defense-in-depth controls at the HTTP boundary include:

- loopback-only socket binding
- loopback Host-header validation
- rejection of non-loopback browser Origin headers
- no absolute workspace paths in `list_workspaces` MCP results
- filesystem work delegated to the same backend policy used by Tauri commands
- structured filesystem tools remain confined to approved workspace roots; AI live terminal/process commands are also confined by the Bubblewrap project sandbox, with only narrow authenticated Git/GitHub passthrough operations outside it

The Host/Origin checks reduce the risk of a malicious website reaching a local MCP server through browser-based DNS rebinding or cross-origin requests. They do not replace workspace authorization.

## Revocation

Removing a workspace from RepoTunnel revokes its registered access. MCP tools resolve the workspace identifier from the current approved workspace registry for every operation, so a previously issued workspace ID stops working after removal.

## Writes and destructive actions

Write operations are treated as higher risk than reads. Every mutation now passes through the Rust safe-editing layer after the workspace access guard. Each workspace has an independent change policy:

- `review` — the write is persisted as a pending request and does not touch the project until the user approves it locally.
- `automatic` — the write applies immediately, while RepoTunnel still records history and creates a safe undo point when possible.

Text previews are generated from current file contents. Approval revalidates the target and checks a local content fingerprint before full-file replacement, targeted patching, or undo. This prevents stale reviewed content from silently replacing a newer file. MCP does not expose approve/reject/undo tools, so an AI cannot approve its own pending request through the same remote connection.

Pending request payloads and undo data are stored in RepoTunnel application data, not inside the user repository. They are written atomically and use owner-only file permissions on Unix. Recursive directory deletion and binary deletion may be non-undoable; the history record explicitly reports whether a safe undo point exists.

## Commands

RepoTunnel has two execution surfaces with different trust models.

The **disposable verification sandbox** is limited to discovered build/test/check/lint presets and has an independent per-workspace `review`, `automatic`, or `disabled` policy. It requires Bubblewrap and an actual namespace probe. Presets run with network disabled, stdin closed, the environment cleared, bounded time/output, and a disposable copy of the approved project. Protected secret paths and symlinks are omitted from that copy. Dependency/toolchain mounts are read-only and narrowed so credential files from the user's home directory are not exposed. Writes in this path are discarded with the temporary copy.

The **AI live terminal/process surface** now operates on the real approved project through an operating-system Bubblewrap boundary. The project is mounted read/write at `/workspace`, its `.git` metadata is hidden behind an isolated overlay so shell commands cannot mutate the real repository metadata or bypass RepoTunnel's Git stage/commit guards, the general home directory is absent, the environment is cleared, credential-like environment overrides are refused, and only narrow runtime/toolchain/system mounts required to execute normal developer tools are available. Network access remains available so project builds, package installation, development servers, and tests can work. If Bubblewrap is unavailable, RepoTunnel blocks the AI command instead of falling back to unrestricted host execution.

A very small authenticated host passthrough exists for allowlisted `gh workflow`/`gh run` operations and an explicitly user-authorized normal `git push`. Those operations are parsed as single commands rather than arbitrary shell strings, continue to run RepoTunnel secret preflight/redaction, and do not expose credential files to the AI shell. Local commands started directly by the human from RepoTunnel's desktop terminal remain user-controlled host commands.

In AI Auto, compatible AI terminal commands and managed process starts run without local confirmation by design, but the filesystem/secret/push boundaries remain mandatory. In AI Review, the project command policy may queue them for local Accept/Reject or disable them. RepoTunnel records terminal/process activity, redacts credential-like terminal/process output before returning it through MCP, and Pause AI terminates active RepoTunnel-managed execution/process groups as the emergency stop.

MCP cannot approve its own pending Review command/process requests.


## Git operations

RepoTunnel does not expose arbitrary Git arguments. Git support requires the repository worktree root and `.git` metadata directory to remain inside the approved workspace. Parent repositories and linked worktrees with metadata outside that boundary are rejected.

Git status/diff output is filtered so protected credential paths are not surfaced. Diffs disable external diff drivers and text conversion. Staging accepts explicit paths only, fingerprints the file/index state, rejects symlinks and Git clean-filtered files, and runs the internal secret guard before content enters the index. Commit requests use staged changes only, disable hooks and GPG signing, revalidate both HEAD and the staged fingerprint, and rescan staged blobs for credential-like material. AI-triggered `git push` commands receive a final committed-tree secret preflight and are refused unless the MCP call explicitly represents a current human instruction to push. AI Auto removes confirmation interruptions; it does not grant standing push permission.

In AI Auto, RepoTunnel's dedicated stage/commit operations apply immediately without a local approval interruption. In AI Review they remain pending for local Accept/Reject, and MCP cannot approve or reject its own pending Review actions. Raw AI `git add`/`git commit` through the live terminal are refused so those checks cannot be bypassed accidentally. Restore-to-HEAD requests remain forced-review safe-editing changes instead of exposing destructive `git reset`, `git clean`, or unrestricted restore commands.

## Filesystem operation limits

The local file engine applies limits before operations reach MCP clients:

- text reads are limited to 1 MiB per file
- file writes are limited to 2 MiB of UTF-8 content
- directory listings are limited to 1,000 accessible entries
- searches scan at most 10,000 files and return at most 200 matches
- search skips common generated/dependency directories and never follows symlinks
- protected files are omitted from recursive search and directory results
- exact-context patches fail if their expected text is missing or appears more than once
- rename and move never overwrite an existing destination
- workspace-root deletion or modification is rejected
- write, rename, move, and delete operations reject symlink leaf targets

These are defense-in-depth limits. User-facing review/approval controls and remote connection authentication remain separate layers.

## MCP action classification

Inspection tools advertise the MCP `readOnlyHint` annotation. Mutation tools intentionally do not, so clients can apply their normal write-confirmation policy. The `list_change_history`, `get_execution_status`, `list_command_presets`, `list_command_history`, `git_status`, `git_diff`, `git_log`, and `list_git_history` tools are read-only. Filesystem mutation, `run_command`, `request_git_stage`, `request_git_commit`, and `request_git_restore_file` are actions. Local change approval/rejection/undo, command approval/rejection, and Git approval/rejection are intentionally not exposed through MCP. RepoTunnel still enforces read-only/read-write workspace permissions and the review/automatic change policy independently in Rust.


## Secure MCP Tunnel credentials

RepoTunnel uses the official OpenAI `tunnel-client` as a separate transport process for ChatGPT connectivity. The local MCP endpoint remains bound to loopback.

The tunnel Runtime API key is treated as an ephemeral session secret:

- entered through a password field
- passed to `tunnel-client` only through the `CONTROL_PLANE_API_KEY` child-process environment
- never stored in the workspace registry or application settings
- never placed in process command-line arguments
- temporary tunnel health/log files are created with owner-only permissions on Unix systems
- cleared from the frontend input after a connection attempt

RepoTunnel does not request an OpenAI admin key. Tunnel creation/association remains an explicit OpenAI Platform operation.

The managed tunnel process receives an ephemeral loopback health listener. RepoTunnel reports the transport as ready only when the official health command reports readiness. Stopping the local MCP gateway first terminates the tunnel process so a remote tunnel cannot remain active against a missing local endpoint.


## Production runtime hardening

The packaged Tauri webview uses an explicit Content Security Policy rather than a `null` CSP. Production content is limited to the application itself plus Tauri IPC/asset protocols required by the UI. Development has a separate policy that permits the local Vite HMR websocket without weakening the packaged build. Tauri responses also set `X-Content-Type-Options: nosniff` and same-origin opener/resource policies.

RepoTunnel writes a bounded rotating runtime log under its application-data directory. New log files use owner-only permissions on Unix, common secret markers are redacted, individual log details are truncated, and source-file contents/API keys are never intentionally logged. Rust panic information is captured to that log for diagnosis before the normal panic hook runs.

Startup removes only RepoTunnel-owned temporary tunnel files that are old and not associated with a live Linux process. This avoids deleting files belonging to another active RepoTunnel instance. Normal application exit explicitly stops the managed Secure MCP Tunnel process and loopback gateway before shutdown.

Linux launch-at-login uses the user's XDG autostart directory and never stores tunnel credentials. For AppImage builds RepoTunnel records the original AppImage path rather than the temporary mounted executable path. RepoTunnel also refuses to write its autostart entry through a symbolic link.
