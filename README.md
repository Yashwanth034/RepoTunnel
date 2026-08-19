# RepoTunnel

RepoTunnel is a local-first desktop MCP gateway that lets AI clients work with user-approved code repositories without giving them unrestricted filesystem access.

## Product goals

- Let users explicitly register local repositories.
- Keep privileged local operations in Rust.
- Expose focused read and write operations through Model Context Protocol (MCP).
- Prevent path traversal and access outside approved roots.
- Make changes reviewable, auditable, and reversible.
- Provide both isolated verification and intentional live developer-machine control with visible policy, history, and emergency-stop boundaries.

## Technology

- Tauri 2
- React
- TypeScript
- Rust
- Model Context Protocol (MCP)
- Official Rust MCP SDK (`rmcp`)

## Development

```bash
npm install
npm run tauri dev
```

Tauri development also requires the Rust toolchain and Linux system prerequisites. The current MCP SDK dependency requires Rust 1.88 or newer.

## Current capabilities

The desktop application currently provides:

- native local project-folder selection
- persistent workspace registration with per-project read/write and change-review policies
- project removal without modifying project contents
- a real loopback-only MCP gateway lifecycle
- a local `GET /health` endpoint while the gateway is running
- a Streamable HTTP MCP endpoint at `http://127.0.0.1:<port>/mcp`
- OpenAI Secure MCP Tunnel lifecycle integration for private ChatGPT connectivity
- automatic discovery of the official `tunnel-client` on common Linux paths
- session-only Runtime API key handling with no key persistence or command-line exposure
- tunnel readiness monitoring through the official `tunnel-client health` probe
- MCP tool discovery and invocation for approved workspace operations
- persistent two-AI Team Mode with stable A/B identities that join once and stay attached until the user ends the Team; repeated user requests can be given in either existing AI chat and flow through the same Team with non-duplicate task ownership, handoffs, cross-review, verification, file claims, progress tracking, and restart recovery
- backend-enforced workspace path validation
- bounded directory listing and text-file reads
- bounded project text search with `.gitignore` / `.ignore` awareness
- smart project indexing with generated-folder filtering, binary/large-file classification, language detection, and manifest discovery
- create and update text files through the safe-editing layer
- exact-context file patching with preview generation
- create folders
- rename and move files or folders without overwriting destinations
- delete files or folders through an explicit destructive operation
- per-project Review changes / Auto apply write policy
- persistent change history with pending/applied/rejected/undone/failed states
- local diff previews for text edits
- local approval/rejection for review-mode changes
- automatic undo points for supported file and structural edits
- stale-file checks before approved writes and undo
- file metadata inspection
- protected credential-file blocking and symlink-escape protection
- loopback Host and Origin validation at the local HTTP boundary
- per-project Review / Auto / Off command policy
- Bubblewrap-gated command sandbox with an actual namespace capability probe
- discovered build/test/check/lint presets instead of arbitrary shell commands
- disposable project copies with network disabled and cleared child environments
- bounded command timeouts and stdout/stderr capture
- local command approval/rejection and persistent command history
- bounded Git status, diff, branch/upstream, and recent-commit inspection
- explicit Git staging requests with local approval, stale-file checks, protected-path blocking, and clean-filter rejection
- staged-only Git commit requests with local approval and stale-HEAD/staged-diff checks
- tracked text-file restore-to-HEAD requests routed through safe editing

MCP tools do not receive arbitrary local paths. They use a RepoTunnel workspace ID plus a workspace-relative path and execute through the same Rust filesystem/security modules used by the desktop application.

## MCP tools

RepoTunnel currently registers a stable MCP surface covering workspace/file operations, safe sandbox verification, real terminal/process control, structured application launching, browser automation, monitoring, Git, and two-agent Team Mode coordination. See `docs/mcp.md` for the compatibility contract and complete capability groups.

The `list_workspaces` result intentionally omits canonical absolute project paths. `inspect_project` returns a bounded filtered tree and project overview without exposing the workspace root path. See `docs/project-index.md` for filtering behavior. AI-facing operations use workspace IDs and relative paths instead.

## ChatGPT connection

RepoTunnel keeps `/mcp` on the loopback interface and uses the official OpenAI `tunnel-client` for outbound-only connectivity. Create an OpenAI Platform tunnel and Runtime API key, start the connection in RepoTunnel, then select the same tunnel when creating a Developer mode app in ChatGPT. See `docs/connection.md` for the complete flow.

## Command execution

RepoTunnel provides two intentionally separate execution paths. Safe verification presets run in a network-isolated disposable Bubblewrap copy. AI live terminal/process work runs against the real approved workspace with network access inside a second Bubblewrap boundary: the project is writable, but the user's general home/host filesystem and host secrets are not exposed. Narrow GitHub workflow commands and explicitly user-authorized normal `git push` can use the existing authenticated host tools without exposing their credential files. AI Auto executes compatible in-project actions without local confirmation; AI Review follows the project command policy. See `docs/commands.md`.

## Git integration

RepoTunnel provides repository inspection, explicit staging, staged-only commits, and conservative restore-to-HEAD without exposing unrestricted Git mutation. In AI Auto, validated in-project Git writes apply without approval interruptions; in AI Review they wait for local Accept/Reject. Remote push is separate and requires an explicit human instruction for the current work. See `docs/git.md`.


## Development workflow

RepoTunnel exposes a project-level workflow readiness check and supports an end-to-end AI development loop: preflight → inspect → edit → run/build → start or inspect dev servers → launch/test the browser → monitor errors and file changes → re-verify → review Git. See `docs/workflow.md` for the full contract. Two-AI collaboration is described in `docs/team-mode.md`.


## Production hardening

RepoTunnel records a small redacted runtime log in its application-data directory, rotates the log automatically, records Rust panic information without intentionally logging secrets, removes stale RepoTunnel tunnel-runtime temp files at startup, and performs a graceful shutdown of the managed local gateway and `tunnel-client` process.

The desktop **Runtime diagnostics** panel reports the installed version/architecture and whether Bubblewrap, Git, and OpenAI `tunnel-client` are available. Linux users can optionally enable an XDG **Launch at login** entry. Startup deliberately remains disconnected: the tunnel Runtime API key is session-only and is never persisted.

The Tauri webview now uses a restrictive Content Security Policy instead of a `null` development CSP. Linux release targets are Debian, RPM, and AppImage.

See `docs/release.md` for build/release instructions and `docs/acceptance.md` for the required final live acceptance test.


## Managed public connection

For normal ChatGPT use, RepoTunnel can create its own public HTTPS MCP endpoint with the embedded ngrok Rust SDK. Each user supplies their own ngrok authtoken once; RepoTunnel does not ship a developer token or shared public URL. The assigned endpoint is saved locally and reused on later launches so the ChatGPT connection normally does not need to be recreated after restarting RepoTunnel.
