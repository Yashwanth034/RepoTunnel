# Fast Chrome automation for RepoTunnel agents

This is a source implementation and benchmarking guide, not a claim that Chrome renders faster or that any third-party benchmark was reproduced on RepoTunnel.

## Use the existing high-throughput path

For an agent doing several steps within one managed Chrome tab, prefer the MCP `browser_transaction` tool (or `browser_fill_form` for multi-field forms) over separate calls for each `click`, `type`, `wait` and `verify`. `browser_transaction` validates the bounded 1–64-step plan, resolves semantic labels and DOM locators as needed, executes it in the managed tab through the persistent local bridge, and returns structured results in one agent turn. Use a fresh `browser_semantic_snapshot` and signed refs where dynamic widgets demand accessibility targeting; avoid unnecessary repeated full-page snapshots. Semantic sequences are also bounded and preserve stale-target checks.

RepoTunnel already supported persistent Chrome DevTools sockets, per-project tab ownership, multi-action transactions, safe sensitive-field denial, human approval/AI Review, mutation receipts and no mutation replay after transport ambiguity. **None of those features was newly invented by this optimization.** Existing Chrome users and Linux sessions are preserved; there is no change to browser profile migration or login sharing.

## Implemented source performance optimizations

- **CDP hot-target reuse:** after a tab ID has been discovered and its authenticated WebSocket is alive, use that socket directly. Do not fetch `/json/list` before every CDP method. On socket close/error/target restart, use the existing target discovery/reconnection path. This removes redundant local HTTP requests on warm runs.
- **Parallel semantic snapshot collection:** issue independent `Accessibility.enable` and `DOM.enable` commands concurrently, then issue the full accessibility tree, piercing DOM document and frame tree concurrently. Their original CDP return values still flow through the same sensitive-field redaction, signed backend-node refs, stale-document and app/tab ownership logic. A failed domain request causes failure; it does not fall back to arbitrary page JavaScript.
- **Single-call task batching:** the agent-facing `browser_transaction` description now explicitly recommends it for multi-step tests rather than serial tool calls. No general unrestricted JS-execute command was exposed to agents.
- **Regression gates:** `npm run test:browser` checks the browser transaction runtime and concurrent fast-path contract. The native source-only CI validates those Node tests and embeds/refreshes all three browser helper files in packaged Rust builds.

## What Ego Lite does that stock Chrome cannot simply copy

Ego Lite's published agent benchmark measures completed multi-step workflows and model/tool overhead, **not browser rendering speed**. Its custom Chromium build supplies semantic snapshots that can include nested third-party content unavailable to a script running in the top document. RepoTunnel uses managed stock Chrome and CDP; cross-origin out-of-process iframe coverage requires per-target CDP session ownership, permissions and security review. Never claim equivalent deep iframe access based on `DOM.getDocument({ pierce: true })` alone.

Do **not** migrate the user's personal Chrome passwords, cookies or regular profile into an agent test session automatically. Keep RepoTunnel's existing local approved-workspace identity, isolated browser profiles, Team owner checks, explicit consent for optional shared logins, and audit history. Preserve the deterministic transaction/semantic fallback on failures.

## Local Chrome transaction spot check (2026-10-23)

A managed Google Chrome session exercised an ephemeral HTTP fixture bound to `127.0.0.1` with two text fields, a button and a verified status output. One four-step transaction completed in **32 ms** of RepoTunnel internal time. A subsequent 64-step transaction completed **16 four-step cycles** in **419 ms** internal time, with all 64 steps completed. Per-cycle sums of reported step timings were **p50 26 ms** and **p95 34 ms**; those sums are not the end-to-end latency of separate MCP calls. The fixture's managed browser and local HTTP process were stopped afterward.

These are observations of the **already optimized** bridge on one local fixture, not an A/B performance comparison or a measured speedup. RepoTunnel's `repoTunnelInternalMs` excludes the external ChatGPT/MCP round trip, and the sample does not establish cold-launch, cross-origin-frame or production-site performance. The full repeatable before/after acceptance defined below remains outstanding.

## Real performance acceptance (not yet run)

Use the *same* managed Chrome version, machine, local deterministic test page, approved test account and model for before/after comparisons. Include a cold launch and 20 warm runs per scenario: (1) simple navigate/inspect; (2) 10 semantic snapshot + action cycles; (3) 10-step form fill/verify; (4) a React SPA rerender and navigation-staleness case; (5) a cross-origin iframe requiring supported fallback. Compare p50/p95 wall time, CDP request count, model/tool round trips, input tokens, first-try completion rate and denied/ambiguous mutations. Report all failures; never exclude privacy denials to improve speed statistics.

**Acceptance requirement:** the optimized bridge must preserve signed semantic refs, sensitive text redaction, document-generation validation, tab/workspace isolation and fail-closed mutation behavior. Third-party vendor speed numbers are not RepoTunnel speed results.

References: https://lite.ego.app/ , https://lite.ego.app/document/en/docs/skills , https://github.com/citrolabs/ego-browser-benchmark-framework .
