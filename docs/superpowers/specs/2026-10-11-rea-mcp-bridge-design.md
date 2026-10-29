# REA MCP Bridge Design

## Goal

Expose REA reverse-engineering capabilities to AI clients through RepoTunnel without changing existing UI, agent pages, History, Commands, Connect behavior, or unrelated code.

## Scope

Add an isolated backend-only bridge for REA. RepoTunnel remains fully functional when REA is missing, incompatible, stopped, or broken.

## Architecture

```text
AI client
   |
   v
RepoTunnel MCP
   |
   v
REA bridge (new isolated module)
   |
   v
REA MCP process
```

The bridge owns REA detection, process startup, MCP discovery/calls, bounded responses, and shutdown. Existing RepoTunnel features do not depend on it.

## Public MCP surface

Add only three RepoTunnel MCP tools:

- `rea_status(workspace_id)` — read-only availability/prerequisite status.
- `rea_list_tools(workspace_id)` — lazily starts REA when possible and returns the current REA MCP tool catalog.
- `rea_call(workspace_id, tool_name, arguments)` — invokes one tool from the discovered REA catalog.

The AI can discover and call REA without a RepoTunnel UI control. REA is started only on demand.

## Isolation rules

- No frontend files are modified.
- No existing navigation, GPT Agents, History, Commands, Connect, browser, phone, continuity, or project-memory behavior is modified.
- REA failures must return structured errors and must never prevent RepoTunnel startup or MCP operation.
- File targets passed to REA must resolve inside the selected approved workspace or RepoTunnel-owned temporary task paths. HTTP(S) URLs and literal loopback CDP/Inspector endpoints may pass through when required by the selected REA tool; absolute host file paths outside the approved workspace are rejected.
- Existing RepoTunnel secret redaction and workspace approval boundaries remain authoritative.
- Do not expose arbitrary host filesystem paths through REA.

## Runtime and prerequisites

Current REA package: `rea-agents` 6.3.0, MIT licensed.

REA 6.3.0 requires Node `^22.19.0 || ^24.11.0 || >=26.0.0`. The current host Node is 20.20.2, so the first implementation must detect this incompatibility and report it cleanly rather than modifying the user's system Node installation. When a compatible Node/npm is available, the bridge launches the exact pinned package through `npx -y rea-agents@6.3.0 mcp`; no global REA installation is required.

Optional REA backends such as JADX, Ghidra, Hopper, IDA, browser/CDP, and other analyzers remain REA-owned capabilities. RepoTunnel reports their availability through REA rather than reimplementing them.

## Process lifecycle

- No permanent background daemon.
- First `rea_list_tools` or `rea_call` starts one REA MCP child for the requesting RepoTunnel runtime when prerequisites are satisfied.
- Reuse the healthy child for later calls.
- Detect child exit and allow a later request to restart it.
- Bound startup/call timeouts and output sizes.
- RepoTunnel shutdown terminates the owned REA child.

## MCP protocol behavior

The bridge speaks MCP to REA over its documented stdio server entry point (`rea mcp`). It reuses RepoTunnel's existing official Rust MCP SDK (`rmcp`) with its client + child-process transport features rather than implementing MCP framing manually. Tool names and schemas are discovered dynamically; RepoTunnel does not hard-code REA's internal reverse-engineering tool catalog.

`rea_call` must reject tool names that were not returned by the current REA `tools/list` result.

## Security

- REA is optional third-party tooling; it receives only explicit approved-workspace targets supplied through the bridge.
- No automatic download of arbitrary APKs/applications is part of this feature. Existing RepoTunnel browser/terminal/file workflows may obtain a target separately when allowed, then pass an approved workspace-relative target to REA.
- Secret-like output is redacted before being returned to the AI.
- No credentials are injected into the REA process.

## Testing

Use TDD. Tests must cover:

1. unsupported Node version returns unavailable status without starting a process;
2. missing REA executable/package returns unavailable status without affecting RepoTunnel;
3. successful mocked MCP initialize + tools/list discovery;
4. `rea_call` rejects unknown tool names;
5. successful mocked tool invocation;
6. malformed/oversized REA responses fail safely;
7. crashed child can be restarted on a later call;
8. existing RepoTunnel tests remain green.

## Files

Expected implementation scope only:

- Create `src-tauri/src/rea_bridge.rs`.
- Modify `src-tauri/src/lib.rs` only to register the new module.
- Modify `src-tauri/src/mcp_server.rs` only for the three MCP tool parameter types and handlers.
- Modify `src-tauri/Cargo.toml` only to enable the existing `rmcp` client/child-process transport features needed by the bridge.
- Update `src-tauri/Cargo.lock` only if Cargo changes it as a consequence of that feature activation.
- Add focused tests next to the bridge/module using existing Rust test conventions.

No other production files should be changed unless implementation reveals a hard dependency; if that happens, stop and get approval before expanding scope.

## Success criteria

- RepoTunnel behavior is unchanged when REA is absent.
- No UI changes.
- AI clients can discover REA availability through RepoTunnel MCP.
- On a compatible machine with REA installed, AI can dynamically list and call REA tools.
- REA process lifecycle is lazy, bounded, recoverable, and isolated.
- Unrelated existing working-tree changes remain untouched.
