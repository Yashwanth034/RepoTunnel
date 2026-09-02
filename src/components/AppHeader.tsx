import type { AppView } from "./AppSidebar";
import { NavIcon } from "./AppSidebar";

type AppHeaderProps = {
  view: AppView;
  gatewayRunning: boolean;
  connectionReady: boolean;
  aiAccessPaused: boolean;
  focusMode: boolean;
  onToggleFocusMode: () => void;
};

const titles: Record<AppView, string> = {
  overview: "Home",
  projects: "Projects",
  team: "Team",
  models: "Model Hub",
  editor: "Editor",
  changes: "Review",
  checks: "Checks",
  git: "Git",
  connections: "Connect",
  commands: "Commands",
  system: "Settings",
  help: "Help",
};

const icons: Record<AppView, Parameters<typeof NavIcon>[0]["name"]> = {
  overview: "home",
  projects: "folder",
  team: "team",
  models: "model",
  editor: "folder",
  changes: "changes",
  checks: "checks",
  git: "git",
  connections: "link",
  commands: "terminal",
  system: "settings",
  help: "help",
};

function AppHeader({ view, gatewayRunning, connectionReady, aiAccessPaused, focusMode, onToggleFocusMode }: AppHeaderProps) {
  return (
    <header className="page-header">
      <div className="page-tab active">
        <NavIcon name={icons[view]} size={18} />
        <h1>{titles[view]}</h1>
      </div>
      <div className="page-statuses" aria-label="Connection status">
        <button
          type="button"
          className={`focus-mode-toggle ${focusMode ? "active" : ""}`}
          onClick={onToggleFocusMode}
          title="Toggle Focus Mode (Ctrl+Shift+Enter)"
        >
          <span aria-hidden="true">{focusMode ? "↙" : "↗"}</span>
          <span>{focusMode ? "Exit focus" : "Focus"}</span>
        </button>
        {aiAccessPaused ? (
          <span className="toolbar-paused" title="MCP project access is blocked until you resume AI access">
            <NavIcon name="pause" size={14} />
            <span>AI access paused</span>
          </span>
        ) : null}
        <span className="toolbar-status" title={gatewayRunning ? "Local MCP gateway online" : "Local MCP gateway offline"}>
          <NavIcon name="gateway" size={15} />
          <span>Gateway</span>
          <span className={`toolbar-dot ${gatewayRunning ? "online" : "offline"}`} />
        </span>
        <span className="toolbar-status" title={connectionReady ? "Remote connection active" : "No managed remote connection"}>
          <NavIcon name="remote" size={15} />
          <span>Remote</span>
          <span className={`toolbar-dot ${connectionReady ? "online" : ""}`} />
        </span>
      </div>
    </header>
  );
}

export default AppHeader;
