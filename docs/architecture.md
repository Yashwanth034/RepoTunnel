# Architecture

## Overview

RepoTunnel separates remote transport, OAuth/MCP serving, the desktop UI, trusted local operations, and per-resource policy.

```text
ChatGPT / MCP-compatible AI client
   |
   | HTTPS + OAuth where applicable
   v
ngrok / Cloudflare / Direct HTTPS / optional OpenAI Secure MCP Tunnel
   |
   v
RepoTunnel authenticated MCP boundary
   |
   v
Loopback MCP gateway
   |
   v
MCP capability router
   |
   +----> Workspace registry + access guard
   |         +----> Safe editing/versioning ----> Approved repository
   |         +----> Large-project reads/index ----> Approved repository
   |         +----> Sandboxed verification ----> Disposable project copy
   |         +----> Live terminal/process supervisor ----> Approved repository
   |         +----> Git/GitHub manager ----> Approved repository metadata
   |
   +----> Managed browser runtime
   +----> Desktop / isolated AI Workspace
   +----> Android Phone runtime + capability broker
   +----> Video Intelligence / Video Production
   +----> Team Mode / Continuity / Project Memory
```

## Desktop application

The Tauri desktop application is responsible for:

- native workspace selection
- gateway/provider lifecycle and connection status
- workspace, Desktop, Phone, and AI-mode permission surfaces
- change review, version history, checkpoints, and undo/restore surfaces
- Git/GitHub status and validated repository workflows
- managed browser, AI Workspace, Video, Team Mode, Phone, and system settings surfaces
- security, diagnostics, update, and continuity information

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

`src-tauri/src/mcp_server.rs` registers the capability-oriented MCP surface for approved project, terminal/process, Git/GitHub, browser, desktop/AI Workspace, Video, Phone, Team Mode, media/temp-workspace, and continuity workflows. It does not turn those subsystems into unrestricted host APIs.

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

`src-tauri/src/project_index.rs` builds the code-focused project view. It applies nested `.gitignore` / `.ignore` rules, skips generated dependency/build directories, excludes symlink traversal, classifies likely binary and oversized files, and detects common source languages/manifests.

`src-tauri/src/large_project_read.rs` adds resumable, bounded access for projects that are too large for one response. It backs paged project inspection, paged directory listing, large text range reads, and incremental file search with cursors. Shared heavy-read gating prevents overlapping scans from overwhelming the process, and changed file/search state invalidates stale cursors.

Legacy bounded project/search tools continue to reuse the same smart traversal. The index/read layers are relevance/performance mechanisms; `src-tauri/src/access.rs` remains the security boundary for every candidate path.


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

## Live terminal and managed-process layer

`src-tauri/src/terminal.rs` owns real-workspace AI terminal commands and durable managed jobs. This path is separate from disposable verification presets.

One-shot commands and managed processes run through the platform's fail-closed AI sandbox, with sanitized environments and bounded/redacted output. Managed jobs have stable RepoTunnel IDs, persisted status/log metadata, incremental output reads, stop/restart support, and bounded literal output waiting through `wait_process`.

The durable supervisor survives ordinary MCP/UI reconnects. On Linux it also survives a complete RepoTunnel process restart: shutdown detaches the independent supervisor, startup reloads its persisted state, verifies the live managed child using Linux process identity, and re-adopts the existing process record/logs. The public capability remains false on Windows/macOS until equivalent recovered-process identity/control is implemented and validated natively.

## Managed browser and semantic interaction

`src-tauri/src/browser.rs` owns the managed Chromium-family runtime. RepoTunnel maintains isolated/persistent browser profiles according to the selected workflow, tracks tabs/downloads/network metadata, and returns an atomic navigation receipt that binds observation to the intended document generation.

The shared semantic layer turns browser and supported desktop accessibility trees into short-lived `eN` references. Mutations revalidate the reference/action/sensitive-field policy.

Selector and semantic click/type mutations (plus semantic sequences) now create a pre-dispatch mutation ID and persist a bounded mutation receipt with helper acknowledgement, redacted before/after URL, before/after document generation, and whether the document changed. If helper transport is lost after dispatch, RepoTunnel reconnects the helper for observation but **does not replay the mutation**; the browser action is persisted as `ambiguous` so a later AI can inspect current state instead of duplicating a potentially completed action.

## Desktop and AI Workspace

RepoTunnel's Desktop layer exposes explicit user-permitted application control and blocks control of RepoTunnel itself. Linux uses AT-SPI; Windows UIA and macOS AX adapters exist behind the same abstraction and require native-platform validation before native-runtime claims.

AI Workspace provides an isolated virtual desktop with per-application ownership/session controls so multiple AI-owned app sessions can coexist without taking over the user's ordinary desktop.

## Android Phone Access

`src-tauri/src/phone.rs` owns Phone discovery, transport selection, the persistent live video/control runtime, central Off/Limited/Full capability enforcement, files/apps/settings/shell/log/network operations, and semantic Phone Helper integration.

The AI-facing target is an opaque selected device identity. Wireless ADB is preferred with authorized USB fallback for the same device. Payment-sensitive foreground apps keep the helper Accessibility service enabled but block AI observation/control until the user leaves the app.

## Video Intelligence

`src-tauri/src/video.rs` owns optional media understanding as a separate product capability. It accepts public HTTP/HTTPS media URLs and workspace-relative local media, prefers existing captions, extracts bounded smart visual frames when needed, and prepares compact audio only when captions are unavailable. Work runs as cancellable background jobs so media processing never blocks the MCP gateway or desktop UI.

RepoTunnel reuses host `yt-dlp` and `ffmpeg` when present. Missing helpers can be provisioned privately under application data with HTTPS, fixed trusted download hosts, published SHA-256 verification, private file permissions, and no PATH/admin changes. Video results are cached under a bounded application-data cache; no media artifacts are written into the user's project.

MCP receives transcript text plus actual image/audio content for model grounding. Video Intelligence never interprets media instructions as execution authorization: installs, edits, browser actions, Git operations and desktop actions continue through their existing policy/security layers.

## Video Production

Video Production persists durable Video Projects with script/storyboard/scene/timeline state, recordings, generated assets, narration/subtitles, previews/renders, QA evidence, and license provenance. Rendering runs as durable jobs with content-state deduplication and final QA gates.

Tutorial/explainer generation can use deterministic HTML/CSS/GSAP frame capture or the native renderer. Story-animation routing can use RepoTunnel's native motion pipeline and supported installed engines such as Godot/Blender/Rhubarb without making those external engines mandatory.

## Team Mode and continuity

Team Mode stores persistent two-engineer coordination outside the project and enforces task ownership, path claims, cross-review, and evidence-based completion.

Continuity/Resume v2, factual activity history, Project Memory, and MCP App self-continuation provide recovery context without treating stale saved intent as current fact or claiming that RepoTunnel can detect every external chat interruption.

## Git integration layer

`src-tauri/src/git.rs` provides a fixed Git capability surface rather than accepting arbitrary Git arguments from MCP. Git integration is enabled only when the approved workspace root owns its `.git` directory and Git reports both the worktree root and metadata directory inside that same approved boundary.

Read operations expose bounded status, diff, branch/upstream, and recent-commit information. Protected credential paths are omitted from status and diff results, external diff/text-conversion execution is disabled, and author email addresses are not returned.

Staging accepts only explicit relative file paths, fingerprints their current index/worktree state, rejects symlinks and protected/secret-bearing paths, and refuses files with Git clean filters because those filters can execute external programs. Commit requests operate on staged changes only, capture the exact staged fingerprint and HEAD, disable hooks and GPG signing, and revalidate before execution. In AI Auto, validated stage/commit actions apply immediately; in AI Review they wait for local approval. MCP may request and inspect Git actions but cannot approve or reject pending Review actions.

Restore-to-HEAD is intentionally narrower than unrestricted `git restore`: RepoTunnel reads the HEAD version of one tracked UTF-8 text file and submits that content to the safe-editing layer, preserving normal diff, stale-file, backup, and undo behavior while respecting AI Auto versus AI Review.

Remote push remains separate from in-project autonomy. It requires a current explicit human push instruction, performs a final committed-tree secret preflight, and uses a narrowly parsed normal push path with local hooks disabled.

## Remote connection layer

RepoTunnel's raw MCP gateway remains private to the machine. Remote access is a separate transport/authentication layer rather than a reason to weaken the local Host/path/workspace boundary.

Current provider paths include:

- managed ngrok through the Rust SDK
- Cloudflare Tunnel through a user-configured `cloudflared` runtime
- Direct HTTPS through RepoTunnel's TLS/ACME reverse-proxy path
- the optional official OpenAI Secure MCP Tunnel integration

Public MCP paths use RepoTunnel's OAuth boundary where applicable, including dynamic-client/PKCE validation. Provider credentials and authorization state are kept outside project repositories and are never exposed as general AI tool inputs.

`src-tauri/src/connection.rs` retains the optional OpenAI `tunnel-client` runtime. Its Runtime API key is supplied through `CONTROL_PLANE_API_KEY`, never persisted by RepoTunnel, and never placed in process arguments. Tunnel readiness is determined through the official health mechanism rather than assuming a launched process is connected.

Direct HTTPS and Cloudflare use RepoTunnel's stable local origin path; Direct HTTPS exposes only the intended MCP/OAuth/health/certificate routes and strips client-controlled forwarding headers before proxying to the trusted local origin.


## Workflow readiness

The `workflow` module composes existing security, indexing, execution, and Git capabilities into a read-only project preflight. It does not create a second execution path. MCP and the desktop UI receive the same readiness report, while actual edits continue through `changes`, commands through `execution`, and repository actions through `git`.


## Request-aware version grouping

RepoTunnel keeps mutation tool schemas compatible with clients that reject direct edit-group arguments. For Streamable HTTP clients that send a valid W3C `traceparent` header, the gateway derives an internal request group from the trace ID and passes it only to the local versioning layer. The identifier is never required as a model-visible tool argument. Missing or malformed trace context falls back to separate protected versions.
