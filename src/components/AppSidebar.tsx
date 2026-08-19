import RepoTunnelLogo from "./RepoTunnelLogo";

export type AppView =
  | "overview"
  | "projects"
  | "team"
  | "editor"
  | "changes"
  | "checks"
  | "git"
  | "connections"
  | "commands"
  | "system"
  | "help";

type AppSidebarProps = {
  activeView: AppView;
  pendingCount: number;
  onNavigate: (view: AppView) => void;
};

export type IconName =
  | "home"
  | "folder"
  | "team"
  | "changes"
  | "terminal"
  | "git"
  | "link"
  | "settings"
  | "plus"
  | "search"
  | "history"
  | "gateway"
  | "remote"
  | "play"
  | "stop"
  | "tip"
  | "help"
  | "bell"
  | "more"
  | "checks"
  | "checkpoint"
  | "shield"
  | "pause"
  | "resume";

const navItems: Array<{ id: AppView; label: string; icon: IconName }> = [
  { id: "overview", label: "Home", icon: "home" },
  { id: "projects", label: "Projects", icon: "folder" },
  { id: "team", label: "Team", icon: "team" },
  { id: "changes", label: "History", icon: "changes" },
  { id: "checks", label: "Checks", icon: "checks" },
  { id: "git", label: "Git", icon: "git" },
  { id: "connections", label: "Connect", icon: "link" },
  { id: "commands", label: "Commands", icon: "terminal" },
];

export function NavIcon({ name, size = 17 }: { name: IconName; size?: number }) {
  const common = {
    width: size,
    height: size,
    viewBox: "0 0 24 24",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 1.7,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    "aria-hidden": true,
  };

  if (name === "home") return <svg {...common}><path d="m4 10 8-7 8 7"/><path d="M6 9v11h12V9"/><path d="M10 20v-6h4v6"/></svg>;
  if (name === "folder") return <svg {...common}><path d="M3.5 7h6l1.8 2H20a1.5 1.5 0 0 1 1.5 1.5v7A2.5 2.5 0 0 1 19 20H5a2.5 2.5 0 0 1-2.5-2.5v-8A2.5 2.5 0 0 1 5 7Z"/></svg>;
  if (name === "team") return <svg {...common}><circle cx="8" cy="8" r="3"/><circle cx="16.5" cy="9" r="2.5"/><path d="M3.5 19c.5-3.7 2.3-5.5 5.2-5.5s4.7 1.8 5.2 5.5"/><path d="M13.7 14.3c.8-.8 1.8-1.2 3.1-1.2 2.3 0 3.7 1.5 4.1 4.5"/></svg>;
  if (name === "changes") return <svg {...common}><rect x="4" y="4" width="16" height="16" rx="3"/><path d="M8 9h8M8 13h5M8 17h7"/></svg>;
  if (name === "terminal") return <svg {...common}><path d="M4 7h16M4 12h16M4 17h16"/><circle cx="3" cy="7" r=".8" fill="currentColor" stroke="none"/><circle cx="3" cy="12" r=".8" fill="currentColor" stroke="none"/><circle cx="3" cy="17" r=".8" fill="currentColor" stroke="none"/></svg>;
  if (name === "checks") return <svg {...common}><circle cx="12" cy="12" r="8.5"/><path d="m8.5 12 2.3 2.3 4.8-5"/></svg>;
  if (name === "checkpoint") return <svg {...common}><path d="M5 5h14v14H5z"/><path d="M8 9h8M8 13h5"/><path d="m13.5 16 1.5 1.5 3-3"/></svg>;
  if (name === "shield") return <svg {...common}><path d="M12 3 19 6v5c0 4.6-2.7 8-7 10-4.3-2-7-5.4-7-10V6l7-3Z"/><path d="m9 12 2 2 4-4"/></svg>;
  if (name === "pause") return <svg {...common}><rect x="7" y="5" width="3.5" height="14" rx="1"/><rect x="13.5" y="5" width="3.5" height="14" rx="1"/></svg>;
  if (name === "resume") return <svg {...common}><path d="m8 5 11 7-11 7V5Z"/></svg>;

  if (name === "git") return <svg {...common}><circle cx="7" cy="5" r="2"/><circle cx="17" cy="8" r="2"/><circle cx="7" cy="19" r="2"/><path d="M7 7v10"/><path d="M9 5h2a6 6 0 0 1 6 6V10"/></svg>;
  if (name === "link") return <svg {...common}><path d="M9.5 14.5 14.5 9"/><path d="M7.2 16.8 5.8 18.2a3.5 3.5 0 1 1-5-5l3-3a3.5 3.5 0 0 1 5 0" transform="translate(2 0)"/><path d="m16.8 7.2 1.4-1.4a3.5 3.5 0 1 1 5 5l-3 3a3.5 3.5 0 0 1-5 0" transform="translate(-2 0)"/></svg>;
  if (name === "plus") return <svg {...common}><path d="M12 5v14"/><path d="M5 12h14"/></svg>;
  if (name === "search") return <svg {...common}><circle cx="10.5" cy="10.5" r="6.5"/><path d="m15.5 15.5 4 4"/></svg>;
  if (name === "history") return <svg {...common}><path d="M4 5v5h5"/><path d="M5.4 16.5a8 8 0 1 0-.8-9"/><path d="M12 8v4l3 2"/></svg>;
  if (name === "gateway") return <svg {...common}><path d="m12 3 7 4v10l-7 4-7-4V7l7-4Z"/><circle cx="12" cy="12" r="2.5"/></svg>;
  if (name === "remote") return <svg {...common}><path d="M8.5 15.5 15.5 8.5"/><path d="m7 17-1.5 1.5a3 3 0 0 1-4.2-4.2l3-3a3 3 0 0 1 4.2 0" transform="translate(3 -2)"/><path d="m17 7 1.5-1.5a3 3 0 1 1 4.2 4.2l-3 3a3 3 0 0 1-4.2 0" transform="translate(-3 2)"/></svg>;
  if (name === "play") return <svg {...common}><path d="m8 5 11 7-11 7V5Z"/></svg>;
  if (name === "stop") return <svg {...common}><rect x="7" y="7" width="10" height="10" rx="1.5"/></svg>;
  if (name === "tip") return <svg {...common}><path d="M9 18h6"/><path d="M10 21h4"/><path d="M8.5 15.5A7 7 0 1 1 15.5 15.5C14.5 16.3 14 17 14 18h-4c0-1-.5-1.7-1.5-2.5Z"/></svg>;
  if (name === "help") return <svg {...common}><circle cx="12" cy="12" r="9"/><path d="M9.8 9.5a2.4 2.4 0 1 1 4 1.8c-1 .8-1.8 1.2-1.8 2.7"/><path d="M12 17h.01"/></svg>;
  if (name === "bell") return <svg {...common}><path d="M18 9a6 6 0 0 0-12 0c0 7-3 7-3 7h18s-3 0-3-7"/><path d="M10 20h4"/></svg>;
  if (name === "more") return <svg {...common}><circle cx="5" cy="12" r="1" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1" fill="currentColor" stroke="none"/><circle cx="19" cy="12" r="1" fill="currentColor" stroke="none"/></svg>;
  return <svg {...common}><circle cx="12" cy="12" r="3"/><path d="M19 12a7 7 0 0 0-.1-1.2l2-1.5-2-3.4-2.5 1a7.5 7.5 0 0 0-2.1-1.2L14 3h-4l-.4 2.7a7.5 7.5 0 0 0-2.1 1.2l-2.5-1-2 3.4 2 1.5A7 7 0 0 0 5 12c0 .4 0 .8.1 1.2l-2 1.5 2 3.4 2.5-1a7.5 7.5 0 0 0 2.1 1.2L10 21h4l.4-2.7a7.5 7.5 0 0 0 2.1-1.2l2.5 1 2-3.4-2-1.5c0-.4.1-.8.1-1.2Z"/></svg>;
}

function AppSidebar({
  activeView,
  pendingCount,
  onNavigate,
}: AppSidebarProps) {
  return (
    <aside className="app-sidebar">
      <div className="sidebar-brand">
        <RepoTunnelLogo size={39} className="sidebar-product-logo" />
        <div className="sidebar-brand-copy">
          <strong>RepoTunnel</strong>
          <span>Local AI Workspace</span>
        </div>
      </div>

      <nav className="sidebar-nav" aria-label="RepoTunnel navigation">
        {navItems.map((item) => (
          <button
            key={item.id}
            type="button"
            className={`sidebar-nav-item ${activeView === item.id || (item.id === "projects" && activeView === "editor") ? "active" : ""}`}
            onClick={() => onNavigate(item.id)}
          >
            <NavIcon name={item.icon} />
            <span>{item.label}</span>
            {item.id === "changes" && pendingCount > 0 ? <span className="sidebar-count">{pendingCount}</span> : null}
          </button>
        ))}
      </nav>

      <div className="sidebar-spacer" />

      <button
        type="button"
        className={`sidebar-nav-item system-item ${activeView === "system" ? "active" : ""}`}
        onClick={() => onNavigate("system")}
      >
        <NavIcon name="settings" />
        <span>Settings</span>
      </button>
      <button
        type="button"
        className={`sidebar-nav-item system-item ${activeView === "help" ? "active" : ""}`}
        onClick={() => onNavigate("help")}
      >
        <NavIcon name="help" />
        <span>Help</span>
      </button>

    </aside>
  );
}

export default AppSidebar;
