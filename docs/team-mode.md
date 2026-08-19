# Team Mode

RepoTunnel Team Mode attaches two persistent AI engineers to one approved project. The user creates the Team once, each external AI joins one assigned A/B identity once, and those identities remain attached until the user explicitly chooses **End Team** in the RepoTunnel desktop app.

## Persistent workflow

The initial product goal and success criteria form work request #1. RepoTunnel now uses a strict coordination pipeline: both agents join first; both post a concise plan; each proposes one distinct initial implementation task; both confirm the split; only then does RepoTunnel unlock parallel implementation. They claim non-overlapping edit paths, implement at the same time, cross-review, discuss/fix review findings, test and verify the current request. `team_action(action=complete)` completes **only the current work request**; it does not end the Team.

After a request is complete, RepoTunnel keeps the Team active in a ready state. The user can stay in either existing AI chat and ask for a new feature, fix, redesign or improvement. The AI that receives the human instruction posts it into the same Team as a decision message beginning exactly:

`USER REQUEST: <human request>`

RepoTunnel then starts the next work cycle in the same Team. No new Team session, kickoff copy/paste or desktop reconfiguration is required. The cycle repeats until the user explicitly ends the Team from RepoTunnel.

## Collaboration rules

- Both agents must be joined before planning/tasks/implementation unlock.
- Each engineer posts one planning proposal before task creation.
- Each engineer proposes one distinct initial implementation task and both confirm the split before coding starts.
- Both agents may plan, implement, test, debug and review.
- One implementation task has one primary owner.
- Each agent may own only one active implementation task at a time.
- Task owners claim the workspace-relative files/folders they intend to edit.
- Overlapping claims are rejected.
- The other agent should take independent work, inspect/test/review, investigate a blocker, or receive an explicit `handoff_task` rather than duplicate the implementation.
- An implementation owner cannot approve its own task; the other joined agent performs cross-review.
- Normal MCP file mutations require a joined Team identity, an owned in-progress task and matching task-scoped path claims while the Team is active.
- Interactive managed-browser actions use a reserved `@browser` Team lease. Only the lease owner may navigate/click/type/reload/start/stop the shared browser; read-only inspection remains available. Release `@browser` after the test so the other engineer can verify without colliding in the same tab.
- Shell commands may run in parallel when safe, but agents should avoid starting competing fixed-port dev servers.

## Verification and completion

Each work request must have at least one completed cross-reviewed task, useful contribution from both AIs, no open tasks, and verified success criteria with concrete evidence. `team_action(action=complete)` records the completed request and returns the Team to **Ready**. A second completion call racing with the first is treated idempotently.

Only the human can permanently end the Team. **Pause** temporarily blocks Team work; **End Team** permanently detaches the A/B identities from the project.

## External chat limitation

RepoTunnel can persist shared state and keep active MCP clients synchronized, but MCP cannot force an arbitrary completely idle third-party web chat to begin a new generation. Each external AI is joined once. After that, the user does not need to recreate the Team or copy the kickoff again; any new request given to either active AI chat is registered into the same persistent Team.

## MCP surface

Team Mode deliberately uses only two public MCP tools:

- `team_status` — read the persistent Team state and optionally long-poll with `after_revision` + `wait_seconds`.
- `team_action` — create/join/message/task/claim/handoff/review/verify/phase/complete current request.

The public MCP surface is **52 tools** total; Team Mode still uses only `team_status` and `team_action`, while the two additional tools are shared normal/Team workspace bootstrap/security capabilities.

## Balanced two-engineer execution

RepoTunnel generates separate Engineer A and Engineer B kickoff prompts. There is no generic shared kickoff in the Team UI.

For every active work request, both engineers are expected to implement meaningful, non-overlapping product work. The second engineer to join must not sit idle while the first engineer is coding; it should identify remaining independent scope from the goal and success criteria, create/claim a distinct task with non-overlapping paths, and implement it. Review/testing remains mandatory but does not replace each engineer's implementation contribution. A work request cannot complete until at least two distinct cross-reviewed implementation tasks are done and both Engineer A and Engineer B have recorded implementation contribution.

## Fast-path coordination

The goal is throughput, not ceremony. RepoTunnel blocks only the transitions that caused real-world races:

1. Both engineers join.
2. Both post one concise plan.
3. Each creates one distinct initial task.
4. Both confirm the split; RepoTunnel unlocks implementation.
5. A and B implement in parallel.
6. Each cross-reviews the other's task. Review bugs must include concrete feedback and go back to the owner for the smallest correct fix.
7. Interactive browser work is leased to one engineer at a time.
8. After cross-review, evidence-based verification completes the request.

The desktop Team screen intentionally hides the raw live message feed; it shows the workflow stage, engineers, current work, and verification instead. Detailed coordination remains in the persistent Team state for the AIs.
