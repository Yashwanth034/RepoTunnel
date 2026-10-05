# MCP contract

RepoTunnel exposes a capability-oriented MCP surface over its authenticated connection paths. The tool contract is a compatibility boundary: internal storage/UI refactors should not casually rename tools or change required parameter meaning.

Do not rely on a hard-coded total tool count. RepoTunnel has expanded well beyond its early file-only surface, and clients that cache tool discovery should refresh actions when the schema changes.

## Workspace and project inspection

Core project discovery and readiness tools include:

- `list_workspaces`
- `get_workflow_readiness`
- project setup/memory/continuity inspection

For large repositories, prefer the bounded paging/cursor tools:

- `inspect_project_page`
- `list_directory_page`
- `read_file_range`
- `fast_search_files`

Legacy bounded inspection/search tools remain available for compatibility where appropriate.

Absolute workspace paths are not part of the normal AI workspace-discovery contract.

## File mutation and history

RepoTunnel exposes focused workspace-relative file/folder mutations rather than arbitrary host filesystem access.

Mutations pass through the same access and safe-editing layers used by the desktop app. Results distinguish an applied operation from a queued AI Review request.

MCP cannot approve/reject/undo its own pending local Review actions.

## Terminal and managed processes

The live execution surface includes one-shot terminal commands and durable managed processes.

Important operations include:

- `run_terminal_command`
- terminal history
- `start_process`
- process listing/status
- `read_process_output`
- `wait_process`
- stop/restart operations

Long-running builds, tests, dev servers, and workers should use managed processes rather than holding one MCP request open.

A managed process survives ordinary MCP/UI reconnects through RepoTunnel's durable supervisor model. On Linux, the runtime capability also advertises complete RepoTunnel-process restart reattachment: persisted supervisor state is revalidated and the same managed-process record/logs are recovered. Windows/macOS still report this capability as false until equivalent native identity/control and acceptance coverage exists.

## Sandboxed verification

RepoTunnel separately exposes discovered build/test/check presets that run in disposable, network-disabled project copies.

This verification path is intentionally different from the real-workspace live terminal path.

## Git and GitHub

The MCP surface supports bounded Git inspection plus RepoTunnel-validated staging, commit, and safe restore flows.

Git push remains separate: it requires a current explicit human instruction to publish the work.

When GitHub is connected in RepoTunnel, supported GitHub/Git workflows can use the trusted RepoTunnel connection without exposing the credential to the AI.

## Browser runtime

RepoTunnel's managed browser surface includes:

- browser/session status and tabs
- navigation/action control
- DOM/text inspection and screenshots
- console/network diagnostics
- browser context/profile handling
- downloads and file upload
- semantic snapshots/find/actions/sequences with short-lived refs
- atomic navigation receipts and document-generation consistency
- mutation receipts for selector/semantic click and type actions and semantic sequences
- successful network-history metadata

A mutation receipt contains a unique mutation ID, helper acknowledgement, redacted before/after URL, before/after document generation, and a document-changed signal. If the browser-helper transport is lost after dispatch, RepoTunnel returns the browser action with `status=ambiguous` and never automatically replays that state-changing action. Clients should inspect the receipt and current page before deciding whether another mutation is necessary. Typed text is not copied into the receipt.

RepoTunnel intentionally does not currently expose raw secret-header persistence, arbitrary response-body capture, WebSocket-frame capture, or a navigation scope allowlist.

## Desktop and AI Workspace

Desktop/AI Workspace tools operate through RepoTunnel's explicit Desktop permission and platform adapters.

AI Workspace is an isolated virtual desktop that can host multiple bounded app sessions. Semantic tools reuse short-lived refs and sensitive-field protections.

RepoTunnel blocks AI control of RepoTunnel itself.

## Team Mode

Team Mode deliberately uses a compact coordination surface:

- `team_status`
- `team_action`

The persistent A/B team model, task ownership, path claims, cross-review, and success-criterion evidence are enforced behind those tools. Do not hard-code a total RepoTunnel MCP tool count; the broader capability surface evolves independently.

## Phone Access

Phone tools are capability-gated and target an opaque selected device identity.

The surface includes status, live screen/control, rapid sequences, semantic transactions, app/file/package operations, settings availability, shell/log/network diagnostics, and helper status/install actions.

MCP cannot pair/select a phone, enable Full/Limited access, alter capability grants, or unpause Phone Access.

Payment-sensitive foreground apps block AI Phone observation/control even while the accessibility service remains enabled.

## Video

RepoTunnel exposes both Video Intelligence and Video Production capabilities.

Video Intelligence uses background jobs for transcript/visual/tutorial/full analysis.

Video Production exposes durable Video Projects, assets/licenses, recording, narration/subtitles, generated scenes/diagrams, rendering jobs, pipeline status, QA, cleanup, and story-animation capabilities.

## Temporary and media middleware

RepoTunnel-owned temporary workspaces provide a scoped place for downloads, extracted/generated media, frames, and intermediate artifacts. Cleanup is marker-owned and cannot target arbitrary project files.

Media inspection, frame extraction, and decode validation are available as bounded helpers.

## Continuity and Project Memory

RepoTunnel exposes read-only continuity/resume information and bounded Project Memory updates. Factual activity, Git/process state, and saved semantic intent are kept distinct.

MCP App self-continuation support exists, but RepoTunnel does not claim a general automatic detector for every ChatGPT session ending/interruption.

## AI modes

### AI Auto

Compatible mutations and actions can execute without repeated RepoTunnel approval prompts while all workspace, secret, sandbox, Phone, browser, and publish boundaries remain active.

### AI Review

Mutating actions may be queued for local Accept/Reject according to the applicable policy.

MCP cannot self-approve those queued actions.

### Pause AI

Pause AI is the emergency stop/boundary for RepoTunnel-managed AI activity.

## Compatibility rule

Prefer extending behavior behind existing coherent tools when practical. Adding/removing/renaming MCP tools or changing required parameter semantics is an explicit compatibility event.

Clients may cache MCP schemas. After a schema change, refresh/re-scan the RepoTunnel app/connector before concluding that a new tool is missing.
