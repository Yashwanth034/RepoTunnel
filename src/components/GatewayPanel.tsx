import { useState } from "react";
import type { ChatConnectionStatus, GatewayStatus, PublicTunnelStatus } from "../types";

type GatewayPanelProps = {
  status: GatewayStatus;
  connection: ChatConnectionStatus;
  publicTunnel: PublicTunnelStatus;
  aiAccessPaused: boolean;
  busy: boolean;
  onToggle: () => void;
};

function GatewayPanel({ status, connection, publicTunnel, aiAccessPaused, busy, onToggle }: GatewayPanelProps) {
  const [copied, setCopied] = useState(false);
  const actionLabel = status.running ? "Stop gateway" : "Start gateway";
  const mcpEndpoint = status.running && status.port ? `http://127.0.0.1:${status.port}/mcp` : null;
  const remoteReady = publicTunnel.ready || connection.ready;
  const managedClientState = publicTunnel.ready ? "Public MCP ready" : connection.ready ? "Connected" : connection.running ? "Starting" : "Not connected";

  async function copyEndpoint() {
    if (!mcpEndpoint) return;
    try {
      await navigator.clipboard.writeText(mcpEndpoint);
    } catch {
      const textarea = document.createElement("textarea");
      textarea.value = mcpEndpoint;
      textarea.style.position = "fixed";
      textarea.style.opacity = "0";
      document.body.appendChild(textarea);
      textarea.select();
      document.execCommand("copy");
      textarea.remove();
    }
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1600);
  }

  return (
    <section className="gateway-panel" aria-labelledby="gateway-title">
      <div className="gateway-copy">
        <span className="section-kicker">Local MCP gateway</span>
        <h2 id="gateway-title">{status.running ? "Ready for MCP connections" : "Currently offline"}</h2>
        <p>{status.running ? "Approved workspace tools are available through a loopback-only MCP endpoint. Every request still passes through RepoTunnel’s local security policy." : "Start the local gateway when you want an MCP client to use RepoTunnel. By default it stays private on this machine."}</p>

        {mcpEndpoint ? (
          <div className="mcp-endpoint" aria-label="Local MCP endpoint">
            <div>
              <span>MCP endpoint</span>
              <code>{mcpEndpoint}</code>
              <small>Loopback only · private to this computer</small>
            </div>
            <button type="button" className="secondary-button" onClick={() => void copyEndpoint()}>{copied ? "Copied" : "Copy"}</button>
          </div>
        ) : null}

        <div className="gateway-client-summary" aria-label="Gateway client status">
          <div>
            <span>Remote connection</span>
            <strong>{publicTunnel.ready ? "Managed public MCP" : connection.running ? "OpenAI Secure Tunnel" : "None"}</strong>
            <small>{managedClientState}</small>
          </div>
          <div>
            <span>Workspace access</span>
            <strong>{aiAccessPaused ? "Paused" : "Available"}</strong>
            <small>{aiAccessPaused ? "MCP project tools are blocked" : "Approved projects follow local policy"}</small>
          </div>
        </div>

        <p className="gateway-client-note">Direct MCP clients can use the endpoint above. RepoTunnel shows a client name when the managed connection reports one; generic HTTP clients may not provide a reliable identity.</p>
      </div>

      <div className="gateway-actions">
        <div className="metric"><strong>{status.workspaceCount}</strong><span>{status.workspaceCount === 1 ? "project" : "projects"}</span></div>
        <div className="metric"><strong>{remoteReady ? 1 : 0}</strong><span>{remoteReady ? "remote link" : "remote links"}</span></div>
        <button className={`primary-button ${status.running ? "danger-button" : ""}`} type="button" onClick={onToggle} disabled={busy}>{busy ? "Working…" : actionLabel}</button>
      </div>
    </section>
  );
}

export default GatewayPanel;
