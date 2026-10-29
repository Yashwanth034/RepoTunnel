# Completion Proof: evidence-backed completion reporting

## Product intent

Completion Proof addresses premature AI claims that a coding request is finished. It is a **separate, nonblocking evidence/reporting feature**, not Task Integrity, a permission system, an automatic reviewer, or a replacement for behavioral testing.

When a coding request is substantial, the AI records a short set of observable acceptance criteria, implements normally, and attaches successful **post-request** RepoTunnel receipts for tests or explicit browser verification. A fresh audit returns a criterion-by-criterion result and exact gaps. An AI must never report "verified complete" solely from a source edit or unrelated passing tests.

## MCP API

1. `start_completion_proof({workspace_id,request,acceptance_criteria})`: stores 1–32 distinct criteria and the initial Git HEAD. Returns a durable `proofId` with `verificationIncomplete`. No project source is changed and no Task Integrity focus is created.
2. `attach_completion_proof_evidence({workspace_id,proof_id,criterion_id,evidence_kind,source_id})`: attaches a real successful receipt **created after** proof creation. Kinds: `terminalCommand`, `managedProcess`, `browserTransaction`, `change`. Unknown, failed, pending, wrong-workspace, or old receipts are rejected. A successful change alone never counts as behavioral verification.
3. `audit_completion_proof({workspace_id,proof_id})`: independently rechecks the receipt status, criterion coverage, newest recorded project change, and Git HEAD. The result distinguishes `verificationIncomplete` from `allCriteriaEvidenceBacked`. Never caches an earlier pass as enduring proof.
4. `list_completion_proofs({workspace_id})`: discovers saved requests after a restart/reconnect; stored in the private RepoTunnel AppData `completion-proofs.json`, backed by RepoTunnel's existing atomic recovery JSON mechanism.

All tools obey approved-workspace access. No extra human approval, task scope expansion, Git stage/commit, tool installation, or full-suite testing is automatically required.

## Evidence policy

- A single broad passing test receipt cannot independently certify several acceptance criteria. To count every criterion as evidence-backed, attach a distinct targeted verification receipt to each one.
- Successful terminal/process receipts only count as behavioral verification when the executed command contains a recognized test-runner command segment. `echo "cargo test"`, `true`, ordinary commands, Clippy, and static checks **do not** satisfy the criterion on their own.
- Browser receipts require a successful complete transaction **including an explicit successful Verify step**. A click or fill alone cannot prove UI behavior.
- Actual criterion-to-test relevance **cannot be determined mechanically from a command exit code**. The AI must separately inspect affected call paths, test the precise user scenario including negative cases, and report unverified integration environments.
- Attached proof evidence is invalidated if the receipt later fails/disappears or if relevant recorded changes occur after its test; a changed Git HEAD makes the proof conservative/incomplete. Changes made directly outside RepoTunnel's recorded change history may not be detected: independently inspect the current working tree before a completion claim.
- Never interpret `allCriteriaEvidenceBacked` as semantic proof, independent code-review approval, tested deployed binaries, or a guarantee of zero bugs. The audit's `limitation` explains the boundary.

## Example

Request: "Task Integrity should only warn, not block ordinary MCP development."

Criteria should cover at minimum: ordinary in-scope edit, out-of-scope edit and warning, cross-project behavior, Git behavior, workflow without scope-approval pause, Mission and Team protections, and a live deployed MCP check. One unrelated Rust unit test does not satisfy all of them.

Start a proof, do the implementation, run specific tests, attach **appropriate** factual receipts per criterion, run the proof audit, then report which criteria remain unverified. Continue fixing or testing where possible rather than declaring completion prematurely.

## Design boundaries

This uses RepoTunnel's existing receipts, storage and MCP server; no upstream Spec Kit installation is bundled and no additional LLM or reviewer is impersonated. An independent human or second AI review may be helpful but is not silently claimed to have happened.

AI Auto / AI Review behavior, workspace approvals, sandboxing, Team ownership, Mission isolation, and Task Integrity's separate warning-only behavior are unchanged.