# AI development workflow

RepoTunnel is designed around one repeatable development loop rather than unrelated tools.

## Recommended sequence

1. **Preflight** — call `list_workspaces`, select the approved project, then call `get_workflow_readiness`.
2. **Inspect** — use `inspect_project`, `search_files`, `list_directory`, and `read_file` before editing.
3. **Edit** — prefer `patch_file` for targeted changes. The public MCP file-tool schema stays compatibility-safe and does not require an edit-group argument.
4. **Automatic request grouping** — when the MCP transport supplies a valid `traceparent` header, RepoTunnel derives an internal group from that trace and saves all mutations from the same AI request into one version. No extra model tool call is required.
5. **Fallback safely** — if a client does not supply a usable request trace, edits still work and are saved as separate protected versions rather than being grouped by time.
6. **Run and verify** — use a disposable command preset when it is sufficient, or `run_terminal_command` for real-workspace commands, dependency installation, scripts, Docker, or other host/network-dependent work.
7. **Keep services alive** — use `start_process` for development servers/watchers, then inspect them with `read_process_output`, `list_processes`, or `get_monitoring_snapshot`.
8. **Test the application** — use `launch_target` for normal desktop launching or the managed browser tools for repeatable UI testing. Start an automation browser, navigate/click/type/reload with `browser_action`, inspect DOM content, capture screenshots, and read console/network diagnostics.
9. **Monitor and iterate** — enable workspace monitoring when project-file changes matter and use `get_monitoring_snapshot` to correlate process output, ports, browser errors, and file activity. Fix and retest until actual results confirm the task.
10. **Review Git** — inspect `git_status` and `git_diff` before staging.
11. **Stage explicit files** — request staging only for the intended paths. In AI Auto, validated staging applies immediately; in AI Review it waits for local approval.
12. **Commit** — request a commit only after staging is confirmed. In AI Auto, a validated commit applies immediately; in AI Review it waits for local approval. A remote push remains a separate action and requires an explicit human instruction to push the current work.

## Readiness levels

- **Ready**: inspection, safe editing, sandboxed verification, and Git completion are all available.
- **Limited**: inspection works, but one or more later steps are intentionally unavailable or need setup. Examples include read-only access, disabled commands, an unavailable native OS sandbox, no discovered command preset, or no supported Git repository.
- **Blocked**: the approved workspace itself cannot be validated safely.

The readiness check is read-only. It does not create test files, execute project code, stage files, or make Git commits.

## Approval boundary

AI Auto applies compatible file/folder mutations, sandboxed live terminal/process starts, structured launches, browser mutations, validated Git staging, and validated Git commits without local confirmation and records the resulting activity. AI Review may queue those mutating actions according to the applicable project policy; MCP cannot self-approve pending Review actions. Git push is separate from mode: it is blocked until the human explicitly asks to push the current work, then AI Auto may perform the normal push without another approval interruption. Monitoring and other observation tools do not require approval.

## Completion rule

An AI should not claim that an edit, command, process start, launch, browser action, test, stage, or commit completed unless the corresponding RepoTunnel result confirms that state. A request that is merely pending is not completion.
