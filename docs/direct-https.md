# RepoTunnel Direct HTTPS Setup

**Verified Route64 + WireGuard + DuckDNS + Let's Encrypt path**

| | |
|---|---|
| **Public endpoint** | HTTPS + OAuth + MCP |
| **Core path** | Route64 → WireGuard → RepoTunnel Direct HTTPS |
| **Monthly infrastructure cost** | No mandatory monthly fee for the documented free-service path |

> **Security note:** This guide intentionally uses placeholders and never includes private keys, account passwords, DuckDNS tokens, OAuth tokens, router credentials, or TLS private keys.

*Verified on Linux Mint*

## Contents

1. What this setup achieves
2. Cost and guarantees
3. Prerequisites
4. Why Route64 is needed on CGNAT
- Part A — Create the public IPv6 path
- Part B — Give the service a stable hostname
- Part C — Enable RepoTunnel Direct HTTPS
- Part D — OAuth and MCP compatibility
- Part E — Connect ChatGPT
- Part F — Restart and network-change behavior
- Part G — Security requirements
- Part H — Verified troubleshooting
- Part I — Recommended RepoTunnel product UX
- Part J — Final checklist and quick reference

---

## 1. What this setup achieves

```
ChatGPT / remote MCP client
 |
 | HTTPS + OAuth
 v
https://YOUR-NAME.duckdns.org/mcp
 |
 | IPv4 clients: free IPv4-to-IPv6 frontend
 | IPv6 clients: direct IPv6
 v
Route64 public IPv6
 |
 | WireGuard tunnel
 v
Your Linux computer
 |
 v
RepoTunnel Direct HTTPS frontend
 |
 | local reverse proxy
 v
127.0.0.1:43555 RepoTunnel MCP gateway
 |
 v
Approved local workspaces
```

The important design rule: the real MCP gateway stays loopback-only. Port 43555 is never exposed directly to the Internet. The public Direct HTTPS frontend exposes only the exact MCP/OAuth/health routes that remote MCP clients need.

```
TCP 443 -> RepoTunnel :43444
TCP 80  -> RepoTunnel :44666 (ACME only)
```

## 2. Cost and guarantees

This path has no mandatory monthly infrastructure bill when using the free services described here:

- Route64 WireGuard IPv6 tunnel — free/best-effort service
- DuckDNS hostname — free dynamic DNS
- Let's Encrypt TLS certificate — free certificate issuance/renewal
- IPv4-to-IPv6 frontend — free/best-effort service so IPv4-only clients can reach the IPv6 origin
- RepoTunnel — runs on your own computer

This should be described as **zero mandatory monthly cost**, not as a commercial uptime or bandwidth guarantee. Free third-party services can change limits or availability.

The laptop/desktop must be powered on, connected to the Internet, with the WireGuard tunnel active and RepoTunnel running, for the MCP URL to work.

## 3. Prerequisites

1. A Linux machine (verified on Linux Mint)
2. RepoTunnel with the Direct HTTPS provider
3. A Route64 account and a WireGuard tunnel
4. WireGuard tools installed (`wg` and `wg-quick`)
5. nftables support, for redirecting inbound public ports to RepoTunnel's local Direct HTTPS ports
6. A DuckDNS hostname
7. OpenSSL
8. Certbot (a rootless Certbot via pipx works if no system Certbot is installed)
9. A browser, for the Route64, DuckDNS, and ChatGPT OAuth authorization steps

Useful package check:

```bash
command -v wg
command -v wg-quick
command -v nft
command -v openssl
command -v pipx || true
```

**Never publish or commit:**

- WireGuard private keys
- Route64 account password
- DuckDNS token
- OAuth access/refresh tokens
- Router/ISP credentials
- TLS private keys

## 4. Why Route64 is needed on CGNAT connections

A normal home connection may look like it has a public IPv4 address, but the router's WAN address can still be private — that's CGNAT (Carrier-Grade NAT).

```
Public Internet IPv4 seen by websites: x.x.x.x
Router WAN IPv4: 10.x.x.x / 100.64.x.x / other private range
```

When the router's WAN address is private, normal router port forwarding cannot make the computer reachable from the public Internet.

Route64 solves this by giving the computer a globally routable IPv6 address through a WireGuard tunnel. The tunnel is initiated outbound, so it works through CGNAT.

If you already have a stable, globally routable IPv6 address from your ISP, Route64 can be skipped — the rest of this guide still applies conceptually.

---

## Part A — Create the public IPv6 path

### 5. Create a Route64 WireGuard tunnel

1. Create/sign in to a Route64 account.
2. Create a new tunnel.
3. Select WireGuard.
4. Choose a Route64 server/hub close to you.
5. Download or copy the generated WireGuard configuration.

The generated configuration looks like:

```ini
[Interface]
PrivateKey = <YOUR_PRIVATE_KEY>
Address = <YOUR_ROUTE64_IPV6>/64

[Peer]
PublicKey = <ROUTE64_SERVER_PUBLIC_KEY>
Endpoint = <ROUTE64_SERVER_IP>:<PORT>
AllowedIPs = ::/1, 8000::/1
PersistentKeepalive = 15
```

Important details from the verified setup:

- Keep the Route64-assigned prefix as given — in the verified setup it was `/64`; don't arbitrarily change it to `/128`.
- Keep Route64's generated `AllowedIPs` values.
- Keep `PersistentKeepalive = 15` when present.
- Do not share the `PrivateKey`.

**Linux interface-name limit:** interface names are capped at 15 characters. Use something short, e.g. `rt-direct` — not `repotunnel-direct`.

Typical config location:

```
/etc/wireguard/rt-direct.conf
```

Set safe permissions:

```bash
sudo chmod 600 /etc/wireguard/rt-direct.conf
```

### 6. Start Route64 automatically at boot

```bash
sudo systemctl enable --now wg-quick@rt-direct
```

Verify:

```bash
systemctl is-active wg-quick@rt-direct
sudo wg show rt-direct
ip -6 addr show dev rt-direct
```

Expected result:

- `wg-quick@rt-direct` is active
- The interface shows the Route64-assigned global IPv6 address
- `wg show` shows a recent handshake once traffic passes through the tunnel

Linux Mint may show a VPN/rocket-style network icon while this interface is active — that's normal. Wi-Fi stays connected underneath the tunnel.

### 7. Redirect public TCP 443 and 80 to RepoTunnel

RepoTunnel's verified Direct HTTPS ports:

| Port | Purpose |
|---|---|
| 43555 | Local MCP gateway — loopback only, never expose publicly |
| 43444 | Direct HTTPS listener |
| 44666 | ACME HTTP challenge listener |

The Linux setup creates an nftables table named `inet repotunnel_direct` and redirects:

- Public TCP 443 → local TCP 43444
- Public TCP 80 → local TCP 44666

Rules must target the Route64 public IPv6 path and must not expose port 43555:

```
table inet repotunnel_direct {
 chain prerouting {
 type nat hook prerouting priority dstnat; policy accept;
 ip6 daddr <YOUR_ROUTE64_IPV6> tcp dport 443 redirect to :43444
 ip6 daddr <YOUR_ROUTE64_IPV6> tcp dport 80 redirect to :44666
 }
}
```

RepoTunnel's Linux helper can automate the WireGuard/service/nftables setup — use your own Route64-generated values, never copy another user's addresses or keys.

Verify:

```bash
sudo nft list table inet repotunnel_direct
```

Do not add DMZ rules, and do not expose the router's admin interface.

### 8. Confirm the Route64 IPv6 is reachable

Before configuring a hostname, confirm the tunnel is active:

```bash
systemctl is-active wg-quick@rt-direct
ip -6 addr show dev rt-direct
```

Once RepoTunnel Direct HTTPS is running, inbound TCP 443 must reach 43444, and inbound TCP 80 must reach 44666 during Let's Encrypt HTTP-01 validation. The verified setup passed an external TCP 80 check and completed Let's Encrypt issuance through this path.

---

## Part B — Give the service a stable hostname

### 9. Create a DuckDNS hostname

Create a DuckDNS subdomain, e.g. `my-example.duckdns.org` (use your own — not another user's).

Configure:

```
AAAA / IPv6 -> your Route64-assigned public IPv6
```

### 10. Add IPv4 compatibility

The verified setup also needed an IPv4 path, since some remote-service infrastructure connects over IPv4 even when the origin is IPv6-only.

The solution used: `v4-frontend.netiter.com` — a free IPv4-to-IPv6 frontend.

At the time of the verified setup, DuckDNS's A record (current IPv4 field) pointed to `116.202.1.213`, and its AAAA field pointed to the Route64 IPv6 address.

Treat the IPv4 frontend address as service-controlled information — before publishing a README, tell users to check https://v4-frontend.netiter.com/ for the current address instead of assuming an old IP is permanent.

Final dual-path hostname:

```
IPv4 client -> IPv4 frontend -> IPv6 origin -> Route64 -> laptop
IPv6 client ------------------> IPv6 origin -> Route64 -> laptop
```

TLS still terminates on your own RepoTunnel instance — the frontend only makes the IPv6 origin reachable from IPv4 clients.

---

## Part C — Enable RepoTunnel Direct HTTPS

### 11. Configure the Direct HTTPS provider

In RepoTunnel, choose the Direct HTTPS public provider. Set the public URL to your DuckDNS hostname:

- Provider: Direct HTTPS
- Public URL: `https://my-example.duckdns.org`
- Auto-start: enabled

The MCP URL becomes:

```
https://my-example.duckdns.org/mcp
```

### 12. Public route allowlist

The Direct HTTPS frontend exposes only:

```
/mcp
/.well-known/oauth-protected-resource
/.well-known/oauth-protected-resource/mcp
/.well-known/oauth-authorization-server
/register
/authorize
/token
/health
```

Everything else stays unavailable/404 — this keeps RepoTunnel's app UI, files, settings, history, and other local surfaces off the public web.

### 13. Get a trusted Let's Encrypt certificate

Use RepoTunnel's trusted-certificate action after Route64, the port 80 redirect, and the DuckDNS hostname are working. Target hostname: `my-example.duckdns.org`.

```
Let's Encrypt HTTP-01
 -> hostname port 80
 -> Route64 IPv6
 -> nftables redirect
 -> /.well-known/acme-challenge/<token>
```

RepoTunnel uses the ACME HTTP challenge listener on local port 44666; the public port mapping makes that reachable on Internet TCP 80 for the ACME path only.

After issuance, RepoTunnel should show:

```
TLS certificate: Trusted
HTTPS listener: :43444 online
https://my-example.duckdns.org
https://my-example.duckdns.org/mcp -> RepoTunnel :44666
```

RepoTunnel should keep the real MCP gateway bound to `127.0.0.1:43555`. The Direct HTTPS layer listens on 43444 and proxies only approved routes to the local gateway.

The certificate private key must stay in RepoTunnel's private application-data directory and must never be committed to the repository.

### 14. Verify public HTTPS before adding ChatGPT

```bash
curl -4 -i https://my-example.duckdns.org/health
```

Expected:

```
HTTP/2 200
content-type: application/json
...
{"service":"RepoTunnel"}
```

This single check verifies: DuckDNS resolution, the IPv4 compatibility path, Route64 reachability, TLS trust, the Direct HTTPS listener, and RepoTunnel responding publicly.

Optionally, test IPv6 directly:

```bash
curl -4 -i https://my-example.duckdns.org/health
curl -6 -i https://my-example.duckdns.org/health
```

---

## Part D — OAuth and MCP compatibility

### 15. Verify OAuth resource metadata

```bash
curl -4 -sS https://my-example.duckdns.org/.well-known/oauth-protected-resource/mcp
```

Expected structure:

```json
{
 "authorization_servers": ["https://my-example.duckdns.org"],
 "bearer_methods_supported": ["header"],
 "resource": "https://my-example.duckdns.org/mcp"
}
```

The hostname in the metadata must match the public hostname the client is actually using.

### 16. Verify OAuth authorization-server metadata

```bash
curl -4 -sS https://my-example.duckdns.org/.well-known/oauth-authorization-server
```

Expected:

```json
{
 "authorization_endpoint": "https://my-example.duckdns.org/authorize",
 "code_challenge_methods_supported": ["S256"],
 "grant_types_supported": ["authorization_code", "refresh_token"],
 "issuer": "https://my-example.duckdns.org",
 "registration_endpoint": "https://my-example.duckdns.org/register",
 "response_types_supported": ["code"],
 "token_endpoint": "https://my-example.duckdns.org/token",
 "token_endpoint_auth_methods_supported": ["none"]
}
```

RepoTunnel uses OAuth + PKCE for the remote MCP connection.

### 17. Critical reverse-proxy rule: never forward the public Host header to the loopback gateway

This was essential to the successful implementation. The local gateway protects itself against DNS rebinding and correctly rejects an Internet hostname as its direct Host header.

**Bad behavior:**

```
Public request Host: my-repotunnel.duckdns.org
 |
 v
proxy forwards same Host to 127.0.0.1:43555
 |
 v
403 Forbidden: Host header is not allowed
```

**Correct behavior:**

- Do not copy the public Host header to the local gateway.
- Let the HTTP client generate the local upstream Host from 127.0.0.1:43555.
- Preserve the original hostname in `X-Forwarded-Host`.
- Set `X-Forwarded-Proto: https`.

Working Rust pattern:

```rust
let mut upstream = state.client.request(parts.method, upstream_url);
for (name, value) in &parts.headers {
 if !hop_by_hop_header(name) && !name.as_str().eq_ignore_ascii_case("host") {
 upstream = upstream.header(name, value);
 }
}
upstream = upstream.header("x-forwarded-proto", "https");
if let Some(host) = original_host.as_ref() {
 upstream = upstream.header("x-forwarded-host", host);
}
```

Don't "solve" the 403 by weakening the gateway's Host validation — the proxy should adapt correctly while the loopback gateway stays protected.

### 18. MCP HTTP behavior

The verified RepoTunnel build uses the current RMCP SDK and Streamable HTTP configuration:

- Legacy session mode: disabled
- JSON responses: enabled
- For MCP protocol versions that require it, requests include the `Mcp-Method` header

The final authenticated diagnostic returned:

```
TOOLS HTTP: 200
TOOLS COUNT: 57
```

That confirms the entire OAuth + MCP + Direct HTTPS path, not just the health endpoint.

---

## Part E — Connect ChatGPT

### 19. Create the RepoTunnel connector/app in ChatGPT

1. Create a new custom MCP connection/app.
2. Name it "RepoTunnel" (or another recognizable name).
3. Use the MCP server URL: `https://my-example.duckdns.org/mcp`
4. Choose OAuth authentication.
5. Allow the OAuth metadata to be discovered from RepoTunnel.
6.Set Registration method to:Dynamic Client Registration (DCR)
6. If the UI has a "Base scopes" field, the verified configuration used `offline_access`.
7. Complete RepoTunnel's browser authorization page.
8. Allow ChatGPT to scan/refresh actions.

The UI wording can change over time — the important pieces are the MCP URL, OAuth, authorization completion, and a successful action/tool scan.

### 20. Verify the connector with a real tool call

Don't stop at "Connected" — test an actual tool. In a fresh ChatGPT conversation:

```
Use RepoTunnel and list my workspaces.
```

A successful setup returns your actual approved RepoTunnel workspaces — the final proof that:

```
ChatGPT -> HTTPS -> OAuth -> MCP -> RepoTunnel tool discovery -> RepoTunnel tool execution -> local approval
```

all work end to end.

If a connector was deleted/recreated while an old ChatGPT conversation was still open, that old conversation may retain a stale connector session — test from a fresh chat for a clean final verification.

---

## Part F — Restart and network-change behavior

### 21. Make WireGuard persistent

```bash
sudo systemctl enable wg-quick@rt-direct
```

After a reboot, verify:

```bash
systemctl is-active wg-quick@rt-direct
sudo wg show rt-direct
```

### 22. Keep the same URL when changing Wi-Fi

The public MCP hostname doesn't depend on the local Wi-Fi address:

```
new Wi-Fi / mobile hotspot
 -> outbound Internet
 -> WireGuard reconnects to Route64
 -> same Route64 public IPv6
 -> same DuckDNS hostname
```

So you normally don't need to recreate the ChatGPT connector when switching between home Wi-Fi, another Wi-Fi network, or a mobile hotspot. The new network only needs to allow outbound WireGuard/UDP — a network that blocks it can temporarily prevent the public route from working.

### 23. Three quick health checks after reboot or Wi-Fi change

**Check 1 — WireGuard**

```bash
systemctl is-active wg-quick@rt-direct
sudo wg show rt-direct
```

**Check 2 — RepoTunnel public HTTPS**

```bash
curl -4 https://my-example.duckdns.org/health
```

Expected: `{"service":"RepoTunnel"}`

**Check 3 — ChatGPT tool**

```
Use RepoTunnel and list my workspaces.
```

If all three pass, the complete path is healthy.

---

## Part G — Security requirements

### 24. Keep the local gateway private

- `127.0.0.1:43555` stays loopback-only.
- Never forward router port 43555.
- Never bind the raw MCP gateway to `0.0.0.0` for convenience.
- Only the Direct HTTPS frontend should be reachable publicly.

### 25. Keep OAuth enabled

Do not disable authentication to make tool scanning easier. A public MCP endpoint with local file/terminal capabilities is powerful, so the authentication boundary matters.

### 26. Keep the public route allowlist small

Expose only MCP/OAuth/health routes. Do not expose: the RepoTunnel desktop UI, project browser UI, settings, history, checkpoints, arbitrary local file paths, or router administration.

### 27. Keep private material out of documentation

A public README **may** contain:

- Port numbers
- Architecture
- Public service names
- Generic commands
- Placeholders

A public README **must not** contain:

- WireGuard private keys
- Route64 passwords
- DuckDNS tokens
- OAuth tokens
- TLS private keys
- ISP/router passwords

Use placeholders such as:

```
<YOUR_ROUTE64_IPV6>
<YOUR_PRIVATE_KEY>
<YOUR_DUCKDNS_HOSTNAME>
```

---

## Part H — Verified troubleshooting

### 28. Health endpoint returns TLS EOF / connection failure

Check RepoTunnel first. Expected UI state:

```
Local gateway: Online
TLS certificate: Trusted
HTTPS listener: :44544 online
```

```bash
ss -ltnp | grep -E ':(43444|43444|43455) '
```

If 43444 is offline, the public HTTPS request can't complete TLS.

### 29. "403 Forbidden: Host header is not allowed"

Cause: the Direct HTTPS reverse proxy forwarded the public DuckDNS Host header to the loopback gateway.

Fix: strip the public Host on the upstream request, preserve it only as `X-Forwarded-Host`, and let the upstream HTTP client generate the loopback Host. Don't weaken the gateway's Host security check.

### 30. ChatGPT says no actions are available

Verify the MCP server itself first — the successful authenticated diagnostic returned `TOOLS HTTP: 200`, `TOOLS COUNT: 57`. If `tools/list` works independently, recreate/reauthorize the ChatGPT connector and scan/refresh actions again. Test from a fresh conversation if an old chat may have a stale session.

### 31. "Reauthentication required"

Complete OAuth authorization again, or recreate the connector using the same MCP URL. Don't change Route64, DuckDNS, or TLS settings when public health and authenticated MCP tests already work.

### 32. Linux shows a rocket/VPN icon instead of the normal Wi-Fi icon

Normal when `rt-direct` is active — Linux Mint may show the WireGuard/VPN connection as the primary network indicator even though the underlying Wi-Fi remains connected.

### 33. Router port forwarding is not required for the Route64 path

The verified setup confirmed CGNAT at the ISP, which traditional router forwarding can't solve. Route64 provides the inbound IPv6 route through the outbound WireGuard tunnel, so this path doesn't depend on the home router having a public IPv4 WAN address.

---

## Part I — Recommended RepoTunnel product UX

### 34. Connection choices for normal users

RepoTunnel should present its providers roughly like this:

- **ngrok** — easiest/default, for users who want the simplest setup and already use ngrok
- **Cloudflare Tunnel** — alternative managed tunnel, for users who prefer Cloudflare
- **Direct HTTPS** — advanced / zero-relay-cost path, for users who have native public IPv4/IPv6 or a routed IPv6 tunnel such as Route64

Direct HTTPS shouldn't require every RepoTunnel user to manually reproduce the Route64 configuration — it's an advanced option for users who need it.

---

## Part J — Final checklist and quick reference

Before declaring a Direct HTTPS installation complete, verify every item below.

**Network**

- [ ] Route64 WireGuard tunnel is active
- [ ] Interface uses a short Linux-safe name such as `rt-direct`
- [ ] Route64 global IPv6 is present on the interface
- [ ] WireGuard service is enabled at boot
- [ ] TCP 443 reaches RepoTunnel 43444
- [ ] TCP 80 reaches RepoTunnel 44666 for ACME
- [ ] Raw MCP gateway 43555 is not public

**DNS / TLS**

- [ ] DuckDNS AAAA/IPv6 points to the Route64 IPv6
- [ ] IPv4 clients have a working IPv4-to-IPv6 path when required
- [ ] `https://HOST/health` returns HTTP 200
- [ ] TLS certificate is trusted
- [ ] Certificate hostname matches the DuckDNS hostname

**OAuth**

- [ ] OAuth protected-resource metadata uses the public hostname
- [ ] Authorization, token, and registration endpoints use the public hostname
- [ ] PKCE S256 is supported
- [ ] Refresh-token grant is supported

**MCP**

- [ ] Public `/mcp` reaches the local gateway through the Direct HTTPS proxy
- [ ] Public Host header is not forwarded as the local gateway Host
- [ ] MCP tool discovery succeeds
- [ ] Authenticated `tools/list` returns the RepoTunnel tools

**ChatGPT**

- [ ] Connector/app uses `https://HOST/mcp`
- [ ] OAuth authorization completes
- [ ] Actions/tools are visible after scan/refresh
- [ ] A real RepoTunnel tool call succeeds in a fresh conversation

**Restart / network changes**

- [ ] `wg-quick@rt-direct` comes back after reboot
- [ ] RepoTunnel Direct HTTPS auto-starts
- [ ] Same hostname works after Wi-Fi/hotspot change when WireGuard is allowed

### Quick reference commands

*(replace `my-example.duckdns.org` with your own hostname)*

```bash
# WireGuard status
systemctl is-active wg-quick@rt-direct
sudo wg show rt-direct
ip -6 addr show dev rt-direct

# Public health
curl -4 -i https://my-example.duckdns.org/health

# OAuth resource metadata
curl -4 -sS https://my-example.duckdns.org/.well-known/oauth-protected-resource/mcp

# OAuth server metadata
curl -4 -sS https://my-example.duckdns.org/.well-known/oauth-authorization-server

# RepoTunnel listeners
ss -ltnp | grep -E ':(43555|43444|44666) '

# nftables public redirect rules
sudo nft list table inet repotunnel_direct
```

Expected public health body: `{"service":"RepoTunnel"}`

### Links

- Route64 — https://www.route64.org/
- DuckDNS — https://www.duckdns.org/
- IPv4-to-IPv6 frontend used in the verified setup — https://v4-frontend.netiter.com/
- Let's Encrypt — https://letsencrypt.org/
- Model Context Protocol — https://modelcontextprotocol.io/
- OpenAI Help Center (MCP / developer-mode guidance) — https://help.openai.com/

---

Service behavior and free-tier policies can change — the architecture is the part that stays stable: stable hostname → trusted HTTPS → authenticated MCP frontend → loopback-only RepoTunnel gateway.

A successful installation keeps the same URL (`https://my-example.duckdns.org/mcp`) even when the computer restarts, the underlying Wi-Fi changes, you switch to a mobile hotspot, or the local LAN address changes. The constant identity is the public hostname and Route64 tunnel, not the local Wi-Fi address.

The final verification isn't just a successful curl — it's a real remote MCP tool call from ChatGPT into an approved local RepoTunnel workspace, with the raw gateway staying private and OAuth staying enabled.
