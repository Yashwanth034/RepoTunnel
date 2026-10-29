# RepoTunnel Team Mode — Optional Direct Agent Messages

## Scope

This adds a **new, optional directed mailbox** for an EXISTING Team Mode A/B
session. It does **not** change any existing Team APIs, planning or review
criteria, task ownership, messages board, file and browser locks, session
revision, or Team UI. \`team_action\` and \`team_status\` retain the shared Team state and action contracts. Agent-bound MCP responses can now additionally expose an optional \`directInbox\` field when unacknowledged recipient-only messages exist. Existing saved \`team-sessions.json\` data remains untouched.

RepoTunnel **already has a real A2A event journal** under
\`src-tauri/src/mission/events.rs\`: Mission Control uses typed, append-only,
cursor-addressable events, segmented persistence, and per-Mission worker
authority. **Those events belong to an Agent Mission and Mission agent
identity, not to a persistent A/B Team session.** Reusing Mission's live
journal for an unrelated Team without an explicit Mission and worker identity
would bypass/complicate Mission isolation and risk breaking current Teams.
This optional Team mailbox therefore mirrors the useful A2A event patterns
(durable typed envelopes, scoped recipient, cursor, retry idempotency,
acknowledgements) **without modifying Mission's journal or Team's existing
coordination engine**. It is a Team-local A2A-style compatibility channel, not
an A2A protocol wire adapter or a claim that Mission workers have joined
the Team. A future common journal extraction or protocol bridge requires
explicit separate security and migration testing.

## MCP tools

1. \`team_send_direct_message\`: the caller supplies existing
   \`session_id\`, its bound and joined \`agent_id\`, the other joined
   \`target_agent_id\`, \`kind=request|response|notice|blocker\`, and
   \`message\`. Optional \`request_key\` provides retry idempotency (exact
   same sender/recipient/type/content/reply); optional \`reply_to_id\` links a
   response to an actual message from that teammate in the current work cycle.
2. \`team_direct_inbox\`: recipient-only view, with optional
   \`after_sequence\` cursor, acknowledged filter and bounded result limit.
   It never updates Team shared state. During normal active coordination, incoming unacknowledged messages are also surfaced in authorized \`team_status\` and \`team_action\` responses, so an additional explicit inbox poll is not always needed. This is not push transport: a direct message cannot wake an idle ChatGPT tab or inject a new user turn.
3. \`team_ack_direct_message\`: recipient-only idempotent acknowledgement by
   exact message ID. Default inbox omits acknowledged messages; callers can
   explicitly include them for history.

Important: An agent must **already be joined/bound to its role** using the
standard Team tools on its existing MCP connection. The directed endpoint
requires that the current client binding match both its exact **Team session**
and its **agent ID**; it pins the first direct-mailbox identity to that MCP
connection so later Team status calls cannot silently switch its recipient
mid-session. This is Team's existing client binding, not a new cryptographic
proof of agent identity. Merely sending another agent's ID on an unbound or
differently bound connection cannot access that agent's inbox. Direct messages require two joined teammates and an active
Team session; reading one's own existing inbox remains possible while the
Team is paused.

## Data and safety

The mailbox is persisted atomically in the separate application-data file
\`team-direct-messages.json\`. It is **not** persisted inside Team snapshots, existing
shared messages, task tables or project files. An authorized MCP Team response may include a transient, recipient-only \`directInbox\` projection; other clients or agent identities cannot read it. Message text is untrusted
peer context, not an approval for modifying files, executing commands,
releasing claims, bypassing the planning barrier or impersonating a user.
The \`USER REQUEST:\` control prefix is rejected in directed agent messages.

Messages are limited to 4,000 characters, 200 per Team and 3,000 globally,
and belong to one Team work-cycle number. Per-Team ordering is defined by
durable monotonic sequence values; retries with the same request key return
the original envelope without duplicate sends **while the original message
is still retained**. The bounded retention policy may evict older messages. Acknowledgements
require the recipient's role in the **same active work cycle** and cannot
be performed by the sender, a different Team or on an older Team cycle.
The mailbox rejects symlink-backed files rather than following untrusted
storage paths.

No external network service, payment, credentials, background worker or
model-to-model automatic conversation is started by this feature.

## Validation and usage

To test, use a synthetic two-agent Team with both existing roles joined,
send a message from A to B, poll B's inbox, optionally send a response
referencing the received ID, acknowledge on B, and confirm A cannot read
B's inbox. Repeat after an application restart to verify persistence and
ensure the original Team shared messages, task states and locks are identical.
Try one unjoined sender, wrong-recipient acknowledgement, duplicate request
keys, paused Team send and a new work cycle.

Focused Rust regression suite:

    cargo test --lib --offline team_comms::tests

MCP exposure and policy:

    cargo test --lib --offline additive_team_direct_messaging_preserves_existing_tool_policies
    cargo test --lib --offline every_exposed_mcp_tool_has_task_integrity_policy

Production compilation:

    cargo clippy --jobs 1 --lib --all-features --offline -- -D warnings

A local build/restart and a real two-client end-to-end test remain necessary
before calling transport-level integration complete.


Recipient isolation relies on RepoTunnel's existing Team role/client binding,
not independent cryptographic A2A agent authentication. A client that was not
already bound to a Team role must join/refresh via the existing tools. Mission Control's separate A2A event journal is present in this checkout
but deliberately not wired to Team persistence or Mission worker credentials.

## Current verification constraints

Production Rust builds passed strict `cargo clippy --lib --all-features --offline -- -D warnings` after the core integration. The full Rust test executable is expensive on this machine and prior `cargo test --lib` runs timed out during compilation/linking; a strict `cargo clippy --lib --tests` run exposed three unrelated pre-existing `cloned_ref_to_slice_refs` warnings in `src-tauri/src/remote_source_receiver.rs`. A subsequent test-target run with those unrelated warnings suppressed timed out compiling the large test target. Neither result is evidence of a passing Team direct-message integration test; the new regression fixtures are written but full end-to-end live A/B testing remains pending.
