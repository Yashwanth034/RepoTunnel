# Remote MCP connections

RepoTunnel keeps its raw MCP gateway local and provides several supported ways to connect a remote MCP client. The connection transport is separate from workspace authorization: changing providers must not weaken the local project boundary, OAuth checks, or tool policy.

## Local gateway

The raw MCP service stays bound to loopback. Remote providers forward authenticated traffic to that local service rather than turning the workspace gateway into a public `0.0.0.0` listener.

The local port may change for ordinary local-only operation. Providers that require a stable local origin use RepoTunnel's reserved local origin path.

## Managed public providers

### ngrok

ngrok is the simplest built-in public-provider path.

- The user supplies their own ngrok account/authtoken.
- RepoTunnel uses the ngrok Rust SDK; the ngrok CLI is not required.
- The configured public HTTPS hostname can be reused across normal restarts when the user's ngrok account/domain supports it.
- RepoTunnel stores provider configuration in application data and does not ship a developer ngrok credential.

### Cloudflare Tunnel

RepoTunnel also supports Cloudflare Tunnel.

- `cloudflared` must be available on the machine.
- The user supplies the Cloudflare tunnel configuration/token and public HTTPS hostname.
- RepoTunnel uses its stable local origin for the Cloudflare forwarder.
- RepoTunnel recreates its local worker when needed without exposing the credential to AI tools.

### Direct HTTPS

Direct HTTPS is the advanced no-relay application path.

- The user supplies a stable public HTTPS hostname/network route.
- RepoTunnel owns the local TLS reverse-proxy and ACME challenge listeners.
- The raw MCP origin remains loopback-only.
- OAuth/MCP/health routes are allowlisted; the desktop UI and arbitrary local paths are not published.
- See `docs/direct-https.md` for the verified Linux setup.

## RepoTunnel OAuth

Public MCP access uses RepoTunnel's OAuth boundary.

The implementation supports the current OAuth/DCR flow used by remote MCP clients, including PKCE validation and refresh-token-capable sessions. Authorization state is stored outside the project. Revoking remote MCP access invalidates current remote authorization without deleting approved projects or local workspace state.

## Normal startup and recovery

When auto-connect is enabled for a configured public provider, RepoTunnel can bring the local gateway and configured provider back online at startup.

Provider workers are health-checked rather than assumed healthy simply because a child process/session started. Normal network interruptions are handled by the provider/runtime recovery path where supported.

Stopping the gateway/provider tears down RepoTunnel-owned connection workers.

## Optional OpenAI Secure MCP Tunnel

RepoTunnel retains the official OpenAI `tunnel-client` integration as an optional private transport for environments that use Secure MCP Tunnel.

For that path:

- the tunnel ID is validated before launch
- the Runtime API key is passed only through the child environment
- the Runtime API key is not stored by RepoTunnel
- RepoTunnel uses the tunnel client's health mechanism to determine readiness
- stopping the local gateway also stops the managed tunnel process

## ChatGPT setup

ChatGPT's developer-mode/app UI and plan availability can change independently of RepoTunnel. Follow the current OpenAI guidance for custom MCP apps.

At a high level:

1. Enable Developer Mode if it is available for your ChatGPT plan/workspace.
2. Create a custom app from ChatGPT's Apps settings.
3. Enter the RepoTunnel MCP HTTPS endpoint and choose OAuth when using the public OAuth path.
4. Scan tools and complete the RepoTunnel authorization flow.
5. Create/enable the app, then test it in a new chat with a real RepoTunnel tool call.

If RepoTunnel's MCP schema changes, refresh/re-scan the app's actions according to the controls available in the ChatGPT workspace.

## Connection health

RepoTunnel connection diagnostics can report provider/gateway state and recent remote activity without logging MCP payload contents or credentials.

A complete health check is not only “the URL responds.” It should include a real authenticated MCP tool call against an approved workspace.
