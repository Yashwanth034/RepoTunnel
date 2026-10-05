# RepoTunnel AI Continuity Guide

> **Current-state note (2026-10-05):** This file is a chronological engineering/continuity log. Dated sections preserve what was true when that work happened, so older “not implemented,” “next step,” test-count, or “current running binary” statements may be superseded. For present behavior, check the live source/runtime plus `docs/product.md`, `docs/architecture.md`, and `docs/mcp.md` before acting. As of this audit, `wait_process` and durable managed supervision exist; Linux source advertises complete RepoTunnel-process restart reattachment after a persisted-state/detach/adoption regression passed, while Windows/macOS intentionally remain false pending equivalent native identity/control validation; browser selector/semantic click/type mutations and semantic sequences now persist non-replayable mutation receipts and surface helper-transport uncertainty as `ambiguous` instead of replaying a possibly completed action; the core Phone real-device audit passed; and the latest full source regression recorded 43/43 frontend tests and 448 Rust tests passed with 0 failures and 8 intentional ignores.

Read this file and `STABLE_CHECKPOINT.md` before making substantial changes to RepoTunnel. Historical entries are evidence, not an instruction to redo completed work.

## Current development policy

RepoTunnel will continue receiving new features and improvements after the stable checkpoint at:

```
8c571da45f5ef4e32e716ed227dddf8553a61599
```

That commit must remain the known-good recovery baseline until the user explicitly locks a newer stable checkpoint.

## Rules for any AI working on this repository

1. Use the existing RepoTunnel project and branch. Do not recreate the project or start a duplicate workspace.
2. Preserve existing working functionality unless the user explicitly asks to replace it.
3. Do not silently reset, revert, or discard unrelated changes.
4. Before risky changes, inspect Git status and understand what is already modified.
5. Keep temporary test/debug artifacts out of commits.
6. Run relevant automated tests after implementation.
7. For desktop/UI/media work, verify the real behavior in the isolated AI Workspace when practical instead of relying only on unit tests.
8. Do not call a feature finished only because it compiles; verify the user-visible behavior.
9. Do not automatically install generated packages on the user's machine. Build and verify them first, then give the user a safe install command when requested.
10. When the user says a new version is fully working and wants it locked, create a new stable checkpoint by updating both this file and `STABLE_CHECKPOINT.md` with the new branch, full commit SHA, validation evidence, and recovery instructions.

## How to resume after a future problem

When the user says something like:

- "go back to the working one"
- "return to the last stable version"
- "this new feature broke RepoTunnel"
- "use the previous correct working version"

first read `STABLE_CHECKPOINT.md`, inspect current Git status/log, and identify the delta from the stable commit. Preserve any newer work unless the user explicitly authorizes discarding it.

## Important locked behavior at the current baseline

- Standalone Video Project workflow stays available.
- Video playback fix using private localhost HTTP byte-range serving stays intact.
- Video Project media helpers remain visible.
- Long-video seeking/playback must not be regressed.
- RepoTunnel desktop UI must not behave like a browser page with whole-app zoom.
- Existing Direct HTTPS, OAuth/DCR, project/workspace data, account state, MCP gateway security, and previously working project functionality must not be casually replaced or reset.

## Next phase

New feature development may continue from the current branch. Treat the stable commit above as the emergency recovery anchor until a newer stable checkpoint is deliberately created and validated.

The **Semantic Interaction Layer source implementation is complete through Stages 1–12** as of 2026-09-23. Its canonical architecture, safety rules, full implementation history, validation evidence, platform limitations, and deployment notes live in `SEMANTIC_INTERACTION_LAYER.md`. Future AI sessions working on semantic/browser/desktop/AI-Workspace code must read that file first and must not restart the architecture or redo completed stages.

Current semantic-layer state:
- Linux browser CDP semantics, semantic refs/actions/find, persistent helper/reconnect, bounded semantic sequences/cancellation, Linux AT-SPI desktop normalization/actions/sequences, and isolated AI Workspace private AT-SPI semantics are implemented in source.
- Windows UIA and macOS AX adapters are implemented behind the shared abstraction but are structural/unit validated only on Linux until real Windows/macOS hardware is available.
- Final source regression evidence: 239 Rust library tests passed, 0 failed, 1 intentionally ignored; TypeScript check passed; frontend Vitest passed 17/17; helper syntax/format gates passed.
- Real Linux current-source private AT-SPI validation passed on a disposable hidden Xvfb/GTK harness: semantic inspect returned nodes, normal semantic type succeeded, password text was redacted and blocked from semantic typing, semantic click succeeded, and re-inspection verified the UI changes.
- No `.deb` has been built or installed for this semantic-layer work.
- Production build verification passed: `npm run build` completed successfully and `cargo build --release --manifest-path src-tauri/Cargo.toml --locked` exited 0. The 53,616,088-byte release binary contains the new semantic MCP endpoint strings.
- The currently running RepoTunnel process may still be the older pre-semantic build. Do not restart it while managed-browser or AI-Workspace work is active. When safe, the next step is a controlled new-build smoke test, not more architecture work.
- Do not update `STABLE_CHECKPOINT.md` or replace the known-good recovery baseline unless the user explicitly confirms the new build is fully working and asks to lock it.

## 2026-09-23 — RepoTunnel reliability hardening after real AI usage

Completed safely on top of the semantic-layer source:
- Linux terminal Bubblewrap now supplies a private synthetic `repotunnel` passwd/group identity for the current UID/GID. It does **not** mount or expose the host `/etc/passwd` or `/etc/group`. This fixes legitimate tools such as D-Bus/NSS consumers that require the sandbox UID to resolve.
- Sandboxed Cargo now persists only per-workspace `CARGO_HOME/registry` and `CARGO_HOME/git` caches in a RepoTunnel-owned cache path. Cargo credentials/config remain ephemeral and the user's real `~/.cargo` is never mounted. This avoids repeatedly downloading the crates registry for every sandboxed build.
- Real Linux Bubblewrap validation passed for both changes: synthetic identity resolved as `repotunnel`, registry cache survived a fresh sandbox, and a fake `credentials.toml` did not survive.
- Continuity Resume v2 source was optimized so `resume_snapshot` no longer calls the full monitoring snapshot merely to obtain process facts. It now calls `terminal::list_processes` once and reuses that canonical list for active/failure reconciliation. This removes unnecessary port scanning, process-log tails, browser diagnostics, terminal-history snapshotting, and file-event work from the reconnect path.
- MCP `request_git_commit` and `list_git_history` now omit full commit-diff `detail` from AI-facing responses. RepoTunnel still persists the exact staged diff internally and the local Git UI still receives it for AI Review/history. Commit approval continues to revalidate HEAD plus the staged fingerprint, so review/security semantics are unchanged.
- Project text search now skips nested entries/directories that fail with `PermissionDenied` or `NotFound` instead of aborting the entire repository search. Unexpected I/O errors still fail normally; protected-path checks remain unchanged.
- MCP guidance now switches builds/tests/install/verification work to `start_process` at about 30 seconds instead of 60 seconds, reducing ambiguity from client/RPC timeouts while a legitimate child process is still working.
- Exact-context patch failure diagnostics are improved without weakening patch safety: when the full expected block no longer matches, RepoTunnel checks a bounded candidate set (at most 32 candidate lines, at most 8 file scans) and may return one current matching line, line number, and re-read guidance. It still never fuzzy-applies or guesses a mutation.
- Focused validation for the latest audit fixes passed: filesystem tests **6/6**, MCP-server tests **4/4**, and targeted Rust formatting check passed.
- Final Rust library regression on this tree: **244 passed, 0 failed, 3 intentionally ignored**. The two Linux Bubblewrap tests that are ignored by the normal suite were run manually earlier and passed.

Important audit conclusions — do not duplicate:
- Continuity/Resume v2, factual activity history/milestones, monitoring snapshots, incremental managed-process output, and bounded AI Workspace sequences already exist. Improve these implementations instead of creating parallel session/checkpoint systems.
- The ChatGPT UI errors `Message delivery timed out` and `Connection interrupted` are outside RepoTunnel's control after tool execution. RepoTunnel cannot reliably infer that a ChatGPT final message failed to reach the browser from an MCP disconnect alone. Do **not** add a fragile “session ended” detector. The supported recovery design is to persist factual work continuously and call `get_resume_snapshot` after reconnect.
- The currently running RepoTunnel process may still be an older build, so the optimized Resume implementation cannot be live-benchmarked through that process until a later safe rebuild/restart.

Confirmed real gaps, intentionally deferred for a dedicated phase:
- Managed processes are persisted to history/logs but are deliberately stopped/detached on RepoTunnel app exit/startup; automatic safe reattachment across RepoTunnel restarts is **not implemented**.
- There is no dedicated nonblocking process-pattern watcher yet. Do not implement a single long-held MCP `wait_for_process_output` call; design it as a durable/nonblocking watcher so transport timeouts cannot recreate the same problem.
- RepoTunnel does not yet provide a generic partial-success batch API for unrelated repository/Git/process operations. Existing semantic/UI sequences are intentionally bounded/fail-fast and should not have their semantics casually changed.
- Compact/filtered Git status modes remain a lower-priority ergonomics improvement.

Next step:
- Do not build/install a package yet. The user has more RepoTunnel improvements to specify. Preserve all current uncommitted semantic + reliability work and continue from this section after the next request.

## 2026-09-23 — Video Project hardening from real tutorial-production feedback

A real ~10:30 tutorial-production run exposed concrete Video Project weaknesses. The feedback was audited against current source before changes; existing capabilities were reused instead of duplicated.

Implemented and validated:
- **Scene-centric durable production state:** ordered scene records under `storyboard/scenes/` now keep purpose, teaching point, narration, duration, caption policy, factual source claims, generated scene source/render, narration audio, subtitle path, and QA status together. Existing script/storyboard/timeline documents remain compatible.
- **Narration synchronization:** narration requests may carry `sceneId`; matching scene records are updated with narration audio/subtitles/duration. Generated scene rendering uses the same scene ID and records its source/render back into the scene.
- **Narration download safety:** managed neural narration is used automatically only when already ready. First-time managed runtime/model download requires both project-level `allowLocalModelDownloads=true` and request-level `allowManagedDownload=true`. Auto mode does not silently fall back to robotic eSpeak.
- **Readable captions:** subtitle generation uses punctuation-aware bounded chunks (roughly 4–8 words, max 42 characters) rather than giant sentence-level paragraphs.
- **Explicit caption delivery:** render requests support `none`, `sidecar`, `embedded`, `burned`, or `burned+sidecar`. Burned captions do not also attach a preview sidecar unless explicitly requested, and there is no accidental burned+embedded mode.
- **Generated-scene preflight:** deterministic layout validation runs before frame generation and catches invalid geometry, text overflow, canvas overflow, text collisions, unsupported multiline layout, plus safe-area/semantic-density warnings.
- **Resource policy:** Video Projects default to blocking local model downloads and automatic media-tool installation. FFmpeg acquisition paths for recording, preview conversion, scene encoding, and timeline rendering respect the policy. Older manifests default safely.
- **Standalone path clarity:** manifests distinguish standalone vs legacy storage; the UI displays standalone projects as `~/Projects/<slug>`. `video_project_ai_workspace` launches the permitted isolated GUI with the validated Video Project root as working directory while preserving normal Desktop permission/self-control protections.
- **Async/idempotent rendering:** `start/get/list/cancel_video_project_render` provide durable background jobs with progress/phase, cancellation, project-owned persisted job snapshots, content-hash deduplication, and safe interrupted-state recovery after RepoTunnel restart. Equivalent retries do not launch duplicate FFmpeg work when the running/completed job is still valid.
- **Job-local failures:** render failures remain on their job rather than poisoning the project with stale global errors.
- **Final QA gate:** a final render registers the project as `review`, never `completed`. Only a passing `qa_video_project` result marks the final project completed.
- **Video QA:** QA resolves standalone assets directly and validates stream count, configured resolution/FPS, duration, audio presence, caption policy, embedded+sidecar duplication, sustained black/static segments, sustained silence, integrated loudness, and true peak. Reports persist under `qa/`. Creative-signal anomalies are warnings where appropriate; clipping/overs are failures.
- **Voice-priority audio mix:** render requests now have an optional `audioMixPreset`. `simple` remains the backward-compatible default; explicit `voice-priority` uses local FFmpeg sidechain compression to duck music under narration and loudness-normalizes narration-led output. The manual editor exposes the preset when replacement narration is selected. No external/cloud service is involved.
- **Semantic diagram renderer:** `render_video_project_diagram` accepts bounded high-level `flow_diagram`, `architecture_diagram`, `comparison`, `timeline`, `token_flow`, `before_after`, and `metric_cards` specs. RepoTunnel validates meaningful node/relationship content, lays the diagram out for the real project canvas, compiles it to the existing scene model, and then runs the normal deterministic layout preflight/render path. Use the low-level scene renderer only for custom graphics that need manual geometry.
- **Consolidated licensing provenance:** each external asset still keeps its individual immutable license record, and recording a license now rebuilds a bounded project-owned `licenses/manifest.json` index containing the current provenance records. The manifest is registered as a Video Project asset and remains protected by cleanup.
- **Cleanup/version management:** `clean_video_project` is dry-run by default, reference-aware, symlink-safe, blocks apply while a render is active, preserves current/final/scene-linked/source/license/QA assets, and removes only stale render versions plus known render/frame temp directories.
- **Canonical pipeline progress:** `get_video_project_pipeline_status` derives Script → Storyboard → Voice → Scenes → Assembly → QA → Export from durable state. MCP guidance tells future AIs to consult it before choosing the next video-production action. The Video Project UI shows overall and per-stage progress.
- **Frontend backward compatibility:** the Video panel safely defaults missing new resource-policy fields, so an older running backend/older project response does not crash the UI during a rolling source upgrade.
- Timeline/scene schema version remains internal to normal MCP input; AIs no longer need to guess a version number.

Latest validation:
- Rust formatting check passed after formatting the touched Rust source.
- Final combined P0+P1 Rust library regression: **264 passed, 0 failed, 3 intentionally ignored**. The ignored tests are environment/download-specific; the two Linux Bubblewrap integration tests were manually proven earlier.
- Advanced Video QA focused tests: **4/4 passed**.
- Scene-centric persistence, pipeline-status, render/cleanup/restart, and MCP-exposure focused tests passed.
- Frontend Vitest: **17/17 passed**, including all 10 Video Production panel tests.
- TypeScript `npm run check` passed.
- No `.deb` was built or installed.
- Stable recovery checkpoint remains unchanged; do **not** update `STABLE_CHECKPOINT.md` unless the user explicitly asks to lock a newer stable version.

Deliberately not implemented blindly:
- No cloud-neural TTS service was added: current Video Project code has no dedicated secure provider/credential contract, and adding a paid/keyed service implicitly would violate the resource/cost constraints. Existing neural/local/import architecture remains the safe base.
- No opaque one-call `produce_video_project` black box was added. The canonical pipeline-status contract plus durable scene/job/QA state provides recoverable orchestration without hiding long-running actions or failure state.
- AI visual-semantic similarity/auto-repair, richer specialized visual components such as code/terminal/browser panels and callouts, final export variants/YouTube package, scripted demo recorder, and secure cloud-neural TTS provider integrations remain separate future improvements. Verify existing source before implementing any of them.

Next Video Project step:
- The P0 foundation and the first high-value P1 quality improvements (semantic diagrams, voice-priority mastering/ducking, and consolidated license provenance) are implemented and regression-tested. Continue only with remaining quality features that add clear value without duplicating the scene/pipeline/job/QA systems.

## 2026-09-23 — Runtime polling + Browser Runtime v2 audit from heavy real-browser feedback

The user supplied `Pasted markdown(20260923-085427).md`, written after another AI used RepoTunnel heavily for browser testing, long terminal jobs, source/binary inspection, repeated resumptions, and security-research-style workflows. The feedback was read in full and audited against current source before implementation. Existing Continuity v2, managed jobs, Semantic Interaction Layer, secret guard, and browser helper work were reused rather than duplicated.

### Repeated status/process timeout — source fix

The recurring “combined status/process query timed out” problem had a real RepoTunnel-side contributor in addition to any outer MCP/client orchestration timeout:
- High-frequency read-only MCP probes were synchronously writing activity-history observations before replying.
- `list_processes` previously reconciled process activity record-by-record, creating avoidable global activity-ledger lock/load/save work.
- `read_process_output` wrote an activity observation on every poll and reconciled activity even while a process was still running.
- Browser recovery probes such as status/tabs/page-inspect/diagnostics also generated read-only activity writes.
- When several managed children exited together, `refresh_all_processes` could update persisted process history in separate cycles.

Implemented:
- Added lightweight `get_workspace_runtime_status`: one read path returns Git repository state plus bounded managed-process state and running-process count. It deliberately skips browser diagnostics, listener scans, log tails, monitoring file events, and read-only activity observations.
- MCP guidance now tells AIs to use `get_workspace_runtime_status` when both Git + managed-process state are needed instead of bundling/parallelizing `git_status` and `list_processes`, and to reserve `get_monitoring_snapshot` for genuinely heavy diagnostics.
- Routine `list_processes` is now read-only with respect to the activity ledger.
- Routine running-process `read_process_output` polling no longer writes read-only activity events and only reconciles activity when the process reaches a terminal state.
- Read-only browser status/tab/DOM/diagnostics probes no longer create activity-history noise.
- Added batched activity process reconciliation for real state transitions.
- Managed-process exit refresh now collects simultaneous child exits and persists their history updates in one process-store transaction instead of N separate writes.
- Kept the existing nonblocking `start_process` + incremental output-offset model. No long-held wait RPC was introduced.
- Kept the invalid host-PID-liveness workaround removed; Bubblewrap PID namespaces make host-`ps` inference unsafe.

Important deployment note:
- These timeout improvements are implemented and regression-tested in source. The currently running RepoTunnel connector can still be an older binary, so this chat may continue to exhibit the old polling behavior until a later controlled rebuild/restart. Do not restart/install a package automatically.

### Browser Runtime v2 — implemented gaps that were actually missing

**Persistent pre-navigation browser context**
- Existing partial `BrowserContextConfig` storage/validation was completed instead of replaced.
- Added MCP `get_browser_context` and `configure_browser_context`.
- Context supports a bounded name, non-secret default HTTP headers, and optional user-agent.
- Secret-bearing header names (Authorization, Cookie, API-key/token/secret/password/credential-like names) are rejected rather than persisted in plaintext.
- Browser startup applies the saved context to initial tabs.
- New external tabs are created as `about:blank`, context is applied first, then external navigation occurs. If the context cannot be guaranteed, RepoTunnel closes the tab/session rather than silently making an unlabelled request.
- Browser-helper reconnect restores the saved context on current tabs before the replacement helper is accepted.

**Atomic navigation receipt + generation consistency**
- Existing partial `navigate-observe` helper/model work was completed and wired into normal `browser_action action=navigate`; no parallel navigation tool was added.
- In AI Auto, the same navigation call now returns a bounded receipt with requested/final URL, title, ready state, observed HTTP status, load state, redirect chain, request count, changed cookie **names** (never values), network failures, duration, navigation loader/generation ID, document generation ID, bounded DOM text/HTML, and typed error state.
- Typed navigation outcomes currently distinguish `NAVIGATION_FAILED`, `DOCUMENT_GENERATION_MISMATCH`, and `PAGE_LOAD_TIMEOUT`.
- DOM/title/readystate are returned only when the observed document generation matches the requested navigation generation. An aborted navigation can no longer silently hand the AI DOM from the previous page as though it belonged to the new request.
- `browser_inspect_page` now exposes the current `documentGeneration` as well.
- Ambiguous navigation mutation is never automatically replayed after helper transport failure.

**Network inspector foundation**
- Added MCP `get_browser_network_history`, exposing bounded successful + failed request metadata captured continuously by the managed browser monitor: request ID, URL, method, HTTP status/status text, resource type, MIME type, failure/error, timestamp.
- Raw Authorization/Cookie headers, arbitrary response bodies, and WebSocket frames are intentionally **not** exposed in this pass.
- Browser event-log URLs and AI-facing navigation/network URLs redact secret-like query values and drop fragments before persistence/output. Ordinary host/path/status information remains available.

**Capabilities discovery**
- Added compact read-only `capabilities` MCP endpoint so AIs do not need to inspect internal tool metadata to discover supported runtime/browser/continuity behavior.
- It truthfully reports current limitations, including: no managed-process restart reattachment, no automatic ChatGPT session-end detector, no browser response-body capture, no WebSocket-frame capture, no scope allowlist, and no persistent raw secret browser headers.

**Redaction precision**
- Current secret guard was audited and already avoids generic hex/native-address redaction. It targets credential-shaped assignments, known token prefixes, bearer tokens, JWTs, private keys, and sensitive env/header names.
- Added regression coverage proving ordinary native addresses, offsets, and hash-like technical values remain unchanged.

### Feedback items already present — do not rebuild

- Managed background jobs with stable IDs, status, bounded output offsets/tails, stop/cancel behavior, and persistence are already present. Keep improving the existing runtime; do not create a second job system.
- Continuity Resume v2, factual activity/milestone history, semantic project memory, live Git/process reconciliation, and reconnect recovery already exist. Do not replace them with another handoff Markdown/database layer.
- Linux AI-terminal sandbox already supplies private ephemeral `/tmp` + `TMPDIR=/tmp`; a second persistent scratch subsystem was not added without a demonstrated remaining need.
- Semantic browser/desktop interaction, short-lived refs, helper recovery, and Team browser locking already exist.

### Deliberately deferred rather than implemented blindly

- **Reliable click/type/submit transaction receipts:** still a real gap. Existing browser mutations deliberately are not auto-retried because replay can duplicate state-changing actions. A trustworthy receipt needs explicit CDP causal correlation/nonces and duplicate-risk semantics; do not fake certainty or replay ambiguous mutations.
- **Research Mode / scope allowlist / redirect boundary / rate limiter:** valuable but requires a coherent interception/policy model before destination requests. Do not bolt an after-the-fact hostname check onto navigation and call it safe.
- **Response-body capture and WebSocket-frame capture:** require explicit retention/redaction/privacy limits before enabling.
- **Unified raw HTTP client:** separate network/security surface; not added just because `curl` was sometimes used.
- **Security evidence recorder / branch database:** Continuity v2 covers generic continuity today; a specialized evidence system should be designed as one coherent extension, not another ad-hoc state file.
- **Managed-process reattachment after RepoTunnel restart:** still not implemented; process history/output survive, but automatic safe reattachment is a separate lifecycle/security change.
- **Automatic ChatGPT session-end detection:** still not reliably observable from RepoTunnel and must not be faked.
- Optional binary/Android/security capability packs and generic artifact cleanup remain separate future work.

### Validation for this runtime/browser pass

Focused validation:
- terminal tests: **8 passed, 0 failed, 2 intentionally ignored** (the two Linux Bubblewrap integration tests are normal-suite ignores and were manually proven earlier).
- browser tests: **12/12 passed**.
- secret-guard tests: **3/3 passed**.
- Runtime/Browser v2 MCP exposure test passed.
- Rust `cargo check` passed.
- Browser helper `node --check` passed.
- TypeScript `npm run check` passed.

Final current-tree regression:
- Rust library: **268 passed, 0 failed, 3 intentionally ignored**.
- Frontend Vitest: **17/17 passed**.
- Rust formatting check passed.
- Browser helper syntax check passed.
- TypeScript check passed.

Safety/state:
- No `.deb` was built or installed.
- Nothing was staged, committed, pushed, reset, reverted, recloned, or reinstalled.
- Stable recovery checkpoint remains unchanged; do **not** update `STABLE_CHECKPOINT.md` unless the user explicitly asks to lock a newer stable version.
- Preserve unrelated untracked artifacts and the full existing Semantic Interaction Layer/Video Project work.

Next step:
- Do not add the deferred browser/research features casually. If the user asks to continue Browser Runtime v2 later, the highest-value unresolved item is a non-replayable state-changing action receipt design (click/type/submit) with causal request/navigation correlation and explicit duplicate-risk semantics; after that, scope interception/research policy can be designed as a separate security feature.

## 2026-09-23 — AI Self-Continuation via MCP App

The user explicitly accepted an MCP-App-only recovery design and rejected Chrome extensions, browser automation, and a separate desktop app. The implementation was added as a small extension of existing Continuity/activity/process state rather than a second project-recovery system.

### Official ChatGPT / MCP behavior verified before implementation

Current OpenAI MCP Apps/Plugins documentation was checked before coding:
- ChatGPT implements the MCP Apps UI resource model.
- Standard tool/UI linkage is `_meta.ui.resourceUri`; ChatGPT also honors `openai/outputTemplate` as a compatibility alias.
- MCP App resources use `text/html;profile=mcp-app`.
- Standard `ui/message` is exposed in ChatGPT as `window.openai.sendFollowUpMessage({ prompt, scrollToBottom })`.
- `window.openai.callTool` is ChatGPT's compatibility alias for UI-initiated MCP `tools/call`.
- Tool calls include `_meta["openai/session"]`, an anonymized ChatGPT conversation identifier intended for correlating requests in one conversation.
- Widget state is UI-instance state; durable authoritative recovery state therefore stays in RepoTunnel AppData.
- Tool annotations must reflect real state mutation. Recovery polling/claim/ACK are not falsely marked read-only.

Important official limitation:
- The documented MCP App bridge does **not** provide RepoTunnel with a reliable global “the assistant is currently reasoning/generating” signal or a guaranteed final “session is ending” callback.
- This feature therefore uses conservative stale detection plus proactive AI state updates/heartbeats. Do not replace this limitation with browser automation or pretend `automatic_chat_session_end_detection` exists.
- RepoTunnel capabilities now report `mcpAppSelfContinuation=true`, `assistantGenerationSignal=false`, and `automaticChatSessionEndDetection=false`.

### Architecture

New source:
- `src-tauri/src/self_continuation.rs`
- `src-tauri/resources/self_continuation_app.html`

Wiring:
- `src-tauri/src/lib.rs` includes the self-continuation module.
- `src-tauri/src/mcp_server.rs` exposes the tools/resource and server instructions.

No normal RepoTunnel desktop UI was added.

### One AI-maintained continuation sentence

Model-visible tools:
- `arm_self_continuation`
- `update_self_continuation`
- `heartbeat_self_continuation`

AI instructions now require:
- arm once near the start of substantial multi-step ChatGPT work;
- maintain one specific sentence that says the exact remaining work and what is already complete;
- replace that sentence when meaningful remaining work changes;
- use heartbeat only for prolonged active work when the sentence is still current;
- set `waiting_user` before intentionally waiting for human input;
- set `completed` before intentionally finishing the request;
- do not arm trivial one-step answers;
- never use a generic built-in message such as `continue`.

Sentence validation:
- one line only;
- max 500 characters;
- at least 6 words;
- rejects known generic continuation phrases;
- rejects credential/token/private-key-shaped content through RepoTunnel's existing `secret_guard`.

RepoTunnel never generates a replacement continuation sentence itself. The pending recovery always copies the exact latest AI-maintained sentence.

### Conversation binding

- The arm/update/heartbeat/poll/claim/ACK flow binds durable state to ChatGPT's `_meta["openai/session"]`.
- Arming fails closed if that official ChatGPT conversation metadata is unavailable; it does not silently fall back to workspace-only recovery in another MCP host.
- Every later operation verifies the same conversation session.
- Within one ChatGPT conversation, re-arming the same workspace reuses the current watch identity and replaces its sentence/state; any older duplicate watches from pre-fix state are disabled.
- Switching RepoTunnel workspaces in the same ChatGPT conversation disables the previous workspace's watch so an old widget cannot inject stale work.

### Durable state + stale detection

Private AppData stores small per-watch JSON state with atomic temp-write + rename:
- watch/workspace/session IDs;
- latest sentence;
- mode: `working`, `waiting_user`, `completed`, or `disabled`;
- revision/activity epoch;
- AI/project activity timestamps;
- stale threshold;
- pending recovery metadata;
- last sent epoch/recovery ID/time.

Stale threshold:
- default 600 seconds;
- bounded to 180..1800 seconds.

Recovery evaluation reuses existing RepoTunnel state:
- Continuity meaningful project activity (`latest_workspace_activity_at` / `latest_meaningful_activity_at`);
- canonical managed-process state.

It does **not** trigger while:
- mode is `waiting_user`, `completed`, or `disabled`;
- a managed process is pending/running;
- recent AI heartbeat/update exists;
- newer meaningful RepoTunnel project activity exists;
- the current activity epoch already sent a recovery.

If a managed process starts after recovery was queued, the pending recovery is explicitly cancelled. Delivery claim also re-checks current meaningful activity/process/mode immediately before message submission so a stale earlier poll cannot send after work resumed.

### Minimal MCP App + queue/retry

Resource:
- `ui://repotunnel/self-continuation/v1.html`
- MIME: `text/html;profile=mcp-app`
- minimal status text only; no second RepoTunnel interface.

The app:
1. reads the watch ID/workspace from the arm tool's structured output;
2. polls `poll_self_continuation`;
3. if a recovery is pending, attempts a server delivery claim;
4. sends the **exact pending AI sentence** through `window.openai.sendFollowUpMessage`;
5. records the recovery ID in widget state;
6. ACKs the server.

If the ChatGPT tool/follow-up bridge is unavailable or message submission fails:
- the durable server recovery remains pending;
- no ACK occurs;
- focus/visibility/timer polling retries later.

If no UI connection exists during an outage, the first poll after reconnection evaluates the durable stale watch and queues/sends recovery then. No final session-end callback is required.

### Deduplication / concurrent widgets

Three layers are used:
1. server `activity_epoch` + `last_sent_epoch` prevents repeated recovery for unchanged work;
2. widget state remembers recently submitted recovery IDs;
3. `claim_self_continuation_recovery` grants one mounted widget a short 60-second delivery lease before `ui/message`, preventing multiple old/mounted widgets from racing the same pending recovery.

ACK is bound to the widget claimant and is idempotent for an already-sent recovery ID.

Important exactly-once limitation:
- MCP Apps currently do not expose one atomic transaction combining “ChatGPT definitely accepted this `ui/message`” with RepoTunnel's durable ACK/idempotency key.
- Therefore the implementation strongly suppresses duplicates, but a host/widget crash in the narrow interval **after ChatGPT accepts the follow-up and before both local sent-state/server ACK become durable** can theoretically cause a retry later.
- Do not claim mathematically exact once-only delivery until the host API provides an atomic/idempotency mechanism.

### MCP visibility / resource metadata

Tool visibility:
- `arm_self_continuation`: model + app, owns the UI resource.
- `update_self_continuation`, `heartbeat_self_continuation`: model-only.
- `poll_self_continuation`, `claim_self_continuation_recovery`, `ack_self_continuation_recovery`: app-only.

Metadata:
- standard `_meta.ui.resourceUri` on the arm tool;
- ChatGPT `openai/outputTemplate` compatibility alias;
- `openai/widgetAccessible=true`;
- app-only/model-only tools deliberately have no resource URI so they do not mount extra UI;
- server advertises MCP resources and `resources/list` exposes the minimal self-continuation resource;
- resource uses standard `ui.prefersBorder=false` plus compatibility widget metadata.

Safety annotations:
- all continuation tools are `readOnlyHint=false` because they mutate private recovery state;
- all are `destructiveHint=false`;
- all are `openWorldHint=false`;
- poll + ACK are marked idempotent where behavior actually supports it.

### Validation

Focused current-tree validation:
- self-continuation Rust state-machine tests: **9/9 passed**;
- MCP tool visibility/resource/annotation contract test: passed;
- Rust `cargo check`: passed;
- embedded MCP App JavaScript extraction + `node --check`: passed.

Mocked MCP-App flow:
- normal flow `poll -> claim -> exact sendFollowUpMessage -> widget dedup state -> ACK`: **PASS**;
- second tick after successful send does not resend: **PASS**;
- first `sendFollowUpMessage` failure leaves recovery unacked/pending; focus/reconnect retry sends once and ACKs: **PASS**;
- subsequent retry after successful recovery does not resend: **PASS**.

Final whole-tree regression after self-continuation:
- Rust library: **278 passed, 0 failed, 3 intentionally ignored**;
- Frontend Vitest: **17/17 passed**;
- TypeScript check: passed;
- production Vite build: passed;
- Rust formatting check: passed;
- optimized Rust release build: passed.

### Deployment / live-host limitation

- The currently running RepoTunnel connector in this chat predates this source implementation.
- Therefore the new MCP tools/resource cannot be end-to-end rendered and exercised against the **real ChatGPT MCP App host in this same live session** without a controlled RepoTunnel rebuild/restart.
- The source, server contract, state machine, embedded UI JavaScript, host-message mock flow, full regression, and optimized release build are validated.
- Do **not** install/restart RepoTunnel automatically. A later controlled rebuild/restart can perform the final real-host smoke test (`arm -> render minimal app -> stale/controlled recovery -> same-conversation follow-up`) without disturbing the current working session.

Safety/state:
- No Chrome extension/browser automation/separate recovery desktop app was introduced.
- No `.deb` was built or installed.
- Nothing was staged, committed, pushed, reset, reverted, recloned, or reinstalled.
- `STABLE_CHECKPOINT.md` remains untouched.
