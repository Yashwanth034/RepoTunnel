# Command execution

RepoTunnel provides both disposable sandbox verification and real-workspace terminal/process control. The two paths are separate so safe verification remains available without limiting full AI development workflows.

## Command policy

Each approved workspace has an independent command policy:

- `review` — an AI may request a discovered command, but it remains pending until the user approves it in the desktop application.
- `automatic` — discovered commands may run immediately.
- `disabled` — command execution is blocked for the workspace.

The default is `review`.

## Disposable verification presets

The sandbox path does not accept arbitrary command strings. It derives bounded presets from project manifests and currently supports:

- safe-named package scripts such as build, test, lint, check, typecheck, verify, and validate for npm, pnpm, yarn, or Bun projects
- `cargo check`, `cargo test`, and `cargo build`
- Python pytest and unittest discovery
- `go test ./...` and `go build ./...`

Deployment, release, publish, development-server, and arbitrary shell presets are not discovered.

## Native OS sandbox

RepoTunnel refuses AI command execution when the required native sandbox is unavailable. The backend is OS-specific:

- **Linux:** the existing Bubblewrap namespace/mount sandbox remains unchanged.
- **Windows:** an ephemeral AppContainer profile is attached to a kill-on-close Job Object. The AppContainer SID receives only command-lifetime ACL access to the approved writable/project paths and required read-only tool roots. Cleanup removes those ACEs/profile state after exit; recovery manifests handle abnormal termination without revoking access from a still-running helper.
- **macOS:** a fail-closed Seatbelt profile is applied with `/usr/bin/sandbox-exec`. This is a compatibility backend because Apple has deprecated that interface; a signed App Sandbox/XPC helper is the long-term backend.

Disposable verification commands deny network access on every supported backend. Live terminal/process commands may use network access. On Windows, AppContainer network isolation still blocks host `localhost` interoperability by default; RepoTunnel does not silently create an administrator-level loopback exemption.

### Linux Bubblewrap details

RepoTunnel probes Bubblewrap before enabling commands and refuses execution if the required Linux namespaces are unavailable.

Each command receives:

- a new mount, PID, IPC, UTS, cgroup, user, and network namespace through Bubblewrap
- no host network access
- a cleared environment with only minimal non-secret variables restored
- no stdin
- a bounded execution timeout
- bounded stdout/stderr capture
- a disposable project working copy instead of the real workspace

The sandbox receives read-only system runtime directories needed to launch normal toolchains. User-home toolchain mounts are narrowed to required binaries and dependency caches; credential files are not mounted.

## Disposable project copy

RepoTunnel prepares a temporary working tree using the same project intelligence and access guard used by filesystem tools:

- ignored/generated dependency and build folders are not copied as normal source
- symbolic links are skipped
- protected secret paths are skipped
- per-file, total-copy, and entry-count limits are enforced

Large dependency directories such as `node_modules` and local Python virtual environments may be mounted read-only from the approved project so tests can reuse installed dependencies without granting access to unrelated host files.

All writes made by a command occur only inside the disposable copy or temporary build/cache locations. The temporary tree is deleted after execution, so a build/test command cannot silently mutate the real repository.

## History and approval

RepoTunnel persists command records in its application data directory. Records contain the workspace, preset, displayed command, status, duration, exit code, bounded stdout/stderr, and errors.

Pending command definitions include an internal fingerprint. Before local approval, RepoTunnel re-discovers the preset and refuses to run it if its definition changed after the request was created.

MCP exposes command request and history tools, but it intentionally does not expose command approval or rejection. Those actions stay local in the desktop application.

## Live terminal and persistent processes

The separate AI live execution path accepts shell commands against the real approved workspace, but runs them inside RepoTunnel's fail-closed native OS sandbox with a cleared/sanitized environment. Linux uses Bubblewrap, Windows uses AppContainer + Job Object isolation, and macOS uses the Seatbelt compatibility backend described above. Project writes and permitted network access persist while unrelated host files and credential-like environment values remain outside the AI command boundary. RepoTunnel blocks the AI command if the native sandbox is unavailable instead of silently falling back to unrestricted host access. Narrow `gh workflow`/`gh run` operations and explicitly user-authorized normal `git push` use controlled host passthrough so existing GitHub authentication can be used without exposing credential files to the AI shell.

One-shot commands are recorded with bounded output, exit status, duration, and errors. Persistent processes are managed by RepoTunnel, capture stdout/stderr for later reads, expose running/exited/stopped state, and can be stopped or restarted through MCP.

In AI Auto, live commands and process starts execute without local confirmation. In AI Review, they follow the workspace command policy and may remain pending for local Accept/Reject. MCP cannot self-approve pending Review actions.

Pause AI is the emergency stop for active live execution and managed process groups.

The disposable sandbox remains the preferred option when a build/test/check/lint preset is sufficient and persistent host/workspace side effects are not needed.
