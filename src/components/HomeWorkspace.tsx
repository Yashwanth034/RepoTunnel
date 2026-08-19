import type { ChangeRecord, ChatConnectionStatus, GatewayStatus, PublicTunnelStatus, Workspace } from "../types";
import type { AppView, IconName } from "./AppSidebar";
import { NavIcon } from "./AppSidebar";
import RepoTunnelLogo from "./RepoTunnelLogo";

type HomeWorkspaceProps = {
  gateway: GatewayStatus;
  connection: ChatConnectionStatus;
  publicTunnel: PublicTunnelStatus;
  workspaces: Workspace[];
  changes: ChangeRecord[];
  gatewayBusy: boolean;
  adding: boolean;
  checkpointBusy: boolean;
  safetyBusy: boolean;
  aiAccessBusy: boolean;
  aiAccessPaused: boolean;
  onToggleGateway: () => void;
  onAddProject: () => void;
  onCreateCheckpoint: () => void;
  onSafetyScan: () => void;
  onToggleAiAccess: () => void;
  onNavigate: (view: AppView) => void;
};

function basename(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/\/$/, "");
  return normalized.split("/").filter(Boolean).pop() ?? path;
}

function relativeTime(timestamp: number): string {
  const diff = Math.max(0, Date.now() - timestamp);
  if (diff < 60_000) return "now";
  if (diff < 3_600_000) return `${Math.max(1, Math.floor(diff / 60_000))}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

function ActionCard({
  icon,
  title,
  caption,
  onClick,
  disabled = false,
  active = false,
}: {
  icon: IconName;
  title: string;
  caption: string;
  onClick: () => void;
  disabled?: boolean;
  active?: boolean;
}) {
  return (
    <button
      className={`home-action-card ${active ? "active-safety-action" : ""}`}
      type="button"
      onClick={onClick}
      disabled={disabled}
    >
      <NavIcon name={icon} size={27} />
      <strong>{title}</strong>
      <span>{caption}</span>
    </button>
  );
}

function HomeWorkspace({
  gateway,
  connection,
  publicTunnel,
  workspaces,
  changes,
  gatewayBusy,
  adding,
  checkpointBusy,
  safetyBusy,
  aiAccessBusy,
  aiAccessPaused,
  onToggleGateway,
  onAddProject,
  onCreateCheckpoint,
  onSafetyScan,
  onToggleAiAccess,
  onNavigate,
}: HomeWorkspaceProps) {
  const recent = changes.slice(0, 4);
  const hasProject = workspaces.length > 0;
  const remoteReady = publicTunnel.ready || connection.ready;
  const remoteTitle = publicTunnel.ready ? "Public MCP ready" : connection.ready ? "Connected" : "Disconnected";
  const remoteDetail = aiAccessPaused
    ? "MCP workspace access paused"
    : publicTunnel.ready
      ? (publicTunnel.lastRemoteRequestAt ? "Remote MCP traffic detected" : "Stable public endpoint online")
      : connection.ready
        ? (connection.clientVersion ?? "Managed client connected")
        : "External MCP access";

  return (
    <div className="home-workspace">
      <section className="home-center-stage">
        <div className="home-center-content">
          <RepoTunnelLogo size={74} className="home-product-mark" />
          <h2>{aiAccessPaused ? "AI access is paused." : "Ready when you are."}</h2>
          <p>{aiAccessPaused ? "Local projects are locked from MCP clients until you resume access." : "Open a project or use a local safety control."}</p>

          <div className="home-action-grid">
            <ActionCard
              icon="folder"
              title={adding ? "Opening..." : "Open project"}
              caption="Browse workspace"
              onClick={onAddProject}
              disabled={adding}
            />
            <ActionCard
              icon="checkpoint"
              title={checkpointBusy ? "Saving..." : "Create checkpoint"}
              caption="Save project state"
              onClick={onCreateCheckpoint}
              disabled={!hasProject || checkpointBusy}
            />
            <ActionCard
              icon="shield"
              title={safetyBusy ? "Scanning..." : "Safety scan"}
              caption="Check protection"
              onClick={onSafetyScan}
              disabled={!hasProject || safetyBusy}
            />
            <ActionCard
              icon={aiAccessPaused ? "resume" : "pause"}
              title={aiAccessBusy ? "Updating..." : aiAccessPaused ? "Resume AI access" : "Pause AI access"}
              caption={aiAccessPaused ? "Unlock MCP clients" : "Temporarily lock AI"}
              onClick={onToggleAiAccess}
              disabled={aiAccessBusy}
              active={aiAccessPaused}
            />
          </div>
        </div>

        <div className="home-security-line">
          Your local AI workspace <span>•</span> Private <span>•</span> {aiAccessPaused ? "AI access paused" : "Secure"}
        </div>
      </section>

      <aside className="home-right-rail">
        <section className="rail-card connection-card">
          <div className="rail-card-title"><NavIcon name="gateway" size={16} /><strong>Gateway</strong></div>
          <div className={`rail-status ${gateway.running ? "online" : "offline"}`}>
            <span className="rail-status-dot" />
            <strong>{gateway.running ? "Online" : "Offline"}</strong>
          </div>
          <small>{gateway.running && gateway.port ? `127.0.0.1:${gateway.port}` : "Local MCP endpoint"}</small>
          <button type="button" onClick={onToggleGateway} disabled={gatewayBusy}>
            <NavIcon name={gateway.running ? "stop" : "play"} size={16} />
            {gatewayBusy ? "Working..." : gateway.running ? "Stop gateway" : "Start gateway"}
          </button>
        </section>

        <section className="rail-card connection-card">
          <div className="rail-card-title"><NavIcon name="remote" size={16} /><strong>Remote</strong></div>
          <div className={`rail-status ${remoteReady ? "online" : "neutral"}`}>
            <span className="rail-status-dot" />
            <strong>{remoteTitle}</strong>
          </div>
          <small>{remoteDetail}</small>
          <button type="button" onClick={() => onNavigate("connections")}>
            <NavIcon name="link" size={16} />
            {remoteReady ? "Manage remote" : "Connect remote"}
          </button>
        </section>

        <section className="rail-card activity-card">
          <div className="rail-card-heading-row">
            <strong>Recent activity</strong>
            <button type="button" onClick={() => onNavigate("changes")}>View all</button>
          </div>
          {recent.length === 0 ? (
            <div className="rail-empty">No changes yet</div>
          ) : (
            <div className="rail-activity-list">
              {recent.map((change) => (
                <button key={change.id} type="button" className="rail-activity-row" onClick={() => onNavigate("changes")}>
                  <span className={`activity-dot ${change.status}`} />
                  <span className="rail-activity-copy">
                    <strong>{basename(change.primaryPath)}</strong>
                    <small>{change.workspaceName}</small>
                  </span>
                  <span className="rail-activity-status">{change.status}</span>
                  <time>{relativeTime(change.updatedAt)}</time>
                </button>
              ))}
            </div>
          )}
        </section>

        <section className="rail-card tips-card">
          <div className="rail-card-title"><NavIcon name="tip" size={16} /><strong>Tips</strong></div>
          <p>{aiAccessPaused ? "AI workspace access is paused. Click Resume AI access when you are ready." : workspaces.length === 0 ? "Add a project to begin." : "Create a checkpoint before large AI changes."}</p>
          <button type="button" onClick={() => onNavigate("projects")}>Learn more <span>→</span></button>
        </section>
      </aside>
    </div>
  );
}

export default HomeWorkspace;
