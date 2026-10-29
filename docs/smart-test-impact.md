# Smart Change Impact & Test Selection (Rust-first)

RepoTunnel can recommend **which checks to run next** without launching them, creating tasks, changing workspace permissions, or treating a test recommendation as proof of correctness.

## Usage

Call `recommend_affected_tests({workspace_id})` for the approved local Git workspace. The tool returns:

- `changedPaths`: bounded, exact Git working-tree paths, including staged/unstaged files, individually expanded untracked files, and both ends of renames
- `targeted`: fast commands for directly affected Rust module tests, existing browser-helper tests, and discoverable frontend/Python tests
- `finalChecks`: compilation/lint and conservative broad-regression commands for shared, security, dependency, tooling, or otherwise uncertain changes
- `unclassifiedPaths`, `highRisk`, `requiresFullRustRegression`, and explicit `limitations`

The planner is **read-only** and **does not execute commands**. A command is a recommendation, not a factual terminal receipt. Use RepoTunnel's existing `start_process` and `wait_process` to execute chosen checks and inspect the actual results.

## Selection policy

1. Inspect exact Git status using `--porcelain=v1 -z --untracked-files=all`; fail conservatively if Git cannot enumerate paths, a rename record is incomplete, paths cannot be inspected safely, or more than 256 changed paths are present.
2. Rust source modules must have a recognizable `mod` registration chain starting at the crate's `lib.rs` and an actual test marker before a direct module filter is suggested. Unregistered files, platform-gated/ambiguous modules, and files without discoverable tests fall back to broader Rust verification. Direct candidates use `cargo test --locked --manifest-path src-tauri/Cargo.toml --lib 'module::tests::' -- --test-threads=2`. **Verify that at least one real test ran**; a successful zero-test run is not evidence.
3. Rust core boundaries (including access, gateway, MCP, auth, models, storage, Git, Team, Mission, security, workflow, Task Integrity, and Completion Proof) always recommend a full Rust library regression as a final check. So do unknown Rust module paths, absent tests, and Rust dependency/build changes.
4. Less risky Rust leaf changes may be developed using their module tests plus all-target Clippy. The full Rust library regression is still advised at an integration/release checkpoint.
5. Existing frontend tests, browser tests, and AI Workspace Python tests can be recommended where the repository advertises their runners. This first iteration **does not claim transitive JavaScript/Python import analysis or precise test-file selection**.
6. Build/configuration and unrecognized paths are made visible; there is no safe “skip all checks” conclusion merely because a change is not classified.
7. For more than eight targeted commands, the planner falls back to a broader Rust gate rather than issuing an unwieldy command set.

## Non-goals and important limitations

- This is **not** a sound dependency graph. Dynamic imports, macro expansion, build scripts, native/platform gates, ignored files, external mutation, and generated or deleted modules can change effective impact.
- Git `--untracked-files=all` still respects Git ignore rules; ignored files will not appear, and the AI should investigate if ignored generated sources matter.
- Matching a Rust module name or detecting `#[cfg(test)] mod tests` is a hint only; the AI must inspect the actual run count and failures.
- The returned test commands must never be auto-run, used as authority to bypass local Review mode, or treated as evidence for Completion Proof until real successful receipts exist.
- The tool does not reduce the release gate automatically. Large changes, shared subsystems and uncertain impacts still need integration tests and native behavior checks.

## Example

For a change only in `src-tauri/src/access.rs`, the planner suggests direct access-module tests and a broader Rust regression due to the sensitive shared access layer. For a small leaf Rust module with discoverable direct tests, targeted module tests can run repeatedly during iteration, with Clippy and broader integration checks before release.

For a mixed source/configuration change, keep all indicated language checks. For a change with no discoverable tests, report the gap and create a relevant regression rather than claiming completion.

Future work could add a measured reverse-dependency graph and per-test timing cache; those are **not implemented** or implied here.
