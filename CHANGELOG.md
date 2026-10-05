# Changelog

## 0.4.1 - 2026-10-05

RepoTunnel v0.4.1 is the first public 0.4.x release. It combines the capability work completed during the unpublished v0.4.0 validation build with the final Phone reliability fixes verified before release.

### Highlights

- Added full Phone Access workflows with persistent scrcpy 4.1 live video, human control, semantic accessibility actions, bounded transactions, app/files/settings/network/shell tooling, and payment-sensitive privacy blocking for AI/MCP access.
- Fixed intermittent Phone Helper `Resource temporarily unavailable (os error 11)` failures by treating timed `WouldBlock` reads as transient on the same request/socket, with a bounded response deadline and no semantic-action replay.
- Fixed local Phone-panel lag by separating human tap/swipe/key/text control from the AI/MCP payment-policy helper path while keeping AI/MCP control on the strict payment-sensitive guard.
- Fixed stale/lagging Phone-panel video by giving the local human view a dedicated cached scrcpy frame path instead of running semantic-helper policy checks on every refresh; healthy rendering now waits briefly for the next frame instead of busy-polling duplicates.
- Treats empty semantic-helper socket closures as transient helper unavailability rather than protocol corruption, allowing safe read-only policy checks to recover cleanly.
- Added bounded large-project inspection/search/range-read tools with resumable cursors and concurrency protection for very large repositories.
- Strengthened long-running work with durable managed-process supervision and Linux restart reattachment backed by persisted supervisor state and process-identity checks.
- Added browser mutation receipts for selector/semantic click and type operations so post-dispatch transport loss is surfaced as `ambiguous` and never blindly replayed.
- Expanded Continuity/Resume, ChatGPT continuation recovery, Team Mode coordination, AI Workspace recovery, and current project documentation for long multi-session AI work.
- Expanded Video Production with persistent Video Projects, narration/subtitles, generated scenes, rendering/preview/QA, story adapters, and project-owned asset/license tracking.
- Final v0.4.1 release validation passed 43/43 frontend tests and 448 Rust tests with 0 failures and 8 intentional ignores.

## 0.4.0 - 2026-10-05

Unpublished internal validation build. Its capability work was superseded by v0.4.1 before any public GitHub release; the v0.4.1 notes above are the canonical public 0.4.x release notes.

## 0.3.1 - 2026-09-06

RepoTunnel v0.3.1 is a focused reliability and editor-quality update for v0.3.0.

### Highlights

- Fixed Projects / Continuity layouts so long live context cannot stretch the application horizontally or distort the page.
- Restored normal editor long-line behavior with visual line wrapping, while preserving the file contents unless the user explicitly inserts a newline.
- Added editor regression coverage for Backspace, undo/redo, caret stability, and long-line wrapping.
- Reduced UI stalls by moving heavier status/history/workspace operations off the frontend command path and coalescing repeated activity refreshes.
- Reduced AI Workspace preview polling pressure and cleaned up stale isolated-session processes more reliably.
- Cached external connection-tool probes so routine connection-status refreshes do not repeatedly spawn expensive checks.

## 0.3.0 - 2026-09-05

RepoTunnel v0.3.0 focuses on reliable continuation, safe updates, and a hardened Direct HTTPS connection.

### Highlights

- Added Continuity / Resume v2, which resumes from live Git, activity, process, and bounded project context instead of trusting stale saved next steps.
- Added signed in-app Auto Update infrastructure and release artifacts, with install-safety checks and persisted-state health verification.
- Fixed Direct HTTPS startup under Rustls 0.23 by selecting a deterministic crypto provider before TLS initialization.
- Made Direct HTTPS listener status truthful: `:43183` is reported online only after TLS and ACME listeners initialize successfully.
- Kept the MCP/Direct HTTPS recovery channel independent from optional product features so normal connection startup cannot be stranded by unrelated UI features.
- Prevented Ollama Model Hub recovery from triggering privileged system-service password prompts; only the per-user service is attempted.
- Strengthened Project Memory/continuity persistence and connector safety metadata.
- Removed the experimental Google account/sync work before release; RepoTunnel remains fully accountless.

## 0.2.0 - 2026-09-02

RepoTunnel v0.2.0 focuses on reliability, AI Workspace, editor quality, safety, and release hardening.

### Highlights

- Added isolated AI Workspace automation with bounded multi-action sequences while preserving workspace, credential, self-control, browser, Docker, and teardown protections.
- Added platform-aware productivity integrations and verified VS Code with its integrated terminal as the practical Linux terminal workflow.
- Replaced the custom editor input layer with CodeMirror 6, including native undo/redo, selection, Backspace behavior, search, indentation, line operations, diagnostics, and lazy language support.
- Reduced background monitoring cost with metadata-only scans and moved heavy inspection/check work away from the desktop UI path.
- Added persistent Pause/Resume AI access, expanded Safety Scan coverage, request-grouped reversible history, checkpoints, and restore protections.
- Embedded the managed ngrok connection path with stable endpoint reuse, health controls, reconnect handling, and local credential protection.
- Added an advanced Direct HTTPS path with OAuth/MCP route allowlisting, trusted TLS, a loopback-only raw MCP gateway, and a documented Route64/WireGuard setup for CGNAT environments.
- Preserved fail-closed command isolation: Bubblewrap on Linux, AppContainer + Job Object on Windows, and Seatbelt compatibility sandboxing on macOS.
- Cleaned stale development artifacts and strengthened release checks for formatting, Clippy warnings, tests, dependency audits, version consistency, and package hygiene.
- Added release packaging for Linux DEB/RPM/AppImage, Windows NSIS/MSI, and macOS Apple Silicon/Intel DMG builds with SHA-256 checksums.

## 0.1.0 - 2026-08

Initial RepoTunnel release with approved workspace access, MCP connectivity, protected-path enforcement, safe file operations, command sandboxing, Git controls, history/undo, project checks, diagnostics, and desktop installers.
