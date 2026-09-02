# Direct HTTPS

RepoTunnel Direct HTTPS is an advanced public-connection option for users who want to expose MCP through their own trusted HTTPS endpoint while keeping the raw MCP gateway private.

The verified Linux path is:

```text
Remote MCP client / ChatGPT
        |
        | HTTPS + OAuth
        v
Stable public hostname
        |
        | IPv6 directly, or IPv4-to-IPv6 frontend when needed
        v
Route64 public IPv6
        |
        | WireGuard
        v
Linux computer
        |
        v
RepoTunnel Direct HTTPS
        |
        | approved reverse-proxy routes only
        v
127.0.0.1:43555 MCP gateway
```

This setup can use Route64, DuckDNS, Let's Encrypt, and an IPv4-to-IPv6 frontend without a mandatory monthly infrastructure fee. These are third-party free/best-effort services, not uptime or bandwidth guarantees.

## Ports and boundaries

| Port | Purpose |
| --- | --- |
| `43555` | Raw RepoTunnel MCP gateway. Loopback only; never expose publicly. |
| `43444` | RepoTunnel Direct HTTPS listener. |
| `44666` | ACME HTTP challenge listener used for certificate issuance. |

On the verified Route64 path, public TCP `443` is redirected to local `43444`, and public TCP `80` is redirected to local `44666` for ACME validation. Do not expose `43555`, add a router DMZ, or expose router administration.

## 1. Create the public IPv6 path

This route is useful when a normal home IPv4 connection is behind CGNAT and router port forwarding cannot make the computer publicly reachable.

1. Create a Route64 WireGuard tunnel and choose a nearby Route64 server.
2. Keep the Route64-generated prefix, routes, and keepalive settings unchanged unless Route64 instructs otherwise.
3. Use a short Linux interface name such as `rt-direct`; Linux interface names are limited to 15 characters.
4. Store the WireGuard configuration with restrictive permissions.
5. Enable the interface at boot:

```bash
sudo systemctl enable --now wg-quick@rt-direct
```

Verify:

```bash
systemctl is-active wg-quick@rt-direct
sudo wg show rt-direct
ip -6 addr show dev rt-direct
```

The interface should show the Route64-assigned global IPv6 address. Never publish or commit WireGuard private material.

If your ISP already provides a stable globally routable IPv6 address, the Route64 tunnel can be skipped; the remaining Direct HTTPS design still applies.

## 2. Redirect public HTTPS and ACME traffic

On Linux, use narrowly scoped nftables rules for the Route64 public IPv6 path:

```text
Public TCP 443 -> RepoTunnel 43444
Public TCP 80  -> RepoTunnel 44666
```

RepoTunnel's Linux helper can automate the WireGuard/service/nftables setup. Use only your own Route64-generated network values.

Verify the table after configuration:

```bash
sudo nft list table inet repotunnel_direct
```

## 3. Configure a stable hostname

Create a DuckDNS hostname and point its IPv6/AAAA value to the Route64-assigned public IPv6 address.

Some remote infrastructure may still connect over IPv4. If an IPv4 compatibility frontend is required, configure DuckDNS using the current address published by the frontend service rather than copying an old hard-coded IP. The verified setup used `v4-frontend.netiter.com`.

The resulting path is conceptually:

```text
IPv4 client -> IPv4-to-IPv6 frontend -> IPv6 origin -> Route64 -> RepoTunnel
IPv6 client -------------------------> IPv6 origin -> Route64 -> RepoTunnel
```

TLS terminates on RepoTunnel, not on the compatibility frontend.

## 4. Enable Direct HTTPS in RepoTunnel

Choose **Direct HTTPS** as the public provider and set the public URL to your HTTPS hostname, for example:

```text
https://your-hostname.example
```

The MCP URL becomes:

```text
https://your-hostname.example/mcp
```

Recommended configuration:

- Provider: **Direct HTTPS**
- Public URL: your stable HTTPS hostname
- Auto-start: enabled when desired

The real MCP gateway must remain on `127.0.0.1:43555`. Direct HTTPS listens separately and proxies only approved public routes.

## 5. Keep the public route allowlist small

The Direct HTTPS frontend should expose only the routes required for MCP, OAuth, health checks, and authorization:

```text
/mcp
/.well-known/oauth-protected-resource
/.well-known/oauth-protected-resource/mcp
/.well-known/oauth-authorization-server
/register
/authorize
/token
/health
```

Everything else should remain unavailable. Do not expose RepoTunnel's desktop UI, settings, project browser, history, checkpoints, arbitrary local paths, or other application surfaces.

## 6. Issue a trusted TLS certificate

After the hostname resolves correctly and public TCP `80` reaches RepoTunnel's ACME listener, use RepoTunnel's trusted-certificate action for the hostname.

Let's Encrypt HTTP-01 follows this path:

```text
hostname:80
  -> public IPv6 path
  -> nftables redirect
  -> RepoTunnel:44666
  -> ACME challenge
```

After issuance, RepoTunnel should report a trusted certificate and an online HTTPS listener. The certificate private key must remain in RepoTunnel's private application-data area and must never be committed.

Verify public HTTPS before configuring a remote MCP client:

```bash
curl -4 -i https://your-hostname.example/health
curl -6 -i https://your-hostname.example/health
```

A successful health response verifies DNS resolution, network reachability, TLS trust, the Direct HTTPS listener, and RepoTunnel's public health route.

## 7. OAuth and MCP requirements

RepoTunnel uses OAuth with PKCE for the remote MCP connection. Verify that the public hostname is consistently used by:

- protected-resource metadata
- authorization-server metadata
- authorization endpoint
- token endpoint
- registration endpoint
- MCP resource URL

Useful checks:

```bash
curl -4 -sS https://your-hostname.example/.well-known/oauth-protected-resource/mcp
curl -4 -sS https://your-hostname.example/.well-known/oauth-authorization-server
```

PKCE `S256`, authorization-code flow, and refresh-token flow should be advertised where supported by the configured server.

### Critical reverse-proxy rule

Never forward the public `Host` header unchanged to the loopback MCP gateway. The loopback gateway deliberately validates its Host header to resist DNS-rebinding-style access.

The Direct HTTPS proxy should:

- let the upstream HTTP client generate the loopback Host for `127.0.0.1:43555`;
- preserve the original public hostname as `X-Forwarded-Host`;
- set `X-Forwarded-Proto: https`;
- preserve the gateway's Host validation rather than weakening it.

A `403 Forbidden: Host header is not allowed` response usually means the public Host was forwarded incorrectly to the loopback gateway.

## 8. Connect a remote MCP client

Use the public MCP URL:

```text
https://your-hostname.example/mcp
```

For ChatGPT, create the MCP connection/app, choose OAuth, complete RepoTunnel authorization, and allow the client to discover/scan tools.

Do not stop at a UI status such as **Connected**. Verify an actual RepoTunnel tool call from a fresh conversation, for example by listing the approved workspaces. A real tool call confirms the complete path:

```text
ChatGPT -> HTTPS -> OAuth -> MCP -> RepoTunnel -> approved local workspace
```

## 9. Restart and network changes

Keep the WireGuard interface enabled at boot. The public MCP hostname is tied to the public hostname and routed IPv6 path, not to the local Wi-Fi address, so changing Wi-Fi or using a mobile hotspot normally does not require recreating the MCP connection as long as the new network permits the outbound WireGuard tunnel.

After a reboot or network change, verify:

```bash
systemctl is-active wg-quick@rt-direct
sudo wg show rt-direct
curl -4 https://your-hostname.example/health
```

Then verify one real MCP tool call.

## Security checklist

Before considering Direct HTTPS ready:

- raw MCP gateway `127.0.0.1:43555` remains loopback-only;
- only the Direct HTTPS frontend is public;
- OAuth remains enabled;
- the public route allowlist stays minimal;
- TLS is trusted and matches the public hostname;
- private keys, service tokens, OAuth tokens, passwords, and router credentials are absent from source control and documentation;
- public `Host` is not forwarded as the loopback gateway Host;
- a real authenticated MCP tool call succeeds.

## Troubleshooting

**TLS EOF / connection failure** — confirm RepoTunnel shows the local gateway online, a trusted TLS certificate, and the Direct HTTPS listener online.

**403 Host header error** — strip the public Host from the upstream request, preserve it as `X-Forwarded-Host`, and keep the loopback gateway's Host validation enabled.

**No MCP actions/tools** — verify public health and OAuth metadata first, then test authenticated MCP tool discovery. Reauthorize or recreate the client connection only after the server path is known to work.

**Reauthentication required** — complete OAuth again using the same MCP URL. Do not rebuild Route64/DNS/TLS when those layers are already healthy.

**VPN/rocket network icon on Linux Mint** — this can be normal while the `rt-direct` WireGuard interface is active; the underlying Wi-Fi can remain connected.

## External services

- Route64: `https://www.route64.org/`
- DuckDNS: `https://www.duckdns.org/`
- IPv4-to-IPv6 frontend used by the verified setup: `https://v4-frontend.netiter.com/`
- Let's Encrypt: `https://letsencrypt.org/`
- Model Context Protocol: `https://modelcontextprotocol.io/`

Free-service behavior can change. The stable architecture is: **stable hostname -> trusted HTTPS -> authenticated MCP frontend -> loopback-only RepoTunnel gateway**.
