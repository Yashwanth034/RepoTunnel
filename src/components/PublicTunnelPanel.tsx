import { FormEvent, useMemo, useState } from "react";
import type { PublicTunnelStatus } from "../types";

type PublicTunnelPanelProps = {
  status: PublicTunnelStatus;
  gatewayRunning: boolean;
  busy: boolean;
  onConfigure: (authtoken: string) => Promise<void>;
  onRestart: () => Promise<void>;
  onRevoke: () => Promise<void>;
  onForget: () => Promise<void>;
};

function relativeTime(timestamp: number | null): string {
  if (!timestamp) return "No remote request seen yet";
  const diff = Math.max(0, Date.now() - timestamp);
  if (diff < 10_000) return "Remote request seen just now";
  if (diff < 60_000) return `Remote request seen ${Math.floor(diff / 1000)}s ago`;
  if (diff < 3_600_000) return `Remote request seen ${Math.floor(diff / 60_000)}m ago`;
  return `Remote request seen ${Math.floor(diff / 3_600_000)}h ago`;
}

function PublicTunnelPanel({
  status,
  gatewayRunning,
  busy,
  onConfigure,
  onRestart,
  onRevoke,
  onForget,
}: PublicTunnelPanelProps) {
  const [authtoken, setAuthtoken] = useState("");
  const [copied, setCopied] = useState(false);
  const activity = useMemo(() => relativeTime(status.lastRemoteRequestAt), [status.lastRemoteRequestAt]);
  const reconnecting = status.running && !status.ready && /reconnect|interrupt/i.test(status.message ?? "");
  const publicStateLabel = status.ready ? "Ready" : status.running ? (reconnecting ? "Reconnecting" : "Connecting") : status.configured ? "Offline" : "Setup needed";

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    try {
      await onConfigure(authtoken.trim());
      setAuthtoken("");
    } catch {
      // Keep the token in the field so the user can correct a failed setup attempt.
    }
  }

  async function copyMcpUrl() {
    if (!status.mcpUrl) return;
    try {
      await navigator.clipboard.writeText(status.mcpUrl);
    } catch {
      const textarea = document.createElement("textarea");
      textarea.value = status.mcpUrl;
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
    <section className="connection-panel public-tunnel-panel" aria-labelledby="public-tunnel-title">
      <div className="connection-heading">
        <div>
          <span className="section-kicker">Recommended ChatGPT connection</span>
          <h2 id="public-tunnel-title">Managed public MCP endpoint</h2>
          <p>
            RepoTunnel embeds the ngrok connection itself. Each installation uses that user&apos;s own
            ngrok account and keeps its assigned public address across restarts, so the ChatGPT plugin
            normally needs to be connected only once.
          </p>
        </div>
        <span className={`connection-state ${status.ready ? "ready" : status.running ? "starting" : ""}`}>
          <span className="status-dot" aria-hidden="true" />
          {publicStateLabel}
        </span>
      </div>

      {!status.configured ? (
        <form className="connection-form" onSubmit={handleSubmit}>
          <div className="connection-callout">
            <strong>One-time setup per user</strong>
            <p>
              Create a free ngrok account and paste that account&apos;s authtoken here once. RepoTunnel
              stores it only in this user&apos;s private app-data file and never ships a developer token or
              public URL inside the application.
            </p>
          </div>

          <label className="field-label" htmlFor="ngrok-authtoken">
            ngrok authtoken
            <input
              id="ngrok-authtoken"
              type="password"
              autoComplete="off"
              spellCheck={false}
              placeholder="Paste your ngrok authtoken"
              value={authtoken}
              onChange={(event) => setAuthtoken(event.target.value)}
              disabled={busy}
              required
            />
          </label>

          <div className="connection-form-footer">
            <span>No RepoTunnel developer credential is bundled.</span>
            <button className="primary-button" type="submit" disabled={busy || authtoken.trim().length < 20}>
              {busy ? "Connecting…" : "Set up & connect"}
            </button>
          </div>
        </form>
      ) : (
        <div className="connected-card public-connected-card">
          <div className="managed-client-summary public-health-grid">
            <div><span>Local gateway</span><strong>{gatewayRunning ? "Online" : "Offline"}</strong></div>
            <div><span>Public tunnel</span><strong>{status.ready ? "Verified online" : status.running ? (reconnecting ? "Reconnecting" : "Connecting") : "Offline"}</strong></div>
            <div><span>Launch auto-connect</span><strong>{status.autoStart ? "On" : "Off"}</strong></div>
            <div><span>Remote requests</span><strong>{status.requestCount}</strong></div>
          </div>

          {status.mcpUrl ? (
            <div className="runtime-endpoint public-mcp-endpoint">
              <div>
                <span>Stable ChatGPT MCP URL</span>
                <code>{status.mcpUrl}</code>
                <small>{activity}</small>
              </div>
              <button type="button" className="secondary-button" onClick={() => void copyMcpUrl()}>
                {copied ? "Copied" : "Copy"}
              </button>
            </div>
          ) : null}

          <div className="connection-next-step">
            <strong>{status.ready ? "Connect this URL in ChatGPT once" : "Connection needs attention"}</strong>
            <p>
              {status.ready
                ? "After the ChatGPT plugin/app uses this MCP URL, normal RepoTunnel restarts reuse the same saved public address. RepoTunnel also retries the public forwarder automatically after connection interruptions. Reconnect the ChatGPT plugin only when the MCP tool schema itself changes or you reset this public connection."
                : (status.message ?? "RepoTunnel will retry recoverable public-connection interruptions automatically. Use Restart connection if setup or authentication needs attention.")}
            </p>
          </div>

          <div className="public-connection-actions">
            <button className="secondary-button" type="button" onClick={() => void onRestart()} disabled={busy}>
              {busy ? "Working…" : "Restart connection"}
            </button>
            <button className="secondary-button danger-outline" type="button" onClick={() => void onRevoke()} disabled={busy}>
              Revoke MCP access
            </button>
            <button className="secondary-button danger-outline" type="button" onClick={() => void onForget()} disabled={busy}>
              Forget public setup
            </button>
          </div>
        </div>
      )}
    </section>
  );
}

export default PublicTunnelPanel;
