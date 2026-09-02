# Security Policy

RepoTunnel is designed around least-privilege access to explicitly approved local projects.

## Reporting a vulnerability

Do not post secrets, private repository content, or personal filesystem paths in a public issue. Use the repository owner's private security-reporting channel when available; otherwise publish only a minimal non-sensitive reproduction and wait for a private contact path before sharing exploit details.

## Security boundaries

RepoTunnel's guarantees apply to operations performed through its approved workspace, MCP, command, Git, and AI Workspace interfaces. They do not sandbox software that the user launches manually.

AI-triggered command execution is fail-closed: Bubblewrap on Linux, AppContainer + Job Object isolation on Windows, and a Seatbelt `sandbox-exec` compatibility backend on macOS. If the required sandbox is unavailable, RepoTunnel blocks the operation instead of falling back to unrestricted host execution.

Keep RepoTunnel and its dependencies updated, and do not expose the loopback MCP server directly to untrusted networks.

## Dependency audits

Releases run npm and RustSec dependency audits. Security vulnerabilities block release. Informational, unmaintained, unsound, or yanked transitive dependency notices are reviewed separately and are not hidden to produce a clean-looking report. Some Linux notices currently originate in Tauri's GTK3/WebKit dependency stack and remain tracked until upstream replacements are available.

See `docs/security.md` for the technical model and `docs/acceptance.md` for release-security verification.
