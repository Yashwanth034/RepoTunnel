# RepoTunnel continuity and capability discovery: incremental integration

## Why this change exists

A new AI chat should not need to rediscover how a project was set up, repeat
previously solved problems, or ask for test credentials already configured in
an approved runtime. Similarly, one failed tool invocation is not evidence
that RepoTunnel cannot perform the requested action.

These improvements **reuse existing systems**. They do not create a separate
memory database, project runtime, credential vault or tool registry.

## Existing systems retained

- **Project memory**: \`project_memory.rs\`, persisted in RepoTunnel app data
  \`project-memory.json\`. Ordinary \`update_project_memory\` continues to
  update the summary, goals, decisions, preferences and next steps.
- **Continuity Resume v2**: \`get_resume_snapshot\` continues to build its
  factual live Git, managed-process, workflow, and activity summary.
- **Tool discovery**: \`capabilities\`, \`list_tool_capabilities\`,
  \`get_environment_diagnostics\`, \`get_project_setup\`,
  \`list_deep_integrations\`, \`rea_list_tools\` and
  \`search_mcp_registry\` remain the same public tools.
- **Permissions**: Existing workspace and application permissions are unchanged.
  Saved knowledge is never a replacement for current authorization.

## Durable project knowledge (same memory JSON file)

The backward-compatible \`knowledge\` array stores typed, key-addressed,
non-secret items. Older project-memory files deserialize with an empty array.

Kinds:

- \`instruction\`: Explicitly supplied project constraints. AI suggestions are
  not automatically elevated to human authority.
- \`test_procedure\`: Repeatable testing steps, environment, conditions and
  observed test receipts.
- \`resource_reference\`: What approved session/account/mount/config may be
  reusable and how to verify its live availability. Never authentication tokens,
  passwords, session cookie contents or other credentials.
- \`known_pitfall\`: Previously attempted failed, abandoned or prohibitively
  expensive approaches with the reason to avoid repeating them.
- \`capability_route\`: A legitimate existing tool/workflow plus acceptable
  fallback routes when a first tool fails.

Each record has \`recorded\`, \`needs_recheck\` or \`retired\` state; a
\`needs_recheck\` item does not certify that a saved session still works. The
\`retired\` state prevents an abandoned idea being taken as current guidance.

### MCP behavior

- \`upsert_project_knowledge\` updates one stable key and preserves the other
  project-memory fields. Retrying the same content is idempotent. Limits reject
  oversized entries rather than silently truncating them; secret scanning is
  mandatory. The existing project-memory mutation mutex guards concurrent
  writes from \`update_project_memory\` and \`upsert_project_knowledge\`.
- \`find_project_knowledge\` provides bounded, ranked multi-word search,
  optional kind filter and explicit inclusion of retired entries.
- \`get_project_memory\` still returns the original fields, plus structured
  knowledge. \`get_resume_snapshot\` includes up to eight small prioritized
  knowledge hints and links to the full retrieval tool.
- Existing summaries do not silently erase typed knowledge, and adding a
  knowledge entry does not refresh the saved summary's Git/activity freshness
  markers.

### Fresh-chat example (illustrative only)

A new agent receives an approved project ID, calls \`get_resume_snapshot\`,
then \`find_project_knowledge\` for the active task, test resources and known
pitfalls. A saved note might say "previously used a Telegram testing session;
verify the current authorized session before requesting a fresh login."
The agent checks the appropriate permitted browser/AI Workspace/terminal
resource; if it is valid, it reuses it. If it is missing, paused or forbidden,
the agent reports the exact blocker rather than pretending it can access it.
An abandoned performance benchmark remains a known pitfall instead of being
repeated because of stale chat context.

No Telegram session or TeraRelay project files were read or modified while
adding this RepoTunnel functionality.

## Existing capability discovery: fallback routes

The existing \`capabilities\` response now also includes a small
\`discoveryRoutes\` list for commonly needed tasks:

1. Resume prior work and test procedures.
2. Browser testing with existing controlled tabs and diagnostics.
3. AI Workspace and desktop application interaction.
4. Host vs sandbox dependency and SDK mismatches.
5. Specialized local/external integrations.

Every named route points to an already exposed RepoTunnel MCP tool. This list
is **guidance**, not real-time bookability, connectivity, permission, or
availability. For real access the agent must invoke the relevant status/inspect
tool. The agent should not ask the human to repeat configuration before
checking existing records, and must not bypass or weaken a permission denial.

## Safety, scope and verification

All changes are confined to the existing **RepoTunnel**
\`workspace-1a05e4cea94\` source. No changes to TeraRelay, existing Team Mode,
Mission A2A event journal, credential stores, project Git history, or installed
application sessions.

Checks:

- Production: \`cargo clippy --jobs 1 --lib --all-features --offline -- -D warnings\`.
- Test fixtures: old memory JSON compatibility, typed upsert idempotency,
  preservation of summary and next steps, input bounds, multi-word retrieval,
  retired record filtering, Resume v2 knowledge hints, existing MCP router and
  discovery route names, policy classification.
- \`git diff --check\` and targeted \`rustfmt --check\`.
- Live post-build acceptance: start a fresh authorized AI conversation for an
  approved existing project; verify that it retrieves preexisting resource
  references and test procedures before asking for login; verify a broken tool
  leads it to check relevant existing alternatives without bypassing security;
  confirm the original project state and other agents' work stay unchanged.

**Limit:** Adding knowledge retrieval and MCP discovery metadata improves
cross-chat onboarding but does not guarantee a particular model will follow
these procedures; verify using real two-chat scenarios. The large Rust test
binary may require more memory than the workspace provides, so compilation
alone is not a runtime test pass.
