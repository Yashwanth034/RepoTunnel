# Changelog

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
