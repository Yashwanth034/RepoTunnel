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

Registered project metadata is stored under the application data directory. Automatic version-history metadata, snapshots, legacy change records, and undo data are persisted locally. Gateway and tunnel runtime state is not persisted.

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


## Private ChatGPT transport

RepoTunnel uses OpenAI Secure MCP Tunnel rather than exposing its local MCP listener publicly. The official `tunnel-client` runs as a managed child process and forwards the OpenAI-hosted tunnel traffic to RepoTunnel over loopback.

This keeps public ingress, TLS termination, and tunnel protocol behavior outside RepoTunnel while preserving the existing Rust workspace authorization boundary.

The Runtime API key is intentionally session-only for now. Persistent credential storage, if added later, must use an operating-system secret store rather than the JSON workspace registry.
