# Changelog

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
