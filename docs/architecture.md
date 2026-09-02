# Architecture

## Overview

RepoTunnel separates the user interface, local MCP transport, trusted filesystem operations, and workspace policy.

```text
ChatGPT / AI client
   |
   | OpenAI-hosted MCP tunnel endpoint
   v
OpenAI Secure MCP Tunnel
   ^
   | outbound HTTPS
   |
tunnel-client on the local machine
   |
   | MCP Streamable HTTP over loopback
   v
Loopback MCP gateway
   |
   v
MCP tool router
   |
   v
Workspace registry + access policy
   |
   +----> Safe-editing / change manager ----> Local filesystem engine ----> Approved repository
   |
   +----> Command policy / preset manager ----> Native OS sandbox ----> Disposable project copy
   |
   +----> Git manager ----> Approved repository .git metadata
```

## Desktop application

The Tauri desktop application is responsible for:

- native workspace selection
- gateway lifecycle and connection status
- permission management
- change review, diff, history, and undo surfaces
- Git status, diff, staging, commit-review, and history surfaces
- security and diagnostic information

React and TypeScript are used for the interface. Rust owns privileged local operations, MCP serving, and persistent workspace metadata.

## Workspace registry

Registered workspaces are stored in the application data directory as local JSON metadata. A workspace record contains:

- an internal identifier
- display name
- canonical absolute path
- registration timestamp
- read-only or read-write access mode
- review or automatic write policy
- review, automatic, or disabled command policy

The canonical absolute path remains internal application state. AI-facing tools use workspace identifiers and workspace-relative paths.

## Local MCP gateway

`src-tauri/src/gateway.rs` owns the local HTTP boundary. When enabled it:

- binds only to `127.0.0.1`
- uses an operating-system-assigned local port
- exposes `GET /health`
- exposes MCP at `/mcp`
- uses Streamable HTTP through the official Rust MCP SDK
- rejects non-loopback Host headers
- rejects browser Origin headers that are not loopback origins
- supports graceful shutdown from the desktop application

The MCP transport is stateless for legacy protocol versions as well as the current MCP protocol so the service does not rely on per-session in-memory authorization state.

## MCP server

`src-tauri/src/mcp_server.rs` registers focused tools for workspace discovery and filesystem operations.

The protocol layer does not implement filesystem access itself. Every tool:

1. resolves the current workspace ID from the persisted approved workspace registry
2. delegates reads to `src-tauri/src/filesystem.rs` and writes to `src-tauri/src/changes.rs`
3. inherits `src-tauri/src/access.rs` validation
4. returns a bounded success or tool-level error result, including whether a write was applied or queued

Blocking filesystem operations run on Tokio blocking workers so project searches and reads do not block the MCP HTTP runtime.

The `list_workspaces` MCP tool returns only the workspace ID, display name, access mode, and change policy. It intentionally does not return the absolute local filesystem path.

## Workspace access guard

`src-tauri/src/access.rs` is the single backend boundary for workspace-relative filesystem paths. It validates operation intent, rejects unsafe path components and protected files, canonicalizes existing paths, and verifies that symlinks or existing ancestors remain under the approved workspace root.

Both Tauri commands and MCP tools must pass through this guard before touching project files.

## Project intelligence

`src-tauri/src/project_index.rs` builds the code-focused view used by the desktop project overview and MCP `inspect_project` tool. It applies nested `.gitignore` / `.ignore` rules, skips generated dependency/build directories, excludes symlink traversal, classifies likely binary and oversized files, detects common source languages and manifests, and caps returned tree size.

Broad text search reuses the same smart traversal instead of independently walking every file. The project index is a relevance/performance layer; `src-tauri/src/access.rs` remains the security boundary for every candidate path.


## Filesystem engine

`src-tauri/src/filesystem.rs` contains RepoTunnel's trusted local file-operation engine. Every public operation receives an approved workspace record plus a workspace-relative path and passes through the workspace access guard before touching the filesystem.

The engine supports directory listing, UTF-8 text reads, bounded text search, file and directory creation, full-file writes, exact-context patches, rename, move, delete, and metadata inspection.

Write replacement uses a temporary file in the same directory followed by an atomic rename on the Linux target. Existing file permissions are preserved. Write and destructive operations reject symbolic-link leaf targets rather than mutating through them.


## Safe-editing layer

`src-tauri/src/changes.rs` mediates every Tauri and MCP mutation. A workspace can use `review` mode, where the requested operation and diff are persisted for explicit local approval, or `automatic` mode, where the operation executes immediately but still creates history and an undo point when one can be made safely.

Pending requests and undo data are stored separately from the public change-history record in the application data directory. These files are written atomically and with owner-only permissions on Unix. Approvals revalidate the live workspace and compare text fingerprints before overwriting or deleting a file, preventing an old preview from silently replacing newer content.

Undo is conservative: text writes restore the prior content only if the current file still matches the applied change; created files are removed only when their contents still match; created directories are removed only when the normal non-recursive delete remains safe; rename/move operations reverse the path change only when the original destination is free; deleted UTF-8 files can be restored. Recursive directory deletions and binary deletions are recorded but do not claim a safe automatic undo point.

## Command execution layer

`src-tauri/src/execution.rs` owns controlled project command execution. It discovers a small set of build/test/check/lint presets from project manifests and never accepts a raw shell string from MCP. The command policy is independent from filesystem write policy.

Before execution RepoTunnel verifies that the native sandbox for the current OS is available and usable. Linux probes Bubblewrap namespace creation, Windows probes an AppContainer launch, and macOS probes the Seatbelt compatibility backend. RepoTunnel then prepares a disposable project copy, excludes protected/ignored paths, grants or mounts only the dependency/toolchain access required by that backend, sanitizes the child environment, disables networking for verification commands, bounds execution time and output, and deletes the temporary working tree afterward. Commands therefore cannot persist their incidental filesystem writes into the approved project.

Pending command requests are fingerprinted and revalidated before local approval. MCP can request and inspect commands but cannot approve or reject its own pending execution.


## Git integration layer

`src-tauri/src/git.rs` provides a fixed Git capability surface rather than accepting arbitrary Git arguments from MCP. Git integration is enabled only when the approved workspace root owns its `.git` directory and Git reports both the worktree root and metadata directory inside that same approved boundary.

Read operations expose bounded status, diff, branch/upstream, and recent-commit information. Protected credential paths are omitted from status and diff results, external diff/text-conversion execution is disabled, and author email addresses are not returned.

Staging accepts only explicit relative file paths, fingerprints their current index/worktree state, rejects symlinks and protected/secret-bearing paths, and refuses files with Git clean filters because those filters can execute external programs. Commit requests operate on staged changes only, capture the exact staged fingerprint and HEAD, disable hooks and GPG signing, and revalidate before execution. In AI Auto, validated stage/commit actions apply immediately; in AI Review they wait for local approval. MCP may request and inspect Git actions but cannot approve or reject pending Review actions.

Restore-to-HEAD is intentionally narrower than unrestricted `git restore`: RepoTunnel reads the HEAD version of one tracked UTF-8 text file and submits that content to the safe-editing layer, preserving normal diff, stale-file, backup, and undo behavior while respecting AI Auto versus AI Review.

Remote push remains separate from in-project autonomy. It requires a current explicit human push instruction, performs a final committed-tree secret preflight, and uses a narrowly parsed normal push path with local hooks disabled.

## ChatGPT connection layer

`src-tauri/src/connection.rs` manages the optional OpenAI Secure MCP Tunnel runtime used for ChatGPT connectivity. RepoTunnel detects the official `tunnel-client`, starts it as a child process, points it at the current loopback `/mcp` endpoint, monitors its readiness, and terminates it when the user disconnects or the local gateway stops.

The Runtime API key is supplied to the child process through `CONTROL_PLANE_API_KEY`; it is never persisted by RepoTunnel and is never placed in process arguments. The tunnel ID is validated against the OpenAI tunnel identifier format before launch.

`tunnel-client` receives an ephemeral loopback health listener and health URL file. RepoTunnel uses the official `tunnel-client health --url-file` probe instead of treating a launched process as proof of readiness.

## Remote connection boundary

The local MCP listener remains intentionally private. `tunnel-client` initiates outbound HTTPS to the OpenAI tunnel service, so ChatGPT connectivity does not require RepoTunnel to bind to `0.0.0.0`, open an inbound firewall port, or publish the local MCP URL.


## Workflow readiness

The `workflow` module composes existing security, indexing, execution, and Git capabilities into a read-only project preflight. It does not create a second execution path. MCP and the desktop UI receive the same readiness report, while actual edits continue through `changes`, commands through `execution`, and repository actions through `git`.


## Request-aware version grouping

RepoTunnel keeps mutation tool schemas compatible with clients that reject direct edit-group arguments. For Streamable HTTP clients that send a valid W3C `traceparent` header, the gateway derives an internal request group from the trace ID and passes it only to the local versioning layer. The identifier is never required as a model-visible tool argument. Missing or malformed trace context falls back to separate protected versions.
