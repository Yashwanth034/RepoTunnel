# Product Definition

## Purpose

RepoTunnel is a local-first AI workspace gateway. It gives MCP-compatible AI clients controlled access to local software projects and selected device/application capabilities without granting unrestricted access to the computer.

The product is designed for end-to-end AI work: inspect a project, edit it safely, run verification, operate supported development/browser/desktop surfaces, review Git state, and continue long-running work while RepoTunnel enforces workspace, secret, approval, and platform boundaries.

## Primary workflow

1. The user opens RepoTunnel and adds or clones a project.
2. RepoTunnel canonicalizes and registers the project as an approved workspace.
3. The user chooses read-only/read-write access and AI Auto or AI Review behavior.
4. RepoTunnel starts its local MCP gateway on loopback.
5. The user chooses a supported remote connection path:
   - managed ngrok,
   - managed Cloudflare Tunnel,
   - Direct HTTPS through the user's own public network path, or
   - the optional OpenAI Secure MCP Tunnel integration.
6. Public MCP access is authenticated through RepoTunnel's OAuth boundary where applicable; the raw workspace gateway remains local.
7. The AI discovers approved workspaces through `list_workspaces` and checks `get_workflow_readiness`.
8. The AI inspects only the approved workspace and uses bounded, capability-specific tools for project, terminal, Git, browser, desktop, video, or Phone work.
9. RepoTunnel validates every operation against the current workspace/device/application policy.
10. Mutations either apply automatically in AI Auto or wait for local review in AI Review, depending on the relevant policy.
11. RepoTunnel records factual activity, change history/version information, and recovery state so later turns can verify what actually happened.

## Current capability groups

### Projects, files, and large repositories

- approved workspace registration and removal without deleting project contents
- read-only/read-write access
- bounded directory and text operations
- exact-context patching, create, rename, move, and delete
- protected-file and symlink-escape enforcement
- project indexing and smart search
- cursor-based large-project search, directory paging, project-tree paging, and large text range reads
- project context retrieval and Project Memory

### Safe editing and history

- AI Review and AI Auto
- pending diff review
- request-aware version grouping
- conservative undo and version restore
- stale-change protection
- checkpoints, history, and factual activity records

### Commands and processes

- sandboxed build/test/check presets in disposable project copies
- real-workspace one-shot terminal commands through the native OS sandbox
- durable managed development processes
- bounded incremental output reads
- process stop/restart and bounded output-pattern waiting
- workflow monitoring and runtime status

### Git and GitHub

- Git status, diff, log, branch/upstream state
- explicit validated staging and staged-only commits
- safe restore-to-HEAD through the editing layer
- secret scanning and protected-path enforcement
- normal push only when the current human instruction explicitly authorizes publishing
- use of the RepoTunnel-managed GitHub connection without exposing credentials to the AI

### Browser and desktop work

- managed Chromium-family browser automation
- persistent browser profiles/context, atomic navigation receipts, network history, downloads, uploads, screenshots, DOM inspection, and semantic interaction
- non-replayable mutation receipts for selector/semantic click and type operations, including explicit ambiguous completion when helper transport is lost after dispatch
- short-lived semantic refs and stale-ref protection
- real desktop control
- isolated multi-application AI Workspace
- RepoTunnel self-control protection

### Team Mode

- persistent two-engineer A/B teams
- planning/task ownership and path claims
- parallel non-overlapping implementation
- cross-review and success-criterion verification
- user-controlled pause/end behavior

### Video

- Video Intelligence for public media and approved local media
- durable Video Projects
- script/storyboard/timeline state
- recording, generated scenes, narration/subtitles, rendering, QA, preview, cleanup, and license provenance
- HTML/CSS/GSAP tutorial scene workflow
- story-animation routing with native motion and optional installed Godot/Blender/Rhubarb engines

### Android Phone Access

- wireless ADB pairing/reconnect with USB fallback
- Off/Limited/Full access and Pause AI
- persistent live screen/control
- apps, files, APK install, settings reads, shell, logs, and network diagnostics
- semantic Phone Helper integration
- payment-sensitive foreground blocking that leaves Accessibility enabled while denying AI inspection/control

### Connections, security, and lifecycle

- loopback-only raw MCP gateway
- OAuth/DCR/PKCE support for public MCP connections
- ngrok, Cloudflare Tunnel, Direct HTTPS, and optional OpenAI Secure MCP Tunnel paths
- Direct HTTPS TLS/ACME support
- dependency/security checks and cross-platform release workflow
- signed auto-update metadata with platform-specific install safety gates
- Continuity/Resume v2, Project Memory, factual activity history, and MCP App self-continuation support

## Current known limitations

These are product boundaries, not implied failures:

- full managed-process reattachment across a complete RepoTunnel process restart is currently advertised on Linux only; Windows/macOS remain intentionally false until equivalent recovered-process identity/control is implemented and validated natively
- browser response-body capture, WebSocket-frame capture, and a navigation scope allowlist are not currently exposed
- Windows/macOS semantic adapters and platform packaging must be validated on their native operating systems; Linux is the current live development host
- Android permissions/device policy can deny some operations even under Full Phone Access; settings write/delete availability is learned from the actual device
- payment-sensitive apps intentionally block AI Phone inspection/control while foreground
- external services, operating-system policy, and third-party application behavior remain authoritative outside RepoTunnel's own boundary

## Non-goals

RepoTunnel does not:

- expose the whole computer by default
- silently expand workspace, Phone, browser, or desktop permissions
- bypass operating-system or application security
- expose the raw local MCP gateway directly to the Internet
- treat arbitrary host shell access as equivalent to approved project access
- allow remote MCP calls to self-approve local Review actions
- silently publish Git history, releases, files, videos, or other external artifacts
- claim completion without a corresponding verified RepoTunnel result
