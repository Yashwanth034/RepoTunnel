# Release Acceptance Test

This is the final live test required before RepoTunnel v0.1.0 should be considered ready for general use.

## 1. Clean install

- Build a Linux bundle from a supported baseline environment.
- Install the `.deb` or `.rpm`, or run the AppImage.
- Confirm RepoTunnel opens without a terminal window.
- Confirm the Runtime diagnostics panel reports the correct version and architecture.
- Confirm a log file is created inside RepoTunnel application data and contains no API keys or file contents.

## 2. Startup and shutdown

- Enable **Launch at login** and verify the XDG autostart entry is created.
- Disable it and verify the entry is removed.
- Close RepoTunnel while the local gateway and tunnel are active.
- Confirm the `tunnel-client` child process exits and the local MCP port closes.
- Relaunch and confirm RepoTunnel starts disconnected and does not remember the Runtime API key.

## 3. Workspace boundary

Use a disposable Git project plus a separate sibling directory containing test secrets.

- Add only the disposable project.
- Confirm normal source files can be listed/read.
- Confirm `../`, absolute paths, protected files, and symlink escapes are rejected.
- Confirm removing the workspace immediately revokes its workspace ID.

## 4. Safe editing

In **Review changes** mode:

- Ask the AI to edit a text file.
- Confirm the real file is unchanged until local approval.
- Review the diff and approve it.
- Confirm history shows the applied change.
- Undo it and confirm the file returns to the expected content.
- Create a stale approval by changing the file manually before approving; confirm RepoTunnel refuses to overwrite the newer file.

In **Auto apply** mode:

- Apply a small edit.
- Confirm history and undo information are still created.

## 5. Sandboxed verification

- Install Bubblewrap.
- Use a project with a supported build/test script.
- Confirm RepoTunnel discovers the preset.
- Run it and confirm output, exit status, timeout handling, and history.
- Confirm the command cannot access a test file outside the disposable project and cannot use the network.
- Confirm writes produced by the command occur only in the disposable sandbox copy.

## 6. Git workflow

- Confirm Git status/diff/log work.
- Request staging of an explicit normal source file.
- Confirm staging waits for local approval.
- Approve staging, then request a commit.
- Confirm the commit uses only already-staged changes and waits for local approval.
- Confirm stale staged state or changed HEAD invalidates a pending commit.
- Confirm protected files and Git clean-filtered files cannot be staged through RepoTunnel.

## 7. ChatGPT end-to-end

- Create/associate an OpenAI Secure MCP Tunnel.
- Start RepoTunnel's ChatGPT connection with the matching tunnel ID and Runtime API key.
- In ChatGPT Developer mode, enable the RepoTunnel MCP app using that tunnel.
- Ask ChatGPT to run the workflow readiness check.
- Ask it to inspect the disposable project and explain its architecture.
- Ask it to fix a small intentional bug that requires changing at least three files.
- Confirm the public MCP file tools do not expose `begin_edit_group`, `complete_edit_group`, or `edit_group_id`.
- Confirm multiple mutation calls from one ChatGPT user request share the same `traceparent` trace ID and appear as one RepoTunnel history version.
- Confirm a later user request receives a different trace ID and creates a separate version.
- Introduce a delay longer than four seconds between two of the file mutations and confirm the whole request still appears as exactly one RepoTunnel history version.
- Ask a second edit request and confirm it creates a separate history version.
- Confirm Previous, Next, Restore this version, and Restore original all reproduce the expected project contents while later versions remain available.
- Ask it to run the discovered verification command.
- Ask it to inspect Git diff, request staging, and request a commit.
- Approve both Git actions locally.
- Confirm the final commit contains exactly the intended files.

## Pass criteria

Release passes only when no operation escapes the approved workspace, no remote MCP call can self-approve a privileged action, no Runtime API key is persisted, shutdown leaves no managed tunnel/gateway process alive, and the complete ChatGPT → RepoTunnel → project → verification → Git workflow succeeds.
