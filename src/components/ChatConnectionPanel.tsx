import { FormEvent, useState } from "react";
import type { ChatConnectionStatus } from "../types";

type ChatConnectionPanelProps = {
  status: ChatConnectionStatus;
  gatewayRunning: boolean;
  busy: boolean;
  aiAccessPaused: boolean;
  onConnect: (tunnelId: string, apiKey: string) => Promise<void>;
  onDisconnect: () => Promise<void>;
};

function ChatConnectionPanel({
  status,
  gatewayRunning,
  busy,
  aiAccessPaused,
  onConnect,
  onDisconnect,
}: ChatConnectionPanelProps) {
  const [tunnelId, setTunnelId] = useState("");
  const [apiKey, setApiKey] = useState("");

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    await onConnect(tunnelId.trim(), apiKey);
    setApiKey("");
  }

  const connectionLabel = status.ready
    ? "Connected"
    : status.running
      ? "Starting"
      : "Disconnected";

  return (
    <section className="connection-panel" aria-labelledby="connection-title">
      <div className="connection-heading">
        <div>
          <span className="section-kicker">Advanced OpenAI connection</span>
          <h2 id="connection-title">OpenAI Secure Tunnel</h2>
          <p>
            Use OpenAI tunnel-client only when your OpenAI organization supports Secure MCP Tunnels.
            For normal ChatGPT use, the managed public MCP connection above is the recommended path.
          </p>
        </div>
        <span
          className={`connection-state ${status.ready ? "ready" : status.running ? "starting" : ""}`}
        >
          <span className="status-dot" aria-hidden="true" />
          {connectionLabel}
        </span>
      </div>

      {aiAccessPaused ? (
        <div className="connection-access-banner">
          <strong>AI workspace access is paused</strong>
          <span>Gateway and remote connections may stay online, but MCP project tools are blocked until you resume access from Home.</span>
        </div>
      ) : null}

      <div className="managed-client-summary">
        <div><span>Managed tunnel</span><strong>{status.ready ? "1 connected" : status.running ? "Starting" : "0 connected"}</strong></div>
        <div><span>Runtime</span><strong>{status.clientVersion ?? "Not detected"}</strong></div>
      </div>

      {!status.clientAvailable ? (
        <div className="connection-callout warning-callout">
          <strong>OpenAI Secure Tunnel is unavailable</strong>
          <p>
            tunnel-client is optional. RepoTunnel's managed public MCP connection above does not require this binary.
          </p>
          <code>tunnel-client --version</code>
        </div>
      ) : status.running ? (
        <div className="connected-card">
          <div className="connection-details">
            <div>
              <span>Tunnel</span>
              <code>{status.tunnelId}</code>
            </div>
            <div>
              <span>Local gateway</span>
              <strong>{gatewayRunning ? "Running" : "Offline"}</strong>
            </div>
            <div>
              <span>Runtime</span>
              <strong>{status.clientVersion ?? "tunnel-client"}</strong>
            </div>
          </div>

          {status.message ? <p className="connection-message">{status.message}</p> : null}

          {status.adminUrl ? (
            <div className="runtime-endpoint">
              <span>Local tunnel dashboard</span>
              <code>{status.adminUrl}</code>
            </div>
          ) : null}

          <div className="connection-next-step">
            <strong>{status.ready ? "Final ChatGPT step" : "Checking tunnel readiness"}</strong>
            <p>
              {status.ready
                ? "In ChatGPT Developer mode, create/select a custom app with Connection: Tunnel and choose this same tunnel ID. Then enable RepoTunnel for the conversation."
                : "RepoTunnel is waiting for tunnel-client to finish its local MCP and OpenAI control-plane checks."}
            </p>
          </div>

          <button
            className="secondary-button danger-outline"
            type="button"
            onClick={() => void onDisconnect()}
            disabled={busy}
          >
            {busy ? "Working…" : "Disconnect OpenAI tunnel"}
          </button>
        </div>
      ) : (
        <form className="connection-form" onSubmit={handleSubmit}>
          <div className="connection-callout">
            <strong>First-time setup</strong>
            <p>
              Create a tunnel in OpenAI Platform and a Runtime API key with Tunnels Read + Use.
              The API key below stays in memory only and is cleared from this form after launch.
            </p>
          </div>

          <label className="field-label" htmlFor="tunnel-id">
            Tunnel ID
            <input
              id="tunnel-id"
              type="text"
              spellCheck={false}
              autoComplete="off"
              placeholder="tunnel_0123456789abcdef0123456789abcdef"
              value={tunnelId}
              onChange={(event) => setTunnelId(event.target.value)}
              disabled={busy}
              required
            />
          </label>

          <label className="field-label" htmlFor="runtime-api-key">
            Runtime API key
            <input
              id="runtime-api-key"
              type="password"
              autoComplete="off"
              placeholder="Enter the runtime key"
              value={apiKey}
              onChange={(event) => setApiKey(event.target.value)}
              disabled={busy}
              required
            />
          </label>

          <div className="connection-form-footer">
            <span>
              {status.clientVersion ? `Detected ${status.clientVersion}` : "tunnel-client detected"}
            </span>
            <button
              className="primary-button"
              type="submit"
              disabled={busy || !tunnelId.trim() || !apiKey}
            >
              {busy ? "Connecting…" : "Connect OpenAI tunnel"}
            </button>
          </div>
        </form>
      )}
    </section>
  );
}

export default ChatConnectionPanel;
