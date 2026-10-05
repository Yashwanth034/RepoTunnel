# Technical Decisions

## Desktop stack

RepoTunnel uses Tauri 2 with React and TypeScript.

Reasoning:

- Tauri keeps the privileged backend in Rust.
- The frontend can remain small and familiar.
- Linux is a first-class desktop target.
- The security boundary can remain outside the web UI.

## Filesystem ownership

Privileged filesystem operations belong in Rust rather than the React frontend.

The UI and MCP server request specific backend operations; neither receives unrestricted filesystem APIs.

## Folder selection

The native folder picker is invoked from a dedicated Rust command. This avoids granting the frontend a general filesystem capability merely to choose a project directory.

## Workspace persistence

Registered project metadata is stored under the application data directory. Version-history metadata, snapshots/deltas, change records, undo/recovery data, Project Memory, and applicable connection/provider configuration are persisted locally. Ephemeral runtime handles such as active listener sockets, child-process handles, and in-memory provider sessions are recreated rather than serialized as live runtime state.

## MCP SDK

RepoTunnel uses the official Rust Model Context Protocol SDK (`rmcp`) rather than implementing JSON-RPC/MCP wire behavior manually.

The dependency is pinned to the current 3.1 release line and uses the Streamable HTTP server transport with JSON responses for simple request/response tools.

## Local listener

The gateway binds to `127.0.0.1` on an operating-system-assigned port. It exposes `/health` and `/mcp` and is started/stopped by the Tauri application.

The listener stays private to the machine. Remote connectivity is handled as a separate connection layer rather than binding the service to `0.0.0.0`.

## Tool model

Model Context Protocol is the external tool protocol.

RepoTunnel exposes narrow, action-oriented tools. A generic `execute_anything` or arbitrary-path filesystem tool is intentionally excluded.

## Path model

MCP tools address files through:

- a RepoTunnel workspace identifier
- a relative path inside that workspace

Absolute local workspace paths are not returned by the MCP workspace-discovery tool. This reduces unnecessary local-path disclosure and keeps authorization consistent.

## Shared enforcement

MCP and Tauri read operations delegate to the same filesystem engine. Mutation requests from both surfaces delegate to the same safe-editing manager, which then calls the filesystem engine after review-policy and backup handling. There is no protocol-specific filesystem implementation.

This prevents the MCP boundary from accidentally diverging from path traversal, protected-file, symlink, size, read-only, review-policy, backup, or overwrite protections.


## Safe-editing policy

New workspaces default to `review` mode. This is deliberately separate from read-only/read-write access: access answers whether a write is permitted at all, while the change policy answers whether a permitted write is queued for local approval or applied automatically.

Approval, rejection, and undo remain desktop-only actions. MCP can inspect recent change status but cannot approve its own pending mutations. Automatic mode is available for users who prefer direct chat-to-project editing, and it still records history and creates conservative undo data when safe.

Pending payloads and undo data are stored outside the repository in application data, using atomic writes and owner-only file permissions on Unix.

## Runtime isolation

Potentially blocking filesystem work is executed with Tokio blocking workers instead of directly on MCP HTTP runtime threads.

## Protocol isolation

The MCP transport layer and local workspace engine remain separate modules. MCP SDK or transport changes can be made without rewriting filesystem authorization.

## MCP read/write signaling

Read-only tools publish `readOnlyHint`; mutation tools remain classified as writes. Client confirmation behavior is an additional safety layer and never replaces RepoTunnel's local Rust permission checks.


## Remote connection transports

The raw MCP gateway stays private to the machine. Remote connectivity is a separate layer with multiple supported provider choices rather than one privileged transport.

RepoTunnel currently supports managed ngrok, Cloudflare Tunnel, Direct HTTPS, and an optional official OpenAI Secure MCP Tunnel path. Public MCP access keeps RepoTunnel's OAuth/workspace authorization boundary regardless of provider.

Direct HTTPS owns its TLS/ACME frontend while proxying only allowlisted routes to the trusted local origin. Cloudflare and Direct HTTPS use RepoTunnel's stable local origin path. ngrok uses the Rust SDK. The optional OpenAI `tunnel-client` remains a managed child process.

Provider/authentication credentials are never general MCP inputs. The Secure MCP Tunnel Runtime API key remains session-only and is passed to `tunnel-client` through its child environment rather than command-line arguments or workspace JSON.
