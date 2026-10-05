# Release Acceptance Test

This is the live acceptance checklist for the current RepoTunnel product. It is intentionally version-agnostic: run the applicable sections against the exact candidate build before calling that build ready.

Compilation or unit tests alone are not release acceptance.

## 1. Clean package/install

On each platform available for validation:

- install or run the produced package for that platform
- confirm RepoTunnel opens normally without an unexpected terminal window
- confirm version/architecture diagnostics match the candidate
- confirm runtime logging is created without API keys, credentials, project file contents, or other protected data
- confirm an upgrade preserves the existing application-data directory and registered workspaces

## 2. Startup, shutdown, and update safety

- verify Launch at login behavior on the target OS when supported
- close RepoTunnel with RepoTunnel-owned gateway/provider/process activity and confirm owned connection workers shut down correctly
- relaunch and confirm persisted workspace/security data is readable
- verify updater discovery/signature behavior for the target platform
- exercise the platform-specific update install path only where RepoTunnel currently enables it
- confirm an update failure does not pretend success or erase existing project/security state

## 3. Workspace boundary

Use a disposable project and a separate sibling directory containing test-only secrets.

- approve only the disposable project
- confirm normal approved files can be listed/read
- confirm `../`, absolute paths, protected paths, and symlink escapes are rejected
- confirm removing the workspace immediately revokes its workspace ID
- confirm `request_external_file` cannot browse an arbitrary path and only receives the exact file chosen by the user

## 4. Large-project reads

- verify project-tree paging on a sufficiently large synthetic/real project
- verify large-directory paging
- verify large text range reads at high line offsets
- verify `fast_search_files` continuation cursors
- change a source file and confirm a stale range/search cursor cannot silently continue as current
- confirm protected/ignored content does not leak through large-project tools

## 5. Safe editing and history

In AI Review:

- request a text edit and confirm the real file is unchanged before local approval
- inspect/apply the diff
- confirm history/version state is created
- create a stale approval and confirm RepoTunnel refuses to overwrite newer content

In AI Auto:

- apply a targeted edit
- confirm the change is recorded and restorable when the operation supports safe recovery

For both:

- verify a persistence/version finalization failure is not reported as a clean successful edit
- verify rollback/recovery data is retained conservatively when automatic rollback cannot be confirmed

## 6. Commands and managed processes

### Disposable verification

- discover a supported build/test/check preset
- run it in the disposable native sandbox
- confirm network denial, bounded output/time, and project-copy isolation
- confirm the command cannot read a sibling secret file

### Live terminal/processes

- run a short real-workspace command
- start a managed process
- verify bounded incremental stdout/stderr
- verify `wait_process` can detect expected output/exit without repeated tight polling
- verify stop/restart behavior
- verify credential-like environment overrides/output remain protected

On Linux, explicitly verify full process reattachment across a complete RepoTunnel restart: start a managed process, restart RepoTunnel, confirm the same RepoTunnel process ID/status/output is recovered, then stop it normally. On any platform where the runtime capability is false, do not claim restart reattachment.

## 7. Git and GitHub

- verify bounded status/diff/log
- stage only explicit intended files
- confirm protected/symlink/clean-filtered/secret-bearing paths are blocked
- commit only already-staged state
- confirm changed HEAD/staged fingerprint invalidates stale pending work
- verify safe restore-to-HEAD on an eligible text file
- confirm push is refused unless the current human instruction explicitly authorizes publishing
- when GitHub is connected, confirm supported GitHub operations work without exposing its credential to AI output

## 8. Public MCP, OAuth, and connection providers

For each provider being released/tested (ngrok, Cloudflare, Direct HTTPS, or optional Secure MCP Tunnel as applicable):

- keep the raw MCP origin private
- confirm provider/gateway health is factual
- confirm OAuth/DCR/PKCE flow works where applicable
- confirm refresh/re-auth behavior
- confirm Revocation invalidates remote authorization without deleting projects
- run authenticated MCP discovery
- make at least one real RepoTunnel tool call from the remote client

If the MCP schema changed, refresh/re-scan the client app before treating missing tools as a server failure.

## 9. Browser runtime

- start the managed browser and open a test page
- verify navigation receipt/document generation consistency
- verify DOM/text inspection and screenshots
- verify selector and semantic click/type return persisted mutation receipts without copying typed text into the receipt/history
- verify an acknowledged mutation receipt reports `applied` and includes bounded redacted before/after page-generation evidence
- verify scroll/reload
- verify semantic refs reject stale/sensitive actions
- verify file upload/download routing
- verify console/network diagnostics
- induce a helper transport loss after mutation dispatch and confirm RepoTunnel returns `status=ambiguous`, preserves the mutation receipt/current-state evidence, reconnects only for observation, and does **not** replay the possibly completed action

## 10. Desktop and AI Workspace

On the target OS:

- verify Desktop permission is required
- verify RepoTunnel cannot control itself
- start an allowed AI Workspace app
- inspect/control it through the supported platform path
- verify sensitive typing protection
- verify multi-app/session ownership isolation
- verify cleanup does not terminate another AI-owned app session

Windows/macOS semantic/screenshot behavior must be validated on those native systems before making native-runtime claims.

## 11. Team Mode

- create/join both A/B engineers
- complete the planning/task split gate
- verify non-overlapping path claims
- perform parallel implementation on distinct tasks
- cross-review each task with the other engineer
- verify success criteria with evidence
- complete the current request and confirm the Team remains ready
- confirm only the human can permanently End Team

## 12. Video

### Video Intelligence

- run at least one supported local/public-media analysis path
- verify bounded job progress/content and cancellation

### Video Production

- create/open a Video Project
- verify script/storyboard/timeline persistence
- run the relevant generated-scene/narration/render/preview path
- verify final render remains gated until required QA passes
- verify cleanup preserves current/final/source/license/QA-linked assets
- verify external asset provenance rules
- when story engines are claimed, validate the actual installed engine path rather than only structural tests

## 13. Phone Access

Using a non-sensitive test phone/workflow:

- verify selected-device identity and Full/Limited/Off enforcement
- verify wireless runtime and authorized USB fallback behavior when available
- verify live screen and guarded input
- verify rapid sequence and semantic transaction paths
- verify app/file/APK/network/shell/log capabilities appropriate to the selected access mode
- verify helper integrity/status
- verify stale semantic refs and wrong-foreground guards fail closed
- verify generic Phone shell cannot bypass dedicated UI/screen guards
- verify Android device-policy denial is reported factually rather than treated as RepoTunnel success

For payment-sensitive foreground protection, use only non-financial safety validation. Do **not** test balances, PINs, OTPs, cards, or real financial transactions.

## 14. Continuity and recovery

- verify `get_resume_snapshot` reflects live Git/process/activity state rather than stale saved intent
- verify Project Memory survives restart/reconnect as designed
- verify old failures do not override newer successful evidence
- verify current unresolved failures still surface attention
- exercise MCP App self-continuation only in a controlled supported host and confirm dedup/ack behavior

Do not claim RepoTunnel can detect every ChatGPT interruption/session end; the runtime does not advertise that guarantee.

## 15. Final regression gates

Run the repository's release validation plus applicable native/live tests.

At minimum:

- TypeScript check
- frontend tests
- production frontend build
- npm audit
- Rust formatting
- Cargo check
- Clippy with warnings denied
- Rust library tests
- RustSec audit according to release policy
- `git diff --check`

## Pass criteria

A release passes only when:

- no tested operation escapes its approved security boundary
- no remote MCP call can self-approve a local Review action
- secrets/credentials are not exposed through logs/tool output
- project/app-data state survives the intended update/restart path
- the tested connection path is authenticated
- the complete AI → RepoTunnel → approved resource → verification workflow succeeds
- every claimed platform/device capability has matching evidence from the platform/device on which that claim is made
