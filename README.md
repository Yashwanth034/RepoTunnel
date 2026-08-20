# RepoTunnel

RepoTunnel turns any MCP-compatible AI chat client into one that can work directly with the local projects you approve.

Instead of copying code between AI chat, your editor, terminal, Git, and browser, RepoTunnel securely connects them together. The AI can work on your project from normal chat instructions while RepoTunnel controls what it is allowed to access and do on your computer.

![RepoTunnel home screen](RepoTunnel.png)

## What RepoTunnel can do

Through RepoTunnel, the AI can:

- Work directly with local projects you explicitly approve.
- Clone GitHub repositories and start working on them.
- Read, create, edit, rename, move, and delete project files.
- Search and understand project structure without exposing unrelated folders on your computer.
- Run builds, tests, package commands, development servers, and managed processes.
- Launch applications and project URLs.
- Open a managed browser for web-app testing.
- Navigate pages, click, type, scroll, reload, inspect pages, capture screenshots, and inspect browser errors.
- Inspect Git status, branches, diffs, and recent commits.
- Stage and commit validated changes.
- Push to Git only when you explicitly ask the AI to push.
- Keep change history and recovery points for supported modifications.
- Use **AI Auto** when you want compatible project work to proceed without repeated RepoTunnel approval prompts.
- Use **AI Review** when you want supported changes and actions to wait for local approval.
- Use **Team Mode** when you want two AI engineers working together on the same project.

### Team Mode

Team Mode connects two persistent AI engineers to the same approved project.

The two engineers split meaningful implementation work and work on non-overlapping parts in parallel. They then cross-review each other's changes, test the result, and verify that the requested work is complete.

After both engineers are connected, the Team stays attached to the project. You can continue giving new work in the same AI chats without recreating the Team each time.

<!-- Add Team Mode screenshot here -->

## Install

**Current version: v0.1.0**

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

The release also includes `RepoTunnel-SHA256SUMS.txt` for verifying downloaded files.

## Setup

### 1. Add a project

Open RepoTunnel and either:

- select an existing local project folder, or
- clone a GitHub repository directly.

Choose **AI Auto** or **AI Review** for the project.

### 2. Set up ngrok

RepoTunnel uses ngrok to create the public HTTPS MCP endpoint required for remote AI connections.

You do not need to install the ngrok CLI separately.

Create or sign in to an ngrok account:

**https://dashboard.ngrok.com/signup**

Open your authtoken page:

**https://dashboard.ngrok.com/get-started/your-authtoken**

Copy your **ngrok authtoken**.

Open the **Connect** page in RepoTunnel, paste the authtoken, and start the public connection.

Wait until the public endpoint shows **Ready**.

RepoTunnel will display an MCP URL similar to:

https://<your-ngrok-domain>/mcp

Use the exact URL shown in RepoTunnel.

RepoTunnel saves the public endpoint and normally reconnects it automatically on later launches.

<!-- Add Connect page screenshot here -->

## Connect ChatGPT

Official ChatGPT MCP setup instructions:

**https://help.openai.com/en/articles/12584461-developer-mode-and-full-mcp-connectors-in-chatgpt-beta**

To connect RepoTunnel:

1. Open ChatGPT on the web.
2. Go to **Settings → Apps → Advanced settings**.
3. Enable **Developer Mode** if required for your account or workspace.
4. Return to **Apps** and choose **Create app**.
5. Enter `RepoTunnel` as the app name.
6. Paste the MCP URL shown on RepoTunnel's **Connect** page.
7. Select OAuth authentication.
8. Choose **Scan Tools**.
9. Continue with **Sign in with RepoTunnel**.
10. RepoTunnel will show an authorization popup.
11. Review the request and choose **Allow**.
12. Finish creating the app in ChatGPT.

RepoTunnel is now connected to ChatGPT.

Normal RepoTunnel restarts should not require you to create the ChatGPT app again.

## Security

RepoTunnel is designed around explicit project access instead of unrestricted computer access.

- AI access is limited to projects you explicitly approve.
- Absolute-path access, `../` traversal, and symlink escapes outside approved projects are blocked.
- Sensitive files such as `.env`, private keys, credential files, and common secret formats are protected.
- Public MCP access is protected with RepoTunnel OAuth.
- **Revoke MCP access** immediately invalidates current remote authorization.
- Git push is allowed only when you explicitly ask the AI to push.
- **Pause AI** provides an emergency stop for RepoTunnel-managed AI activity.
- On Linux, AI terminal and process execution is isolated with Bubblewrap and is blocked if the required sandbox is unavailable.

For the complete security model, see [`docs/security.md`](docs/security.md).

## Troubleshooting

### ChatGPT says “Reconnect RepoTunnel”

Choose **Reconnect** in ChatGPT and approve the authorization request shown by RepoTunnel.

You normally do not need to recreate the ChatGPT app.

### Public connection is not Ready

Check your internet connection and ngrok authtoken, then use **Restart connection** in RepoTunnel.

### ngrok shows a warning page

If your ngrok endpoint shows a first-visit warning, open your RepoTunnel public URL in the browser and choose **Visit Site**, then retry the ChatGPT connection.

### Disconnect remote AI access

Use **Revoke MCP access** in RepoTunnel.

Your approved projects and public endpoint configuration remain unchanged.

### Linux AI terminal commands are unavailable

Install Bubblewrap using your Linux distribution's package manager and restart RepoTunnel.

RepoTunnel intentionally blocks AI terminal execution when the required sandbox is unavailable.

### Windows or macOS shows a security warning

The current v0.1.0 Windows and macOS packages are not commercially code-signed or Apple-notarized.

Download RepoTunnel only from the official release page and verify the package using `RepoTunnel-SHA256SUMS.txt`.

## License

This project is licensed under the MIT License — see [LICENSE](https://github.com/Yashwanth034/RepoTunnel/blob/main/LICENSE) for details.

## Contributing

Bug reports, feature requests, improvements, and pull requests are welcome.

**https://github.com/Yashwanth034/RepoTunnel/issues**

## Copyright

Copyright © 2026 Yashwanth.
