# RepoTunnel

RepoTunnel is a local-first bridge that lets MCP-compatible AI clients work directly with the projects you explicitly approve on your computer.

Instead of copying code between chat, your editor, terminal, Git, and browser, RepoTunnel connects those tools through one controlled workspace. The AI can inspect, edit, test, debug, and help complete real project work while RepoTunnel keeps access limited to the project and permissions you choose.

![RepoTunnel home screen](RepoTunnel.png)

## Why RepoTunnel

AI coding is most useful when it can work with the actual project instead of isolated snippets. RepoTunnel gives the AI that access without giving it unrestricted access to your computer.

With RepoTunnel, an AI can:

- Work only inside projects you explicitly approve.
- Read, create, edit, rename, move, and delete project files.
- Search and understand project structure.
- Run builds, tests, package commands, development servers, and managed processes.
- Inspect Git status, branches, diffs, and recent commits.
- Stage and commit validated changes.
- Push to Git only when you explicitly ask for a push.
- Launch supported applications and project URLs.
- Use a managed browser to test web applications.
- Navigate pages, click, type, scroll, reload, capture screenshots, and inspect browser errors.
- Use an isolated AI Workspace for supported desktop applications without taking over your normal desktop session.
- Keep project monitoring, change history, and recovery information available during longer AI work.
- Use Team Mode when two AI engineers should collaborate on the same project.

## Work modes

### AI Auto

AI Auto allows compatible project work to continue without repeated RepoTunnel approval prompts.

Security boundaries still remain active. AI Auto does not grant unrestricted filesystem access, disable sandboxing, or give standing permission to push Git changes.

### AI Review

AI Review keeps supported changes and actions waiting for local approval before they are applied.

Use it when you want to inspect changes more closely while still allowing the AI to work directly with the project.

## AI Workspace

AI Workspace provides an isolated virtual desktop for supported applications.

It allows the AI to work inside an approved desktop application without stealing focus from your normal desktop. This is useful for editors, development tools, productivity applications, and other supported GUI workflows.

The normal RepoTunnel file, terminal, process, browser, and project methods remain available as fallback paths where appropriate.

## Team Mode

Team Mode connects two persistent AI engineers to the same approved project.

The engineers divide meaningful implementation work into non-overlapping tasks, work in parallel, cross-review each other's changes, test the result, and verify the requested work before completing the current task.

The Team stays attached to the project so later requests can continue without recreating the collaboration session each time.

## Install

Download RepoTunnel from the official GitHub Releases page:

**https://github.com/Yashwanth034/RepoTunnel/releases/latest**

| Platform | Package |
| --- | --- |
| Windows x64 | `.exe` or `.msi` |
| macOS Apple Silicon | `aarch64.dmg` |
| macOS Intel | `x64.dmg` |
| Debian / Ubuntu / Linux Mint | `.deb` |
| Fedora / RHEL compatible | `.rpm` |
| Other supported x86_64 Linux systems | AppImage |

Each release includes `RepoTunnel-SHA256SUMS.txt` so downloaded installers can be verified before use.

Version history and release notes belong on the GitHub Releases page rather than in this README.

## Quick start

### 1. Add a project

Open RepoTunnel and either:

- select an existing local project folder, or
- clone a GitHub repository directly.

Choose **AI Auto** or **AI Review** for the project.

### 2. Start a public MCP connection

RepoTunnel supports a simple ngrok-based connection and an advanced Direct HTTPS setup.

For the normal setup, open the **Connect** page, configure ngrok, and wait until the public endpoint shows **Ready**.

RepoTunnel will show an MCP URL similar to:

```text
https://your-public-host/mcp
```

Use the exact MCP URL shown by RepoTunnel.

RepoTunnel can save the public endpoint configuration and reconnect it on later launches.

For an advanced self-managed HTTPS path, see [Direct HTTPS](docs/direct-https.md).

### 3. Connect your AI client

Add the MCP URL shown by RepoTunnel to your MCP-compatible AI client and complete RepoTunnel authorization.

After connecting, verify the setup with a real RepoTunnel tool call such as listing approved workspaces. A real tool call confirms that the complete MCP path is working.

## Connect ChatGPT

To connect RepoTunnel with ChatGPT:

1. Open ChatGPT on the web.
2. Go to **Settings → Apps → Advanced settings**.
3. Enable **Developer Mode** if required for your account or workspace.
4. Return to **Apps** and choose **Create app**.
5. Enter `RepoTunnel` as the app name.
6. Paste the MCP URL shown on RepoTunnel's **Connect** page.
7. Select OAuth authentication.
8. Choose **Scan Tools**.
9. Continue with **Sign in with RepoTunnel**.
10. Review the authorization request shown by RepoTunnel and choose **Allow**.
11. Finish creating the app.

Official ChatGPT app/connector setup page:

**https://chatgpt.com/plugins#settings/Connectors?create-connector=true&redirectAfter=%2Fplugins**

Normal RepoTunnel restarts should not require recreating the ChatGPT app when the public MCP URL remains the same.

## ngrok setup

RepoTunnel can use ngrok to provide the public HTTPS endpoint required by a remote MCP client.

You do not need to install the ngrok CLI separately.

Create or sign in to an ngrok account:

**https://dashboard.ngrok.com/signup**

Open the authtoken page:

**https://dashboard.ngrok.com/get-started/your-authtoken**

Copy your ngrok authtoken, open RepoTunnel's **Connect** page, enter it there, and start the public connection.

## Direct HTTPS

Direct HTTPS is an advanced option for users who want to expose RepoTunnel through their own trusted HTTPS endpoint while keeping the raw MCP gateway private.

The raw gateway must remain loopback-only. Direct HTTPS exposes only the routes needed for MCP, OAuth, authorization, health checks, and certificate handling.

See the complete setup and security requirements in [docs/direct-https.md](docs/direct-https.md).

## Security

RepoTunnel is designed around explicit project access instead of unrestricted computer access.

- AI access is limited to projects you explicitly approve.
- Absolute-path access, `../` traversal, and symlink escapes outside approved projects are blocked.
- Sensitive files such as `.env`, private keys, credential files, and common secret formats are protected.
- Public MCP access is protected with RepoTunnel OAuth.
- **Revoke MCP access** invalidates current remote authorization.
- Git push is allowed only when you explicitly ask the AI to push.
- **Pause AI** provides an emergency stop for RepoTunnel-managed AI activity.
- AI command execution uses the platform-specific isolation available to RepoTunnel.
- On Linux, AI terminal and process execution uses Bubblewrap and is blocked if the required sandbox is unavailable.
- Direct HTTPS keeps the raw MCP gateway on loopback and preserves Host validation rather than weakening it.

For the complete security model, see [docs/security.md](docs/security.md).

## Troubleshooting

### ChatGPT says “Reconnect RepoTunnel”

Choose **Reconnect** in ChatGPT and approve the authorization request shown by RepoTunnel.

You normally do not need to recreate the ChatGPT app.

### Public connection is not Ready

Check your internet connection and connection-provider configuration, then use **Restart connection** in RepoTunnel.

### ngrok shows a warning page

Open the RepoTunnel public URL in your browser, complete the ngrok first-visit step if shown, and then retry the AI-client connection.

### Disconnect remote AI access

Use **Revoke MCP access** in RepoTunnel.

Your approved projects and local project configuration remain unchanged.

### Linux AI terminal commands are unavailable

Install Bubblewrap using your Linux distribution's package manager and restart RepoTunnel.

RepoTunnel intentionally blocks AI terminal execution when the required Linux sandbox is unavailable.

### Windows or macOS shows a security warning

Download RepoTunnel only from the official GitHub Releases page and verify the package using `RepoTunnel-SHA256SUMS.txt`.

Platform signing and trust behavior may vary by release and operating-system policy.

## Documentation

- [Security model](docs/security.md)
- [Direct HTTPS](docs/direct-https.md)
- [GitHub Releases](https://github.com/Yashwanth034/RepoTunnel/releases)

## Contributing

Bug reports, feature requests, improvements, and pull requests are welcome.

**https://github.com/Yashwanth034/RepoTunnel/issues**

## License

RepoTunnel is licensed under the MIT License. See [LICENSE](LICENSE).

## Copyright

Copyright © 2026 Yashwanth.
