# RepoTunnel

**Give ChatGPT access to the project you're working on—not your entire computer.**

RepoTunnel is a **local-first desktop bridge for MCP-compatible AI clients**. Connect ChatGPT or another supported client to explicitly approved workspaces so it can inspect code, edit files, run tests, use development tools, and help finish real work. RepoTunnel controls the local access, permissions, execution environment, and review flow.

[**Download v0.4.1**](https://github.com/Yashwanth034/RepoTunnel/releases/latest) · [Get started](#get-started) · [Security](docs/security.md) · [Documentation](#documentation)

## What you can do

| Area | Capabilities |
| --- | --- |
| **Projects and code** | Approve local folders or clone repositories; browse, read, search, create, patch, rename, move, and delete files within workspace permissions. |
| **Large repositories** | Indexed project inspection, paginated directory listings, cursor-based searches, and bounded file-range reads. |
| **Terminal and verification** | Run sandboxed build/test/check presets in disposable copies, or use guarded real-workspace commands and managed long-running processes. |
| **Git and GitHub** | Inspect status, diffs, history, and branches; review staging and commits; use a user-connected GitHub account without exposing its credential to AI tools. Publishing requires an explicit request. |
| **Browser automation** | Control an isolated Chromium-family browser, inspect pages and network errors, capture screenshots, manage tabs/downloads, and interact through selectors or grounded semantic references. |
| **Desktop tools** | Control permitted applications through platform adapters or use the isolated AI Workspace for supported GUI workflows. |
| **Long-running work** | Resume with live Git/process/activity facts, Project Memory, change history, checkpoints, and Continuity / Resume v2. |
| **Collaboration and media** | Coordinate two AI engineers with Team Mode; analyze media or manage durable Video Production projects. |
| **Android Phone Access** | View and control a selected authorized phone, subject to locally chosen capabilities and Android's own restrictions. |

### Browser actions with explicit uncertainty

RepoTunnel's semantic interaction layer uses **short-lived, revalidated element references**, not permanent guesses about where a button is. Browser click/type mutations record a bounded receipt. If transport is lost after a mutation may have run, RepoTunnel reports an **ambiguous** outcome instead of blindly replaying a click or form submission.

The same permission and sensitive-field boundaries apply to supported desktop semantic actions.

### Desktop control and AI Workspace

With local **Desktop** permission, RepoTunnel can operate supported application windows while blocking control of RepoTunnel itself. **AI Workspace** runs compatible GUI applications in an isolated virtual desktop so AI interactions do not take focus from your normal desktop session. Linux accessibility automation is the primary validated path; Windows and macOS semantic adapters require native validation.

### Android Phone Access

Pair an Android phone using **Wireless Debugging**, with authorized USB fallback for the same selected device. The Phone page provides persistent **scrcpy-based live viewing and human control**, plus guarded AI capabilities for apps, files, diagnostics, and accessibility-based semantic interactions through the optional bundled Phone Helper.

**Human Phone-panel control and AI/MCP control follow separate paths.** The user chooses **Off / Limited / Full**, individual Limited permissions, and **Pause AI** locally; MCP cannot raise those permissions or select a different phone. Payment-sensitive foreground apps block AI screen observation and control without requiring the Accessibility service to be disabled. Android permissions and vendor policy still apply.

[Phone Access details](docs/phone-access.md)

### Two-engineer Team Mode

Connect two AI engineers to one approved workspace. Team Mode coordinates separate task ownership, non-overlapping file claims, browser leases, cross-review, and evidence-based verification. The team can continue taking new work requests until you end it.

[Team Mode details](docs/team-mode.md)

### Video Intelligence and Production

Analyze supported public or approved local videos with transcript and visual evidence. Video Production maintains projects with scripts, storyboards, scene assets, narration, subtitles, rendering, previews, QA, and license provenance. Tutorial scenes can use HTML/CSS/GSAP; story workflows can use native motion or optional **installed** creative tools. Availability depends on local media tools and project requirements.

[Video details](docs/video.md)

## Security by design

RepoTunnel is a controlled local gateway, **not unrestricted remote desktop or filesystem access**.

- **Approved workspaces:** AI tools use workspace IDs and relative paths. Protected files, path traversal, and escapes through symlinks are blocked.
- **AI Auto or AI Review:** Apply supported actions automatically within the policy, or require local approval. Remote MCP calls cannot approve their own queued Review requests.
- **Fail-closed execution:** AI terminal commands run in a platform-specific OS sandbox. Disposable verification presets have no network access and do not write back into the original project.
- **Publishing stays intentional:** AI Auto is not standing permission to push commits, publish releases, or upload other artifacts.
- **Local control:** Pause AI, revoke remote MCP authorization, and manage Phone and Desktop permissions from RepoTunnel.
- **Protected connection:** The raw MCP gateway binds to loopback. Public connection paths use RepoTunnel's OAuth boundary; Direct HTTPS publishes only its required routes.

RepoTunnel does not replace operating-system, Android, third-party application, or external-service security policies.

[Security model](docs/security.md) · [Vulnerability reporting](SECURITY.md)

## Download

**Current public release: [v0.4.1](https://github.com/Yashwanth034/RepoTunnel/releases/tag/v0.4.1)**

| System | Download format |
| --- | --- |
| Linux x64 — Debian, Ubuntu, Linux Mint | `.deb` |
| Linux x64 — Fedora/RPM-based | `.rpm` |
| Other compatible Linux x64 distributions | `.AppImage` |
| Windows x64 | NSIS `.exe` or `.msi` |
| macOS Apple Silicon | `aarch64.dmg` |
| macOS Intel | `x64.dmg` |

Use the [official Releases page](https://github.com/Yashwanth034/RepoTunnel/releases/latest) for installers, release notes, `RepoTunnel-SHA256SUMS.txt`, and signed updater metadata. No RepoTunnel account is required.

### Platform notes

RepoTunnel packages Linux, Windows, and both macOS architectures. **Packaging is not a promise of identical native automation support.**

- **Linux** is the primary live-validated development platform. AI command isolation uses Bubblewrap; full managed-process reattachment after a complete RepoTunnel restart is currently Linux-only.
- **Windows** uses AppContainer and Job Object command isolation. Native UI Automation adapters exist, but desktop semantic behavior and app compatibility require Windows validation.
- **macOS** uses a Seatbelt `sandbox-exec` compatibility backend for AI commands. Native Accessibility adapters exist, but desktop automation is not claimed to match Linux. In-app update **installation is intentionally disabled on macOS** pending verified replacement/restore safety; install newer DMGs manually.
- **AI Workspace, browser integrations, and phone workflows** may depend on installed helpers, platform permissions, and supported application/device behavior. Android Phone Access has been exercised on a real device, not every Android vendor/version.

For the current technical boundaries, see [Product](docs/product.md), [Architecture](docs/architecture.md), and [Release / Auto Update](docs/release.md).

## Get started

1. **Install** RepoTunnel using the package for your OS and open the desktop app.
2. **Add or clone a project.** Choose the approved workspace and its read-only/read-write permissions.
3. **Choose AI Auto or AI Review.** These control how supported changes and command requests are handled.
4. **Connect an MCP client.** In RepoTunnel's **Connect** page, configure a public HTTPS provider and use the exact MCP endpoint it displays (typically `https://your-host/mcp`).
5. **Authorize the connection.** In ChatGPT's current custom-app/Developer Mode flow, add the MCP endpoint, choose OAuth, authorize RepoTunnel, and refresh its tool discovery if needed. Verify by asking the AI to list approved workspaces.

**Connection choices:** Built-in **ngrok** (ngrok account/token required; CLI not required), **Cloudflare Tunnel** (configured tunnel and `cloudflared`), **Direct HTTPS** (advanced, self-routed TLS/ACME), or the optional **OpenAI Secure MCP Tunnel** integration.

The raw workspace MCP gateway remains on `127.0.0.1`; never expose that gateway directly. ChatGPT custom-app availability and interface steps depend on your current ChatGPT plan/workspace.

[Connection guide](docs/connection.md) · [Direct HTTPS guide](docs/direct-https.md)

## How it works

```text
ChatGPT / MCP-compatible AI client
                |
        HTTPS + OAuth
                |
    RepoTunnel desktop gateway
                |
     Permissioned MCP tools
                |
       Approved workspace
       +-- Files and search
       +-- Sandboxed commands / processes
       +-- Git and GitHub
       +-- Browser / Desktop / AI Workspace
       +-- Phone / Video / Team / Continuity
```

RepoTunnel is built with **Tauri 2, Rust, React, and TypeScript**. The Rust backend enforces the local security boundary; the desktop UI provides connection, project, permission, history, and review controls.

## Documentation

| Guide | Topics |
| --- | --- |
| [Product](docs/product.md) · [Architecture](docs/architecture.md) | Scope, design, and platform limitations |
| [MCP tools](docs/mcp.md) · [Projects and search](docs/project-index.md) | Tool behavior and large-repository inspection |
| [Safe editing](docs/safe-editing.md) · [Commands](docs/commands.md) · [Git](docs/git.md) | Changes, execution, and repository operations |
| [Browser and semantic interaction](SEMANTIC_INTERACTION_LAYER.md) · [AI continuity](AI_CONTINUITY.md) | Semantic safety and recovery design/history |
| [Team Mode](docs/team-mode.md) · [Phone](docs/phone-access.md) · [Video](docs/video.md) | Advanced workflows |
| [Connections](docs/connection.md) · [Direct HTTPS](docs/direct-https.md) | Remote MCP setup |
| [Security](docs/security.md) · [Release / Auto Update](docs/release.md) | Security architecture and distribution |
| [Acceptance checklist](docs/acceptance.md) · [Third-party notices](THIRD_PARTY_NOTICES.md) | Validation and attribution |

## Contributing and support

Issues, feature requests, and pull requests: [GitHub Issues](https://github.com/Yashwanth034/RepoTunnel/issues). Please report security vulnerabilities through the guidance in [SECURITY.md](SECURITY.md), rather than including sensitive details in a public issue.

Released under the [MIT License](LICENSE). Copyright © 2026 Yashwanth.
