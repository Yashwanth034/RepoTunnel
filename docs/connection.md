# ChatGPT connection

RepoTunnel keeps the filesystem gateway bound to loopback and creates the recommended public HTTPS MCP endpoint with the embedded ngrok Rust SDK. No RepoTunnel developer account, developer token, or hard-coded public URL is shipped with the app.

## First-time setup

1. The user creates or uses their own ngrok account.
2. The user pastes their own ngrok authtoken into RepoTunnel once.
3. RepoTunnel stores that credential in the current user's app-data directory with owner-only permissions on Unix.
4. RepoTunnel starts the local MCP gateway and asks ngrok for an HTTPS endpoint.
5. RepoTunnel saves the assigned public URL for that installation and requests the same domain on later starts.
6. The user connects the resulting `https://.../mcp` URL to ChatGPT once.

The public URL belongs to the user's ngrok account. RepoTunnel never contains the developer's ngrok token or development domain.

## Normal startup

When a public connection has been configured, RepoTunnel automatically starts the loopback gateway and embedded public tunnel in the background on application launch. The local gateway may use a different loopback port between launches; that does not affect ChatGPT because RepoTunnel reconnects the saved public endpoint to the current local port automatically.

Stopping the gateway also stops the embedded public tunnel. Starting the gateway again brings the saved public tunnel back online. The embedded ngrok session automatically reconnects after ordinary network interruptions, and RepoTunnel recreates the forwarder against the same saved domain if that forwarder exits unexpectedly.

## Connection health

The Connect page reports:

- local gateway state;
- public tunnel state;
- stable public MCP URL;
- whether auto-connect is enabled;
- number of HTTPS-forwarded MCP requests observed since launch;
- timestamp of the most recent remote request.

The request counter is diagnostic only. RepoTunnel does not log MCP payloads or credentials through this feature.

## When ChatGPT needs reconnecting

Normal RepoTunnel restarts should not require a ChatGPT plugin reconnect because the saved public MCP URL remains the same. Reconnect or refresh the ChatGPT app only when the public connection is reset to a different ngrok account/domain or when RepoTunnel's MCP tool schema changes.

## Optional OpenAI Secure Tunnel

The existing OpenAI `tunnel-client` integration remains available as an advanced alternative for organizations that support Secure MCP Tunnels. It is not required for the managed public connection.
