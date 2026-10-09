# RepoTunnel Website Content Sources

The website copy was written from current RepoTunnel project information inspected read-only. Important source files included:

- `README.md` — product summary, capabilities, setup, release/download links
- `CHANGELOG.md` — public release history and v0.4.1 validation summary
- `SECURITY.md` — public security policy
- `docs/architecture.md` — local MCP gateway, workspace guard, execution, browser, AI Workspace, Phone, Video, Team Mode, Git and connection architecture
- `docs/product.md` — product purpose and trust models
- `docs/security.md` — detailed workspace, secret, command, Git, browser, desktop, Phone, OAuth and runtime security behavior
- `docs/mcp.md` — capability-oriented MCP contract and compatibility rules
- `docs/connection.md` — ngrok, Cloudflare, Direct HTTPS, OAuth and ChatGPT connection model
- `docs/direct-https.md` — verified Linux Direct HTTPS topology and constraints
- `docs/project-index.md` — paged large-project inspection/search behavior
- `docs/safe-editing.md` — AI Review/Auto, stale-change protection and undo behavior
- `docs/acceptance.md` — platform/runtime acceptance boundaries
- `docs/release.md` — v0.4.1 release packaging and updater behavior
- `docs/phone-access.md` — Android Phone Access model
- `docs/team-mode.md` — two-engineer coordination model
- `docs/video.md` and video production documentation — Video Intelligence/Production behavior

The website deliberately distinguishes validated current behavior from platform-specific capabilities that still require native validation. It does not claim a fixed MCP tool count.

## App-screen and companion coverage

The 2026-10-09 expansion also uses actual public UI source to explain workflows that were not sufficiently described by the high-level product docs:

- `src/components/AppSidebar.tsx` and `src/App.tsx` — exact screen names, routes and project/global review scope
- `HomeWorkspace.tsx`, `HomeChat.tsx` and `ModelHub.tsx` — Home controls, local-runtime model discovery and per-message edit opt-in; no inaccessible Model Hub navigation is assumed
- `WorkflowPanel.tsx` and `docs/workflow.md` — read-only Ready/Limited/Blocked preflight
- `PendingChangeReview.tsx`, `ChangeHistoryPanel.tsx` and `CheckpointManager.tsx` — review, timelines, restore and clear scope
- `ProjectMemoryPanel.tsx` and `ProjectSetupPanel.tsx` — human project context, factual continuity and preparation status
- `ProductionPanel.tsx` and `HelpPanel.tsx` — runtime settings, updates, retention and shortcuts
- `HttpsSetupGuide.tsx` and `VideoPanel.tsx` — visible setup stages and media-workflow views
- `extensions/chatgpt-continuation/README.md` and `PRIVACY.md` — installation, exact-chat binding, status, pairing, delivery and companion data handling

Component names above are under `src/components/` in the RepoTunnel app project. Guides follow current project source; current main-branch code is not proof of behavior shipped in an earlier desktop release. The extension's source is available on main, while no separate extension asset is claimed for desktop v0.4.1.

See `docs/content-coverage.md` for the complete screen-to-guide map and reviewed boundaries.

## External setup references and screenshots

The platform-help expansion checks current provider instructions against RepoTunnel's `docs/connection.md`, `docs/direct-https.md`, `PublicTunnelPanel.tsx` and `HttpsSetupGuide.tsx`. External links appear beside the relevant steps rather than only in a reference list.

- ngrok: signup/dashboard/authtoken controls, [quickstart](https://ngrok.com/docs/start), pricing and service status
- Cloudflare: [named tunnel setup](https://developers.cloudflare.com/tunnel/get-started/), connector downloads, Quick Tunnel limits and troubleshooting
- Direct HTTPS: [Route64 manager](https://manager.route64.org/), [DuckDNS](https://www.duckdns.org/), [Netiter IPv4 frontend](https://v4-frontend.netiter.com/), WireGuard, Certbot and Let's Encrypt HTTP-01 documentation
- AI clients: OpenAI's current developer-mode/MCP apps guide and optional Secure MCP Tunnel guide
- Local models: official Ollama, LM Studio and llama.cpp setup/download documentation
- Companion installation: Chrome and Edge's official unpacked/sideloading guides
- Phone: Android developer options and Wi-Fi debugging/pairing documentation
- Git and media: GitHub authentication, Git installation, yt-dlp, FFmpeg and ffprobe documentation

`docs/platform-screenshots.json` records sources, dimensions and SHA-256 hashes for every published guide screenshot. ngrok, Route64 and DuckDNS screenshots are public signed-out pages. The Cloudflare image is an unmodified frame from its official setup tutorial; the caption distinguishes the tutorial's example configuration from RepoTunnel's required origin. Chrome and Android images are unmodified official Google documentation examples, credited in the captions under CC BY 4.0.

No private dashboard, actual authtoken, WireGuard key or authenticated user data is published. The capture helper uses a disposable signed-out browser context and drops transient login URL query parameters from provenance.
