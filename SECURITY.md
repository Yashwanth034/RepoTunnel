# Security Policy

RepoTunnel is intentionally designed around least-privilege access to local source code.

## Reporting a vulnerability

Do not include API keys, private repository contents, personal filesystem paths, or other secrets in a public issue. Use the repository owner's private security-reporting channel when one is configured. Until a private reporting channel is published, provide only a minimal non-sensitive reproduction description publicly and wait for a private contact path before sharing exploit details.

## Security boundaries

RepoTunnel does not claim to sandbox the desktop application itself or software the user launches manually. Its security guarantees are scoped to operations performed through RepoTunnel's approved workspace, MCP, command, and Git interfaces. AI-triggered terminal/process commands and disposable verification commands are isolated with Bubblewrap and are blocked rather than silently falling back to unrestricted host access when the required sandbox is unavailable.

A supported deployment should keep RepoTunnel and its dependencies updated and should not modify the application to expose the loopback MCP server publicly.

See `docs/security.md` for the full technical model and `docs/acceptance.md` for the release security test.
