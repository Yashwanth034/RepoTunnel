# Product Definition

## Purpose

RepoTunnel gives AI clients controlled access to local software projects selected by the user. It removes the need to repeatedly archive, upload, edit, download, and replace project files when working with an AI assistant.

## Primary workflow

1. The user opens RepoTunnel.
2. The user selects a local project folder through the native desktop picker.
3. RepoTunnel validates and stores the canonical project path locally.
4. The user enables the local MCP gateway.
5. For ChatGPT, RepoTunnel launches the official OpenAI `tunnel-client` with the selected tunnel ID and the current local MCP endpoint.
6. The user selects the same tunnel in a ChatGPT Developer mode app.
7. The AI discovers approved workspaces through `list_workspaces`.
8. The AI requests narrowly scoped operations such as listing, searching, reading, or editing project files.
9. RepoTunnel validates every operation against the current approved workspace registry and path policy.
10. Read operations execute immediately. Write operations either enter local review or apply automatically according to the workspace change policy.
11. RepoTunnel records the change, stores a local undo point when safe, and returns whether the write was actually applied.

## Desktop capabilities

The application currently supports:

- adding project folders
- removing registered projects without deleting their contents
- persisting the registered project list between launches
- choosing read-only or read-write access per project
- automatic versioned AI edits with reversible local history
- reviewing pending writes and their diffs
- approving, rejecting, and undoing supported changes
- viewing persistent local change history
- starting and stopping a loopback-only MCP gateway
- showing the local MCP endpoint and project count
- detecting the official OpenAI `tunnel-client`
- starting/stopping the Secure MCP Tunnel transport for ChatGPT
- monitoring tunnel readiness instead of assuming a launched process is connected
- keeping the Runtime API key session-only and out of process arguments
- reporting initialization and operation errors in the interface

## MCP capabilities

The local MCP server currently exposes focused tools for:

- discovering approved workspaces
- listing directories
- reading text files
- searching project text
- creating files and folders
- full-file writes
- exact-context patching
- moving and renaming files/folders
- deleting files/folders with backend safeguards
- retrieving file metadata
- reading recent change-history status without exposing local approval controls

## Later capabilities

- controlled terminal execution
- test and build commands
- Git status, diff, restore, branch, and commit workflows
- connection diagnostics and recovery
- packaging and update support

## Non-goals

RepoTunnel will not:

- expose the entire computer by default
- silently expand workspace permissions
- bypass operating-system permissions
- bind its local filesystem service to a public network interface
- treat arbitrary shell execution as equivalent to file access
- send a complete repository when a smaller response is sufficient
