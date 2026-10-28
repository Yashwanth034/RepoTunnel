import type { ReactNode } from "react";

const platformTunnelsUrl = "https://platform.openai.com/settings/organization/tunnels";
const runtimeKeysUrl = "https://platform.openai.com/settings/organization/api-keys";
const chatGptConnectorsUrl = "https://chatgpt.com/";
const officialGuideUrl = "https://developers.openai.com/api/docs/guides/secure-mcp-tunnels";
const officialClientUrl = "https://github.com/openai/tunnel-client/releases/latest";
const onboardingUrl = "https://github.com/openai/tunnel-client/blob/master/docs/onboarding.md";

function ResourceLink({ href, children }: { href: string; children: ReactNode }) {
  return <a href={href} target="_blank" rel="noreferrer">{children}</a>;
}

function Step({
  number,
  title,
  children,
}: {
  number: string;
  title: string;
  children: ReactNode;
}) {
  return (
    <article className="secure-tunnel-step">
      <span className="secure-tunnel-step-number" aria-hidden="true">{number}</span>
      <div className="secure-tunnel-step-body">
        <h4>{title}</h4>
        {children}
      </div>
    </article>
  );
}

function PlatformSample() {
  return (
    <figure className="secure-tunnel-sample">
      <div className="secure-tunnel-sample-window">
        <div className="secure-tunnel-sample-toolbar">
          <span className="secure-tunnel-window-dots" aria-hidden="true"><i /><i /><i /></span>
          <span>platform.openai.com / Settings / Organization / Tunnels</span>
        </div>
        <div className="secure-tunnel-sample-content">
          <div className="secure-tunnel-sample-overline">OPENAI PLATFORM</div>
          <div className="secure-tunnel-sample-title-row">
            <strong>Tunnels</strong>
            <span className="secure-tunnel-sample-pill">Example</span>
          </div>
          <div className="secure-tunnel-sample-record">
            <span className="secure-tunnel-sample-record-icon" aria-hidden="true"><svg viewBox="0 0 24 24"><path d="M5 6.5h14v11H5zM8 10h8M8 13.5h5" /></svg></span>
            <span><strong>RepoTunnel MCP</strong><small>Local MCP workspace connection</small></span>
            <span className="secure-tunnel-sample-status">Available</span>
          </div>
          <div className="secure-tunnel-sample-fields">
            <div><small>Tunnel ID</small><code>tunnel_0123…</code></div>
            <div><small>Workspace scope</small><strong>Your target ChatGPT workspace</strong></div>
          </div>
        </div>
      </div>
      <figcaption>Sample screen: select the tunnel ID and confirm it is associated with the ChatGPT workspace you plan to use.</figcaption>
    </figure>
  );
}

function ChatGptSample() {
  return (
    <figure className="secure-tunnel-sample">
      <div className="secure-tunnel-sample-window">
        <div className="secure-tunnel-sample-toolbar">
          <span className="secure-tunnel-window-dots" aria-hidden="true"><i /><i /><i /></span>
          <span>chatgpt.com / Plugins / Add custom MCP server</span>
        </div>
        <div className="secure-tunnel-sample-content">
          <div className="secure-tunnel-sample-overline">CHATGPT CUSTOM MCP</div>
          <div className="secure-tunnel-sample-title-row">
            <strong>Add custom MCP server</strong>
            <span className="secure-tunnel-sample-pill">Example</span>
          </div>
          <div className="secure-tunnel-sample-fields stacked">
            <div><small>Connection</small><strong className="secure-tunnel-sample-selected">Tunnel <span aria-hidden="true">⌄</span></strong></div>
            <div><small>Tunnel</small><strong>RepoTunnel MCP · tunnel_0123…</strong></div>
          </div>
          <div className="secure-tunnel-sample-note">Authentication: No authentication (RepoTunnel already authenticates the local tunnel-client with a private process-bound token). Keep the tunnel connected for discovery and tool calls.</div>
        </div>
      </div>
      <figcaption>Sample screen: choose <strong>Connection: Tunnel</strong> and select or paste the same tunnel ID.</figcaption>
    </figure>
  );
}

function SecureMcpTunnelGuide() {
  return (
    <section className="secure-tunnel-guide" aria-labelledby="secure-tunnel-guide-title">
      <header className="secure-tunnel-guide-hero">
        <div className="secure-tunnel-guide-hero-copy">
          <span className="section-kicker">PRIVATE MCP CONNECTION</span>
          <h2 id="secure-tunnel-guide-title">Connect ChatGPT with OpenAI Secure MCP Tunnel</h2>
          <p>
            Connect ChatGPT to RepoTunnel through OpenAI's outbound tunnel. The local MCP gateway stays on
            this computer; you do not need to configure Direct HTTPS, ngrok, or Cloudflare for this option.
          </p>
          <div className="secure-tunnel-guide-pills">
            <span><i /> No inbound port required</span>
            <span><i /> Official OpenAI client</span>
            <span><i /> Optional connection method</span>
          </div>
          <aside className="secure-tunnel-privacy-note" role="note">
            <strong>Privacy note</strong>
            <span>
              RepoTunnel's MCP listener stays on this computer and needs no inbound port. However, ChatGPT
              tool requests, arguments, results, and stream events still pass through OpenAI's hosted tunnel.
              Do not use this option if all MCP traffic must remain entirely local.
            </span>
          </aside>
        </div>
        <div className="secure-tunnel-flow" role="img" aria-label="Connection flow: ChatGPT uses OpenAI's hosted tunnel, tunnel-client forwards requests to RepoTunnel's local MCP gateway.">
          <div className="secure-tunnel-flow-node"><strong>ChatGPT</strong><small>Your workspace</small></div>
          <span className="secure-tunnel-flow-link" aria-hidden="true">↕</span>
          <div className="secure-tunnel-flow-node remote"><strong>OpenAI Tunnel</strong><small>Hosted connection</small></div>
          <span className="secure-tunnel-flow-link" aria-hidden="true">↕</span>
          <div className="secure-tunnel-flow-node local"><strong>RepoTunnel</strong><small>Local MCP gateway</small></div>
        </div>
      </header>

      <section className="secure-tunnel-guide-section" aria-labelledby="secure-tunnel-start-title">
        <div className="secure-tunnel-section-heading">
          <div><span className="secure-tunnel-section-kicker">BEFORE YOU START</span><h3 id="secure-tunnel-start-title">Open the official setup pages</h3></div>
          <p>Use OpenAI's pages and the current tunnel-client docs for the exact release and labels.</p>
        </div>
        <div className="secure-tunnel-resource-grid">
          <a className="secure-tunnel-resource-card" href={platformTunnelsUrl} target="_blank" rel="noreferrer">
            <span className="secure-tunnel-resource-icon" aria-hidden="true">↗</span><strong>OpenAI Platform Tunnels</strong><small>Create or find your tunnel and download the supported client.</small>
          </a>
          <a className="secure-tunnel-resource-card" href={runtimeKeysUrl} target="_blank" rel="noreferrer">
            <span className="secure-tunnel-resource-icon" aria-hidden="true">⌁</span><strong>Runtime API keys</strong><small>Create a restricted key with Tunnels Read + Use.</small>
          </a>
          <a className="secure-tunnel-resource-card" href={officialClientUrl} target="_blank" rel="noreferrer">
            <span className="secure-tunnel-resource-icon" aria-hidden="true">↓</span><strong>Official client releases</strong><small>Download only from OpenAI's tunnel-client repository.</small>
          </a>
          <a className="secure-tunnel-resource-card" href={officialGuideUrl} target="_blank" rel="noreferrer">
            <span className="secure-tunnel-resource-icon" aria-hidden="true">☷</span><strong>Official Secure MCP Tunnel guide</strong><small>Current requirements, networking, permissions, and troubleshooting.</small>
          </a>
        </div>
      </section>

      <section className="secure-tunnel-guide-section" aria-labelledby="secure-tunnel-steps-title">
        <div className="secure-tunnel-section-heading">
          <div><span className="secure-tunnel-section-kicker">SETUP</span><h3 id="secure-tunnel-steps-title">Follow these steps in order</h3></div>
          <span className="secure-tunnel-step-count">4 steps</span>
        </div>
        <div className="secure-tunnel-steps">
          <Step number="1" title="Create or select a tunnel">
            <p>Open <ResourceLink href={platformTunnelsUrl}>OpenAI Platform → Tunnels</ResourceLink>. Create a tunnel or reuse one you already have. Copy its full ID exactly, including any namespace OpenAI shows, and make sure it is associated with the ChatGPT workspace you intend to use.</p>
            <p className="secure-tunnel-tip">Permissions: managing a tunnel requires Tunnels Read + Manage. Running it and selecting it in ChatGPT requires Tunnels Read + Use. Workspace permissions for custom MCP servers are separate.</p>
          </Step>
          <Step number="2" title="Install the official tunnel-client">
            <p>Under <strong>Connect → OpenAI Secure Tunnel</strong>, select <strong>Install verified official client</strong> when the client is missing. RepoTunnel obtains the pinned OpenAI v0.0.18 release, checks its official checksum manifest and archive SHA-256, and places the executable in a private per-user directory. No ZIP extraction, terminal command, admin privileges or PATH changes are needed. The download starts only when you click Install and does not connect a tunnel.</p>
            <p>This one-click path currently applies to supported Linux and Windows platforms. It uses OpenAI's narrow <strong>run-only</strong> runtime, which is sufficient for RepoTunnel's MCP connection and does not include unrelated administration or Cloudflared companion executables.</p>
            <p><strong>macOS:</strong> OpenAI's downloaded client ZIP is not notarized. Use the supported Homebrew installation (<code>brew install openai/tools/tunnel-client</code>) rather than bypassing Gatekeeper. RepoTunnel continues detecting existing official manual installations on every platform.</p>
            <p>If an official client is already installed, RepoTunnel continues to use it. The <ResourceLink href={officialClientUrl}>official release downloads</ResourceLink> remain available for advanced setups; the app also checks PATH and common manual-install locations. This in-app installer will not overwrite an existing client or update one that is running.</p>
          </Step>
          <Step number="3" title="Create a restricted runtime API key">
            <p>On <ResourceLink href={runtimeKeysUrl}>OpenAI Platform → Runtime API keys</ResourceLink>, create a restricted key with <strong>Tunnels Read + Use</strong>. Copy the tunnel ID and key separately.</p>
            <div className="secure-tunnel-key-note"><strong>Important</strong><span>The tunnel ID is not a secret. The runtime API key is a secret. Do not use an Admin API key for the running client or paste the key into chat, screenshots, or a project file.</span></div>
          </Step>
          <Step number="4" title="Connect from RepoTunnel">
            <p>Open <strong>Connect</strong> in RepoTunnel. In <strong>OpenAI Secure Tunnel</strong>, enter the same tunnel ID and runtime API key, then click <strong>Connect OpenAI tunnel</strong>.</p>
            <ul>
              <li>The client forwards tunnel requests to RepoTunnel's local MCP gateway.</li>
              <li>Wait for the status to change from <strong>Starting</strong> to <strong>Connected</strong>.</li>
              <li>If shown, open the local tunnel dashboard link to inspect health and readiness.</li>
            </ul>
            <p className="secure-tunnel-tip">The key is provided to the process at launch; RepoTunnel does not save it as a connection setting. Keep the tunnel running for ChatGPT discovery and tool calls.</p>
          </Step>
        </div>
      </section>

      <section className="secure-tunnel-guide-section" aria-labelledby="secure-tunnel-samples-title">
        <div className="secure-tunnel-section-heading">
          <div><span className="secure-tunnel-section-kicker">VISUAL EXAMPLES</span><h3 id="secure-tunnel-samples-title">What to look for</h3></div>
          <p>Illustrative sample screens—not live captures. OpenAI's actual layout and labels may change.</p>
        </div>
        <div className="secure-tunnel-sample-grid">
          <PlatformSample />
          <ChatGptSample />
        </div>
      </section>

      <section className="secure-tunnel-guide-section secure-tunnel-final-section" aria-labelledby="secure-tunnel-chatgpt-title">
        <div className="secure-tunnel-section-heading">
          <div><span className="secure-tunnel-section-kicker">WHEN YOU ARE READY</span><h3 id="secure-tunnel-chatgpt-title">Select the same tunnel in ChatGPT</h3></div>
        </div>
        <ol className="secure-tunnel-chatgpt-steps">
          <li>Keep RepoTunnel connected and wait until its tunnel status says <strong>Connected</strong>.</li>
          <li>Open <ResourceLink href={chatGptConnectorsUrl}>ChatGPT</ResourceLink>. Open Plugins or the custom MCP server area, choose +, select <strong>Add custom MCP server</strong>, and choose <strong>Tunnel</strong> under Connection. The exact navigation can vary by workspace rollout.</li>
          <li>Select the available tunnel or paste the exact same full tunnel ID. Do not paste RepoTunnel's private localhost MCP URL.</li>
          <li>For RepoTunnel's current local tunnel-token mode, select <strong>No authentication</strong> if offered. The tunnel-client supplies a separate process-bound credential to RepoTunnel's local MCP gateway; you do not need to configure the public Direct HTTPS OAuth flow. Review the security warning, confirm and create the plugin.</li>
        </ol>
        <div className="secure-tunnel-troubleshooting">
          <h4>If the tunnel does not appear</h4>
          <p>Confirm the tunnel is associated with the target ChatGPT workspace (not only the Platform organization), verify Tunnels Read + Use permission, and make sure RepoTunnel reports the client as connected and ready. For client-side diagnostics, run <code>tunnel-client help quickstart</code> and consult the <ResourceLink href={onboardingUrl}>official onboarding guide</ResourceLink>. If the client is missing, click <strong>Install verified official client</strong> on Connect or use the official manual download as an advanced alternative.</p>
          <h4>If ChatGPT cannot discover OAuth settings</h4>
          <p>For RepoTunnel's local Secure Tunnel mode, choose <strong>No authentication</strong> in ChatGPT instead of OAuth. RepoTunnel authenticates the local tunnel-client via a private process-bound header; its separate OAuth metadata is for the publicly reachable Direct HTTPS connection. OAuth over a tunnel requires a separately reachable authorization server and is not automatically provided by selecting a tunnel.</p>
        </div>
      </section>

      <footer className="secure-tunnel-guide-footer">
        <span>Prefer public connectivity?</span>
        <p>Direct HTTPS, ngrok, and Cloudflare remain optional alternatives in Connect. This guide does not enable, disable, or change those options.</p>
        <ResourceLink href={onboardingUrl}>Read the complete official tunnel-client onboarding guide ↗</ResourceLink>
      </footer>
    </section>
  );
}

export default SecureMcpTunnelGuide;
