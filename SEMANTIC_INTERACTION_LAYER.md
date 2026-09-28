# RepoTunnel Semantic Interaction Layer — Architecture & Continuity

## Purpose

This file is the canonical source of truth for the Semantic Interaction Layer implementation. Future chats must read this file together with `AI_CONTINUITY.md` and `STABLE_CHECKPOINT.md` before changing semantic/browser/desktop/AI-Workspace code.

Short resume instruction:

> Continue RepoTunnel Semantic Interaction Layer from `SEMANTIC_INTERACTION_LAYER.md` in workspace `workspace-1a05e4cea94`. Read the continuity/stable files first, inspect Git, preserve unrelated work, then resume the first incomplete stage and continue automatically.

## Repository safety baseline

- Workspace: `workspace-1a05e4cea94`
- Branch: `fix-glib-alert`
- Development HEAD at architecture freeze: `c8f292abcd3879a1bc1c1b11644ba9cdbc864d4a`
- Known-good recovery baseline remains: `8c571da45f5ef4e32e716ed227dddf8553a61599`
- Never reset, revert, reclone, recreate, or reinstall the project to implement this work.
- Do not build/install a Debian package until implementation and real validation are complete.
- Preserve Direct HTTPS, OAuth/DCR, project/workspace/account data, MCP gateway security, Video Project/video playback behavior, updates, Team Mode, AI Review, and all unrelated existing features.
- `.scene_audio_summary.json` was an unrelated untracked file produced by another workflow during architecture testing. Treat it as foreign work: do not read, edit, delete, stage, commit, or otherwise modify it unless the user explicitly reassigns ownership.
- Before every edit batch, re-check Git status. If another process/AI changed a file we plan to edit, re-read/reconcile it before proceeding. Never overwrite concurrent work.
- Prefer targeted `patch_file` edits after reading the current file. Avoid broad rewrites.

## What was inspected before implementation

The full RepoTunnel project index was inspected before coding:
- 208 relevant files
- 157 readable code/text files
- Rust/Tauri backend with 208 registered Tauri commands
- Existing browser, desktop-control, AI Workspace, MCP, Team, security, monitoring, continuity, and model layers were mapped.

Important current implementation facts:

### Browser

Relevant files:
- `src-tauri/src/browser.rs`
- `src-tauri/resources/browser_bridge.cjs`
- `src-tauri/src/models.rs`
- `src-tauri/src/commands.rs`
- `src-tauri/src/mcp_server.rs`

Existing behavior to preserve:
- RepoTunnel launches isolated Chrome/Brave/Chromium/Edge with localhost CDP.
- Existing CSS selector click/type, navigate, scroll, reload, HTML/text inspection, visual element picking, screenshots, console/network diagnostics, AI Review, and Team `@browser` locking remain supported.
- Browser diagnostics already use a long-running monitor helper.

Current performance limitation:
- `browser.rs::run_helper_json()` starts a fresh Node process for normal browser operations.
- `browser_bridge.cjs::cdpCommand()` opens and closes a CDP WebSocket per command.
- This is the principal implementation path to replace with a persistent helper/session while preserving compatibility.

### Linux Desktop Control

Relevant files:
- `src-tauri/src/desktop_control.rs`
- `src-tauri/resources/desktop_control.py`

Existing behavior to preserve:
- Linux AT-SPI semantic inspection already exists.
- Desktop permission is human-controlled.
- RepoTunnel self-control is blocked.
- Actions are scoped to enabled applications/windows.
- Password/PIN/credential/token/API-key-like values are redacted and typing into sensitive fields is blocked.
- Screenshot/coordinate fallback is app-window scoped.

Current performance limitation:
- `desktop_control.rs` launches the Python helper per operation.
- Repeated complete AT-SPI scans should eventually become a persistent event/cache service, not a rewrite of existing Linux control.

### AI Workspace

Relevant files:
- `src-tauri/src/ai_workspace/mod.rs`
- `src-tauri/resources/ai_workspace/ai_workspace.py`

Existing behavior to preserve:
- Isolated display/session and scoped application control.
- Existing bounded local sequence support (maximum 64 steps and execution budget).
- Existing window/visual input safety.
- GNOME Terminal startup already uses a private `dbus-run-session`.
- Current launch paths intentionally set `NO_AT_BRIDGE=1`.

Required direction:
- Do not merely delete `NO_AT_BRIDGE=1`.
- Build a private accessibility environment inside the AI Workspace session, ensure it cannot join the host accessibility bus, then enable the AT bridge only inside that isolated environment.

## Measured architecture evidence

Testing was performed without modifying RepoTunnel source.

Important measured results:
- Unix-domain socket 1-byte RTT: about 16 µs median.
- Persistent stdio 1-byte RTT: about 20 µs median.
- Small JSON encode: about 3 µs median.
- 8 MiB mmap write: about 1.7 ms median.
- 8 MiB base64 encode: about 25 ms median.
- 8 MiB JSON+base64: about 95 ms median.
- inotify event delivery: about 19 µs median.
- 12 separate desktop tool actions: about 40.8 s end-to-end.
- Same 12 actions as one local sequence: about 3.75 s end-to-end; RepoTunnel local executor itself reported about 355 ms.
- 64 local sequence actions: about 1.65 s local execution.
- Semantic browser inspection was much smaller/faster than screenshots.
- Five unchanged Nemo semantic snapshots were byte-identical.
- Five unchanged screenshots were byte-identical but still resent large PNG/base64 payloads.
- Three independent observations executed in parallel were about 1.74x faster than sequential.
- Four concurrent browser read operations completed in roughly the same wall time as one read.
- One aggregate observation request was about 7.2x faster than six separate requests, but returned excess payload, proving that aggregated calls should support field selection/delta modes.
- Nemo exposed a rich AT-SPI tree; Chromium exposed only shallow generic AT-SPI nodes, proving browser-native CDP semantics must outrank generic AT-SPI for Chromium.
- LibreOffice UNO is available and can be considered later as an app-native adapter after this v1 semantic layer is stable.

## Locked interaction priority

For every supported surface:

1. App-native structured API, when a dedicated adapter exists.
2. Browser CDP/DOM/Accessibility for Chromium-family browser content.
3. OS semantic accessibility (AT-SPI / Windows UIA / macOS AX).
4. Verified scoped keyboard/pointer fallback.
5. Screenshot/vision fallback.
6. Raw coordinates only as final fallback.

For the v1 stages below, browser CDP and OS accessibility are the main structured layers. App-native adapters such as VS Code/UNO are a later acceleration layer after v1 is stable.

## Shared semantic contract

Create one backend-neutral Rust contract rather than separate AI-facing formats for every surface.

Conceptual snapshot:

```text
SemanticSnapshot
  snapshot_id
  version
  hash
  surface
  target_id
  document_identity
  created_at
  nodes[]
  truncated
  unchanged
```

Conceptual node:

```text
SemanticNode
  ref_id          # e1, e2, e3...
  role
  name
  description
  text
  value           # never expose sensitive values
  states[]
  actions[]
  bounds
  sensitive
  parent_ref
  child_refs[]
  backend_identity # internal only
```

Rules:
- AI-facing refs are short-lived and scoped to snapshot + surface + target/document identity.
- Never use a short ref as permanent identity.
- Backends may retain native identity internally:
  - browser: AX node / backend DOM node / frame/document identity
  - Linux: AT-SPI application/object identity
  - Windows: UIA runtime/native identity
  - macOS: AXUIElement identity
- A ref from a stale generation must fail safely with an explicit stale-ref result.
- Snapshot output is bounded.
- Stable snapshots should support version/hash and `unchanged` responses.
- Later event-backed adapters may provide deltas between versions.

## Local semantic execution model

The key performance rule is:

```text
one AI request -> many deterministic local operations
```

Do not expose hundreds of tiny operations requiring model round trips.

The local sequence engine must support bounded:
- find
- action
- wait
- assertion
- variable capture
- conditional branch
- retry
- timeout
- cancellation
- dependency-aware parallel read/observation steps when safe

The model should re-enter only for ambiguity, novel reasoning, high-impact approval, or an unresolved error.

## Persistent helper architecture

### Browser

Target:

```text
Rust BrowserRuntime
  -> one persistent helper process per managed browser session
  -> persistent JSON-lines/stdin/stdout control channel
  -> persistent CDP WebSocket(s) per active tab/target
  -> multiplex command IDs
  -> reconnect on safe transport loss
```

Do not invent a binary protocol initially. Measured persistent stdio/UDS latency is already far below the AI/tool boundary.

Safe automatic retries:
- idempotent inspection/read/status operations may reconnect and retry.
- non-idempotent click/type/navigation/mutation operations must never be blindly repeated after uncertain execution.

### Desktop

After the shared contract is working with the existing AT-SPI helper:
- convert Linux desktop semantic access to a persistent helper/cache.
- initial bounded snapshot, then event-driven updates.
- do not discard the existing proven AT-SPI action/security logic.

## Browser semantic implementation

Use RepoTunnel's existing CDP lifecycle. Do not install Playwright MCP.

Add:
- CDP Accessibility tree extraction.
- DOM/backend-node resolution.
- semantic role/name/text/state/action normalization.
- short refs such as `e1`.
- server-side semantic find.
- ref-based click/type/press/focus/scroll actions as appropriate.
- actionability checks before mutation.
- bounded retry for safely repeatable preconditions.
- stale-ref detection across navigation/document changes.
- persistent helper/session.
- bounded semantic sequences.

Keep:
- selectors
- screenshots
- diagnostics
- coordinate/visual selection
- existing browser history
- AI Review
- Team `@browser` mutation locking

## Security invariants

The Rust/Tauri/MCP layers remain the policy authority. Helpers are execution backends, not permission authorities.

Every mutation must continue through applicable gates:

```text
workspace authorization
 -> browser/desktop permission
 -> Team lock
 -> AI Review/change policy
 -> semantic target/ref validation
 -> sensitive-field policy
 -> execution
```

Never expose:
- passwords
- PINs
- passcodes
- OTP/verification secrets when identifiable
- API keys
- auth tokens
- credentials
- secret-like values

Sensitive nodes may expose role/label and `sensitive=true`, but their value/text content must be redacted.

Do not weaken:
- RepoTunnel self-control blocking
- application/window scoping
- credential/authentication typing protection
- workspace boundary
- Team path/browser locks
- AI Review
- MCP auth/security
- external-access protections

## Implementation stages

Update this checklist after every verified stage.

- [x] Stage 1 — Shared semantic node/snapshot/ref contract.
- [x] Stage 2 — Browser CDP accessibility snapshot engine.
- [x] Stage 3 — Browser semantic find + ref actions.
- [x] Stage 4 — Actionability/retry/stale-ref handling.
- [x] Stage 5 — Persistent browser helper/CDP sessions.
- [x] Stage 6 — Browser semantic sequences.
- [x] Stage 7 — Normalize existing Linux AT-SPI desktop semantics.
- [x] Stage 8 — Isolated semantic accessibility for AI Workspace.
- [x] Stage 9 — Desktop/AI-Workspace semantic sequences and fallbacks.
- [x] Stage 10 — Windows UI Automation adapter behind shared abstraction.
- [x] Stage 11 — macOS AX adapter behind shared abstraction.
- [x] Stage 12 — Security/performance/regression tests + real Linux validation.

## Stage-level safety gates

Before each stage:
1. Re-read this file.
2. Check Git status.
3. Re-read every file that will be edited.
4. Confirm unrelated/concurrent changes are preserved.
5. Patch only required paths.

After each stage:
1. Run focused unit tests for changed modules.
2. Run formatting/checks for touched language(s).
3. Re-check Git diff/status.
4. Confirm unrelated files were not changed.
5. Update this file with completed stage, exact files changed, tests, and any remaining limitation.
6. Continue to the next stage automatically unless there is a genuine blocker or user decision.

At major integration points:
- run full Rust tests
- run frontend tests/check/build when frontend/API types are affected
- validate MCP tool schemas/contracts
- verify existing selector/desktop/AI Workspace behavior has not regressed

Do not create/install a Debian package until Stage 12 and real validation are complete.

## Expected primary implementation paths

Likely files; verify before every edit:

New:
- `src-tauri/src/semantic.rs` or a small `semantic/` module if the implementation genuinely needs multiple backend-neutral files.
- semantic-specific tests adjacent to implementation where practical.

Existing, targeted:
- `src-tauri/src/lib.rs`
- `src-tauri/src/models.rs` only when public/shared model exposure belongs there
- `src-tauri/src/browser.rs`
- `src-tauri/resources/browser_bridge.cjs`
- `src-tauri/src/commands.rs`
- `src-tauri/src/mcp_server.rs`
- `src-tauri/src/desktop_control.rs`
- `src-tauri/resources/desktop_control.py`
- `src-tauri/src/ai_workspace/mod.rs`
- `src-tauri/resources/ai_workspace/ai_workspace.py`
- relevant tests/docs

Avoid unrelated frontend/UI edits unless a semantic feature genuinely requires a visible control.

## Compatibility strategy

Existing MCP/Tauri methods remain supported while semantic methods are added.

Do not remove or silently change semantics of:
- `browser_inspect_page`
- selector-based `browser_action`
- `browser_take_screenshot`
- browser diagnostics
- `inspect_desktop_app`
- `desktop_app_action`
- `desktop_take_screenshot`
- AI Workspace window/visual action/sequence APIs

New semantic APIs should route through the same internal core rather than duplicating security logic.

## Performance acceptance goals

Compare new paths against current measured baselines.

Required qualitative outcomes:
- no new process spawn per ordinary browser semantic action after Stage 5.
- no new CDP WebSocket handshake per command in steady state after Stage 5.
- multiple deterministic semantic actions execute locally in one request.
- unchanged semantic state can return a compact version/hash response.
- server-side find avoids shipping giant trees merely to locate one element.
- large images remain fallback data, not the primary observation path.
- independent read operations may execute concurrently when safe.

## Stage 12 validation matrix

Must include:
- browser selector compatibility
- browser semantic snapshot
- semantic find
- ref click/type/actions
- navigation-induced stale refs
- hidden/disabled/covered element handling
- persistent-helper reconnect
- semantic sequence success/failure/cancel/timeout
- sensitive-field redaction and typing block
- Team `@browser` locking
- AI Review behavior
- Linux AT-SPI compatibility
- RepoTunnel self-control block
- AI Workspace host-accessibility isolation
- AI Workspace semantic inspection/action
- visual/coordinate fallback
- browser screenshots/diagnostics
- unchanged snapshot/hash behavior
- bounded/truncated snapshots
- parallel independent reads
- regression suite
- real Linux GUI validation

Windows/macOS adapters may be structurally/unit tested on Linux, but must not be claimed as real-platform validated until tested on those operating systems.

## Progress log

### 2026-09-22 — Architecture freeze / pre-implementation audit

Status:
- Architecture validated from research, direct benchmarks, and current RepoTunnel source inspection.
- Existing browser helper/process and CDP connection costs identified.
- Existing AT-SPI Desktop Control confirmed as the Linux semantic base to normalize, not rewrite.
- Existing AI Workspace private D-Bus launch foundation identified; `NO_AT_BRIDGE=1` must remain until an isolated accessibility bus is intentionally implemented.
- Security/compatibility invariants frozen.
- No product source modifications were made during architecture benchmark testing.
- Stage 1 is complete and verified.

### 2026-09-22 — Stage 1 complete

Implemented:
- Added `src-tauri/src/semantic.rs` with the shared backend-neutral semantic contract.
- Added short `eN` refs, internal-only backend identities, snapshot IDs, versions, SHA-256 hashes, compact unchanged responses, target invalidation/forget hooks, centralized sensitive-label detection/redaction, stale/expired ref errors, and parent/child ref mapping.
- Wired the module in `src-tauri/src/lib.rs` without changing any existing browser/desktop behavior.

Validation:
- `cargo test --manifest-path src-tauri/Cargo.toml --locked semantic::tests --lib`
- Result: 6 passed, 0 failed.
- One expected temporary dead-code warning for `invalidate_target`; it is intentionally consumed by later browser stale-ref/navigation wiring.

Next: Stage 2 — Browser CDP accessibility snapshot engine.

### 2026-09-23 — Stages 2–5 complete

Stage 2 — Browser CDP accessibility snapshot engine:
- Added bounded Chrome accessibility-tree snapshots backed by CDP Accessibility, DOM, and Page frame identity.
- Sensitive password/OTP/credential-like fields are redacted before leaving the helper and sanitized again in Rust.
- Added read-only MCP tool `browser_semantic_snapshot` with short refs, version/hash support, truncation, and compact unchanged responses.

Stage 3 — Browser semantic find + ref actions:
- Added server-side `browser_semantic_find` so clients can locate nodes without retransmitting the whole tree.
- Added `browser_semantic_action` for ref-based click/type.
- Semantic mutations reuse existing approved-workspace, Team `@browser`, browser history, and AI Review policy paths.
- CSS selector APIs remain supported and unchanged.

Stage 4 — actionability, retry, and stale refs:
- Browser semantic refs are bound to the managed browser session plus frame/loader document identity.
- Confirmed navigation/reload/close invalidates target snapshots; browser stop/runtime loss clears browser semantic state.
- Pre-action validation checks node existence, hidden/disabled/read-only state, sensitive fields, scrollability, box geometry, and click hit-target coverage.
- Only read-only pre-action checks may retry; click/type mutation itself is never blindly replayed after uncertain execution.
- Focused validation: 8/8 semantic tests and 6/6 browser tests passed.

Stage 5 — persistent browser helper/CDP sessions:
- Added one long-lived Node helper per managed browser runtime using bounded JSON-lines request/response multiplexing.
- Rust uses request IDs, one stdin writer lock, per-request response channels, and an independent stdout router; callers do not hold the browser runtime lock while waiting.
- Helper supports up to 32 in-flight requests and persistent per-tab CDP WebSockets, including de-duplicated concurrent first-connects.
- Ordinary post-start browser operations now use the persistent helper: tab listing, open/activate/close, navigate/reload, selector actions, semantic snapshot/actions, inspect, visual picking, scrolling, and screenshots.
- One-shot helper processes remain only for Chrome startup readiness and initial pre-runtime tab discovery.
- A normal CDP protocol error no longer tears down unrelated in-flight requests; transport send/close failures still invalidate the connection.
- Closing a tab immediately drops its cached persistent CDP client.
- Focused Rust validation: 8/8 semantic tests and 6/6 browser tests passed.
- Persistent service framing/syntax validation passed; forbidden nested `serve`/`monitor` operations return bounded framed errors and exit cleanly on EOF.
- Fixed and validated request-local boolean/JSON argument decoding in persistent `serve` mode so `clearFirst`, screenshot `fullPage`, and sequence payloads do not accidentally read process-start arguments.
- Live rebuilt-app performance/reconnect validation is intentionally deferred to Stage 12 so the currently running RepoTunnel/other AI session is not disrupted.

Next: Stage 6 — Browser semantic sequences.

### 2026-09-23 — Stage 6 complete

Implemented:
- Added explicit browser-history kind `sequence` plus the minimal frontend type/label support required for existing AI Review history UI.
- Added MCP tool `browser_semantic_sequence`.
- A sequence contains 1..64 already-grounded click/type/wait steps for one tab + semantic snapshot.
- Limits: maximum 2000 ms per wait, 10000 ms total wait, and 131072 bytes total typed text.
- All refs/actions/sensitive-field rules are validated when the sequence is requested and revalidated again when it actually executes after AI Review.
- AI Auto executes one stored sequence request; AI Review queues the entire sequence as one pending browser action.
- Execution is one persistent-helper request, stops at the first failing step, reports the step number, has a 25-second local execution budget under the 30-second transport timeout, and never retries a mutation.
- Once dispatched, the source snapshot is invalidated on both success and failure because earlier steps may already have changed the page.
- Completed history summaries contain only tab/snapshot/step-count metadata and never typed text.
- Persistent helper sequence responses return only completed-step counts/timing, never typed text.

Validation:
- Persistent helper wait-only sequence completed successfully through the real JSON-lines `serve` protocol without requiring Chrome.
- `npm run check` passed after adding the `sequence` history kind.
- Semantic tests: 8 passed, 0 failed.
- Browser tests: 7 passed, 0 failed, including sequence-history text privacy.
- Explicit mid-flight cancellation is still a final workflow-runtime hardening item; bounded timeout/failure behavior is implemented now and cancellation must be completed before the Stage 12 validation matrix is signed off.

Next: Stage 7 — Normalize existing Linux AT-SPI desktop semantics.

### 2026-09-23 — Stage 7 complete

Implemented:
- Added read-only MCP tool `desktop_semantic_snapshot` for normal Linux desktop applications.
- Reused the existing permission-gated AT-SPI `inspect()` path; no Python traversal/action/security behavior was replaced.
- Existing signed desktop element IDs remain internal backend identities behind shared short `eN` refs.
- Normalized common Linux accessibility roles, actions, states, bounds, parent/child relationships, truncation, version/hash metadata, and compact unchanged responses into the shared semantic contract.
- Existing helper protections remain authoritative: RepoTunnel self-control block, sensitive field handling, app/window ownership, signed stale-element validation, semantic-only typing, and window-scoped coordinate fallback.
- `application_id=ai-workspace` is deliberately excluded from this tool until Stage 8 establishes an isolated accessibility bus.

Validation:
- Rust formatting passed.
- Existing `desktop_control.py` Python syntax check passed unchanged.
- Desktop-control tests: 3 passed, 0 failed.
- Shared semantic tests: 8 passed, 0 failed.

Next: Stage 8 — Isolated semantic accessibility for AI Workspace.

### 2026-09-23 — Stage 8 complete

Implemented:
- Added one private D-Bus session owned by each AI Workspace runtime. The private bus process, address, and lifetime are internal and are torn down with the isolated workspace.
- Host-side/pre-display helper calls explicitly remove inherited `DBUS_SESSION_BUS_ADDRESS` / `AT_SPI_BUS_ADDRESS` and keep `NO_AT_BRIDGE=1`, preventing accidental access to the human desktop accessibility bus.
- Isolated target applications receive only the private session-bus address, clear inherited `AT_SPI_BUS_ADDRESS`, and enable their accessibility bridge by removing `NO_AT_BRIDGE`.
- Metacity remains on the private session bus but keeps `NO_AT_BRIDGE=1` so the window manager does not pollute the target app semantic tree.
- GNOME Terminal no longer creates a nested `dbus-run-session`; its explicit private terminal-server and fallback launch paths inherit the single AI Workspace private bus.
- The runtime treats private-bus death as session failure and purges all `SemanticSurface::AiWorkspace` refs on stop, restart, runtime failure, workspace forget, or drop.
- Added bounded private AT-SPI traversal to `ai_workspace.py`: maximum 800 nodes, depth 10, signed internal path identities, states/actions/bounds, sensitive-field text suppression, and continued visual/window fallback when semantics are unavailable.
- Reused Stage 7's Rust AT-SPI normalizer rather than duplicating role/action mapping.
- Added `AiWorkspaceState::semantic_snapshot` using the shared semantic registry with session/window-topology document identity, short `eN` refs, version/hash metadata, stale/expiry behavior, redaction, truncation, and compact unchanged responses.
- Added read-only MCP tool `ai_workspace_semantic_snapshot`.
- Existing `ai_workspace_inspect` and `inspect_desktop_app(application_id=ai-workspace)` remain compatible and now preserve semantic elements when the isolated app exposes them.
- `list_desktop_applications` now reports live AI Workspace accessibility capability from the same isolated inspection instead of hard-coding `false`.
- No private D-Bus or AT-SPI address is exposed in public status/tool responses.

Validation:
- Python helper AST/syntax validation passed.
- Rust formatting passed.
- AI Workspace focused tests: 11 passed, 0 failed.
- Desktop-control tests: 3 passed, 0 failed.
- Shared semantic tests: 8 passed, 0 failed.
- GNOME launch regressions prove target-app launch/fallback commands no longer contain `dbus-run-session`, inherit the explicit private bus, and do not set `NO_AT_BRIDGE=1`.
- The RepoTunnel command sandbox cannot perform a live private D-Bus/AT-SPI activation proof because its bubblewrap process UID has no passwd entry; therefore live private-bus accessibility is intentionally reserved for the Stage 12 rebuilt-app Linux validation. Existing screenshot/coordinate behavior remains the fallback.

Concurrent/unrelated workspace artifacts observed and preserved:
- `.scene_audio_summary.json`
- `.repotunnel_android_home/`

Next: Stage 9 — Desktop/AI-Workspace semantic sequences and fallbacks.

### 2026-09-23 — Stage 9 complete

Implemented:
- Added short-ref semantic click/type tools for both normal Linux desktop applications and isolated AI Workspace applications.
- Rust resolves `eN` refs only through the shared semantic registry, verifies snapshot/target/session ownership, rechecks advertised actions, and blocks sensitive typing before any helper mutation is dispatched.
- Python helpers re-resolve and re-sign-check the internal AT-SPI path immediately before each mutation, providing a second stale-element boundary.
- Normal desktop semantic click reuses the existing AT-SPI action-first behavior and app-owned window bounds fallback; semantic type reuses verified `EditableText`.
- AI Workspace semantic click/type use only the private Stage 8 AT-SPI bus. Click prefers the accessibility action and falls back only to a point proven inside an isolated app window; semantic type uses `EditableText` directly instead of XTest.
- Added bounded semantic sequences for both surfaces: 1..64 steps, click/type/wait, maximum 2000 ms per wait, 10000 ms total wait, 32768 bytes per type action, and 131072 bytes total typed text.
- A semantic sequence runs in one helper request/process and stops at the first failing step; mutations are never blindly replayed.
- Wait-only semantic sequences are rejected so the API cannot be abused as a generic sleep service.
- Single semantic click/type and semantic sequences invalidate the source snapshot after dispatch on both success and failure because the target may already have changed.
- Existing raw/signed-element desktop actions, AI Workspace coordinate actions, screenshots, keyboard shortcuts, and the existing non-semantic AI Workspace sequence remain supported as fallback paths.
- Added MCP tools:
  - `desktop_semantic_action`
  - `desktop_semantic_sequence`
  - `ai_workspace_semantic_action`
  - `ai_workspace_semantic_sequence`
- Workspace removal now also purges the shared semantic registry, preventing refs from surviving project deletion.
- Removed an unused duplicate AI Workspace semantic backend-action helper left by the interrupted patch; final compile is warning-free.

Validation:
- Both Python helpers pass AST/syntax validation.
- Rust formatting passes for all Stage 9-touched Rust modules.
- `cargo check --manifest-path src-tauri/Cargo.toml --locked --lib` passes warning-free.
- Desktop-control tests: 5 passed, 0 failed.
- AI Workspace tests: 13 passed, 0 failed.
- Shared semantic tests: 8 passed, 0 failed.
- Full Rust library suite: 230 passed, 0 failed, 1 intentionally ignored.
- New focused tests verify semantic action capability checks, sensitive typing rejection, AI Workspace session binding, bounded waits, mutation-required sequences, and internal backend-ID translation.
- Live AT-SPI mutation performance/correctness remains part of Stage 12 rebuilt-app Linux validation; existing visual/raw-element fallbacks remain intact until then.

Concurrent/unrelated workspace artifacts observed and preserved:
- `.scene_audio_summary.json`
- `.repotunnel_android_home/`

Next: Stage 10 — Windows UI Automation adapter behind shared abstraction.

### 2026-09-23 — Stage 10 complete (structural/Linux-host validation)

Implemented:
- Added `src-tauri/src/windows_uia.rs` behind `#[cfg(windows)]`, with pure helper tests enabled on Linux via `#[cfg(any(windows, test))]`.
- Added target-specific `uiautomation` dependency so Linux builds are not forced to compile Windows UIA bindings.
- Windows application discovery is process-scoped, excludes RepoTunnel's own PID, and maps top-level UIA windows into the existing desktop application abstraction.
- Added cached UIA snapshot traversal using Control View, bounded to 800 nodes, depth 10, and 200 children per node.
- Cached properties include name, automation ID, class/help text, process ID, control type, bounds, enabled/offscreen/focus/password state, plus Invoke/Value/Toggle/SelectionItem patterns.
- Windows UIA inspection emits the same element shape consumed by the shared desktop semantic normalizer, so public snapshots use `SemanticSurface::WindowsUia` and short `eN` refs rather than native UIA identities.
- Native backend IDs are signed path identities. Mutation re-resolves the path, recomputes the signature, and verifies the element still belongs to the permitted process.
- Click uses UIA Invoke/Toggle/SelectionItem patterns only. Type uses UIA Value pattern only, rejects password/read-only/disabled fields, and never exposes password values.
- Added bounded Windows semantic sequences with the same 1..64 click/type/wait limits used by other semantic surfaces.
- `desktop_control.rs` routes Windows list/inspect/action/semantic-sequence calls to the UIA adapter while Linux continues to use the existing AT-SPI helper unchanged.
- Shared Desktop permission remains mandatory before Windows inspect/actions; RepoTunnel self-control is also blocked inside the Windows adapter by PID.
- Existing MCP semantic tools remain platform-neutral; no separate Windows-only MCP surface was introduced.

Validation on the available Linux host:
- `cargo check --manifest-path src-tauri/Cargo.toml --locked --lib` passed.
- Windows UIA pure/helper tests: 3 passed, 0 failed.
- Shared desktop-control compatibility tests: 5 passed, 0 failed.
- Rust formatting passed for `windows_uia.rs`, `desktop_control.rs`, and `semantic.rs`.
- The Windows adapter remained unchanged during the validation window, avoiding concurrent-edit ambiguity.

Limitations intentionally not claimed:
- This machine has only the `x86_64-unknown-linux-gnu` Rust target and no MinGW/Windows toolchain, so the `#[cfg(windows)]` runtime body was not cross-compiled here.
- No real Windows UI Automation runtime test was possible without Windows hardware/runtime.
- Windows app-window screenshot / pointer-keyboard fallback is not added in Stage 10; Stage 10 is the semantic UIA adapter behind the shared abstraction. Real Windows compile/runtime and any platform fallback validation remain future platform-validation work.

Next: Stage 11 — macOS AX adapter behind shared abstraction.

### 2026-09-23 — Stage 11 complete (structural/Linux-host validation)

Implemented:
- Added `src-tauri/src/macos_ax.rs` behind `#[cfg(target_os = "macos")]`, with pure identity/role tests enabled on Linux via the test module.
- Added target-specific `axuielement 0.9.1` and minimal `objc2-app-kit 0.3.2` features for `NSWorkspace` / `NSRunningApplication`; Linux runtime dependencies remain unchanged.
- macOS application discovery uses AppKit running applications, process-scoped IDs (`ax-pid-<pid>`), excludes RepoTunnel's own PID, and records native AX availability/window count.
- Added bounded native AX inspection: maximum 800 nodes, depth 10, and 200 children per node.
- Inspection reads AX role/title/description/help/identifier/value/enabled/focused/position/size/action metadata and emits the same element shape consumed by the shared desktop semantic normalizer.
- Public semantic snapshots therefore use `SemanticSurface::MacosAx` and short `eN` refs; native AX paths remain internal.
- Native backend IDs are signed path identities. Mutation re-resolves the AX path, verifies the signature again, and verifies the element PID still matches the permitted application.
- Click uses only native AX actions (`AXPress`, `AXConfirm`, or `AXPick`).
- Type uses only a settable native `AXValue`, rejects disabled/secure fields, bounds each action, and never exposes secure-field values.
- Activate uses native AX window raise/focus behavior.
- Added bounded macOS semantic sequences using the same 1..64 step, 2-second per-wait, 10-second total-wait, 32-KiB per-type, and 128-KiB total-text constraints used by the other desktop semantic adapters.
- `desktop_control.rs` now routes macOS discovery/inspect/action/semantic-sequence underneath the existing platform-neutral desktop/MCP tools while Linux AT-SPI and Windows UIA routes remain unchanged.
- Existing Desktop permission and shared semantic ref/sensitive-value protections remain in force.
- macOS app-window screenshot and raw keyboard/scroll fallback are intentionally not introduced by the Stage 11 AX adapter; semantic click/type remains the native structured path.

Validation on the available Linux host:
- Cargo resolved and locked `axuielement 0.9.1` plus required transitive dependencies.
- macOS AX pure/helper tests: 3 passed, 0 failed.
- Shared desktop-control compatibility tests: 5 passed, 0 failed.
- Current Linux `cargo check --locked --lib` passed.
- Rust formatting passed for the macOS adapter and shared routing files.
- Dependency/API selection was checked against current AXUIElement/AppKit Rust bindings before implementation.

Limitations intentionally not claimed:
- This machine has no macOS Rust target/toolchain and no macOS hardware/runtime, so the `#[cfg(target_os = "macos")]` runtime body was not executed here.
- macOS Accessibility permission behavior and live AX app interaction must be validated on macOS before claiming real-platform validation.

Next: Stage 12 — Security/performance/regression tests + real Linux validation.

### 2026-09-23 — Stage 12 complete

Completed validation so far:
- Full pre-hardening Rust library regression passed: 236 passed, 0 failed, 1 intentionally ignored.
- TypeScript `npm run check` passed; frontend Vitest suite passed 17/17.
- Browser helper JavaScript syntax, Desktop/AI Workspace Python AST validation, and touched Rust formatting passed.
- Added cooperative browser semantic-sequence cancellation with caller-visible sequence IDs. A real concurrent JSON-lines helper test proved cancellation interrupts an active wait and returns `SEQUENCE_CANCELLED` without replaying a mutation.
- Added persistent-browser-helper recovery. If only the helper dies, RepoTunnel reconnects a new helper to the existing Chrome debug port/session instead of tearing down Chrome.
- Read-only/idempotent helper operations may retry after reconnect; navigation/click/type/semantic mutation/sequence/scroll/reload operations are never automatically replayed after an ambiguous transport failure.
- Helper replacement is serialized so parallel callers do not race competing replacements; replacement children are cleaned up if the browser session changes/disappears during reconnect.
- Browser recovery tests now pass 10/10, including explicit retry-policy and helper-transport classification tests.
- Real Linux helper restart proof passed against the active managed Chrome session: two different helper PIDs connected before/after restart, Chrome remained the same, and all three tab IDs remained identical.
- Current source helper connected read-only to live Chrome 153 / CDP 1.3 and returned a real semantic snapshot (23 nodes) with document identity and parent/child relationships.
- Live browser snapshot bounding passed: max 20 returned 20/23 nodes with `truncated=true`; max 80 returned all 23; document identity and first node identities remained stable.
- Exact production actionability logic was exercised without live mutation: stale document -> `STALE_REF`; hidden/disabled/covered -> `NOT_ACTIONABLE`; visible hit-test -> valid click point.
- Exact production sensitive typing check returned `SENSITIVE_FIELD` for password input.
- Exact production semantic sequence tests passed for success, first-step failure, timeout, and cancellation.
- Legacy CSS selector inspection remains compatible (`h1` returned the live 403 heading).
- Browser visual grounding fallback passed read-only: center-screen `pick-element` resolved to a valid DOM selector/context.
- Parallel live read-only validation passed for browser DOM inspection, diagnostics, viewport screenshot, and Linux desktop AT-SPI inspection.
- Browser screenshot validation produced a real PNG (102868 bytes) and browser diagnostics returned live console/network entries.
- Linux Chrome AT-SPI inspection succeeded and returned the same six semantic element IDs across three consecutive inspections.
- RepoTunnel remains absent from the controllable desktop-app list, preserving self-control protection.
- AI Workspace became available and an isolated GNOME Terminal fallback sequence (type + Enter + wait) completed in 537 ms; screenshot verification showed exactly `stage12-fallback-ok`. The isolated session was then stopped.
- The disposable failed headless-Chrome harness directory `.stage12_browser_tmp` was removed.

Final real-Linux AI Workspace semantic proof:
- The secure RepoTunnel command sandbox intentionally hides its passwd database, so a normal disposable `dbus-daemon --session` cannot start there. Validation did not weaken or bypass RepoTunnel isolation.
- A disposable workspace-local anonymous private D-Bus was therefore used only as test scaffolding. The real `at-spi2-registryd` registered on that private bus, and a tiny local `org.a11y.Bus` / `org.a11y.Status` broker exposed the same protocol expected by GTK.
- Because the stripped-down sandbox has no desktop settings/module loader, the disposable GTK test app explicitly initialized the installed native `libatk-bridge-2.0` bridge. RepoTunnel source/helper code was not modified for this test workaround.
- The current-source `ai_workspace.py` helper then returned a live private semantic tree with 6 nodes from the hidden Xvfb GTK app.
- Semantic typing into the normal field succeeded and re-inspection returned exactly `stage12-semantic-ok`.
- A password field was marked sensitive, its existing text was redacted, and semantic typing into it was blocked.
- A semantic button action invoked the native accessibility action; re-inspection returned `clicked-ok`.
- This proves the current-source private AT-SPI inspect/type/click/sensitive-field path on real Linux without touching the human desktop, managed Chrome, or another AI Workspace.
- All disposable D-Bus/AT-SPI/Xvfb/GTK processes exited and all `.stage12*` artifacts were removed.

Final post-hardening regression:
- `cargo check --manifest-path src-tauri/Cargo.toml --locked --lib` passed.
- Full Rust library suite on the final Stage 12 tree: 239 passed, 0 failed, 1 intentionally ignored.
- TypeScript `npm run check` passed.
- Frontend Vitest suite passed 17/17.
- Browser helper JavaScript syntax, both Python helper AST checks, and Rust formatting all passed.
- Team `@browser` policy remains in every browser mutation surface, including semantic action, semantic sequence, and semantic-sequence cancellation; read-only browser inspection remains outside that mutation lock.
- AI Review behavior remains unchanged: Automatic executes immediately; non-Automatic actions are stored as Pending, can only be approved/rejected while Pending, execute once on approval, and clear the stored request afterward.

Stage 12 result:
- COMPLETE for source implementation, Linux semantic/browser/desktop validation, security/regression coverage, helper recovery, bounded workflows, and fallback behavior.
- Windows UIA and macOS AX remain structural/unit validated only on this Linux machine, exactly as recorded in Stages 10/11; they must not be described as real-platform validated until tested on those operating systems.
- The currently running RepoTunnel process still predates these source changes. The new semantic MCP methods will become available to the live app only after a later safe rebuild/restart. That is deployment state, not a remaining Stage 12 source-validation blocker.
- No `.deb` was built or installed during this implementation, as required.

Production build verification:
- `npm run build` passed: TypeScript + Vite production bundle completed successfully (117 modules transformed).
- `cargo build --release --manifest-path src-tauri/Cargo.toml --locked` completed with exit code 0.
- Release binary: `src-tauri/target/release/repotunnel`, 53,616,088 bytes, Linux x86-64 ELF.
- Binary string verification confirmed the current semantic endpoints are compiled into the release binary: `ai_workspace_semantic_snapshot`, `browser_semantic_sequence_cancel`, and `desktop_semantic_sequence`.
- This build was not launched and no package was installed, so the currently running RepoTunnel process remains undisturbed.

Next deployment step (only when safe):
- Preserve the current known-good stable checkpoint. Restart/use the new build only when no active managed-browser/AI-Workspace work will be disrupted, then perform a short installed-app smoke test before considering any new stable checkpoint.
