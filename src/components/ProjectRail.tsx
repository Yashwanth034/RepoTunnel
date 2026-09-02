import { useEffect, useMemo, useRef, useState } from "react";
import type { DirectoryEntry, GitFileChange, Workspace, WorkspaceHealth } from "../types";
import { NavIcon } from "./AppSidebar";
import ProjectExplorerTree from "./ProjectExplorerTree";

type ProjectRailProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  activeEditorPath: string | null;
  refreshToken: number;
  gitChanges: GitFileChange[];
  workspaceHealth: Record<string, WorkspaceHealth>;
  relocatingWorkspaceId: string | null;
  onSelectWorkspace: (workspaceId: string) => void;
  onRemoveWorkspace: (workspace: Workspace) => void;
  onOpenFile: (workspace: Workspace, entry: DirectoryEntry) => void;
  onEntryRemoved: (workspaceId: string, path: string, kind: DirectoryEntry["kind"]) => void;
  onEntryRenamed: (workspaceId: string, oldPath: string, newPath: string, kind: DirectoryEntry["kind"]) => void;
  onRelocateWorkspace: (workspace: Workspace) => void;
  onRetryWorkspace: (workspaceId: string) => void;
  onNotice: (message: string) => void;
};

const PINNED_KEY = "repotunnel.pinnedProjects";
const RECENT_KEY = "repotunnel.recentProjects";

function compactPath(path: string): string {
  return path.replace(/^\/home\/[^/]+/, "~");
}

function readIds(key: string): string[] {
  try {
    const value = JSON.parse(window.localStorage.getItem(key) ?? "[]");
    return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
  } catch {
    return [];
  }
}

function ProjectRail({
  workspaces,
  selectedWorkspaceId,
  activeEditorPath,
  refreshToken,
  gitChanges,
  workspaceHealth,
  relocatingWorkspaceId,
  onSelectWorkspace,
  onRemoveWorkspace,
  onOpenFile,
  onEntryRemoved,
  onEntryRenamed,
  onRelocateWorkspace,
  onRetryWorkspace,
  onNotice,
}: ProjectRailProps) {
  const [query, setQuery] = useState("");
  const searchRef = useRef<HTMLInputElement>(null);
  const [pinnedIds, setPinnedIds] = useState<string[]>(() => readIds(PINNED_KEY));
  const [recentIds, setRecentIds] = useState<string[]>(() => readIds(RECENT_KEY));
  const [collapsedProjectIds, setCollapsedProjectIds] = useState<Set<string>>(new Set());

  useEffect(() => {
    function focusSearch(event: KeyboardEvent) {
      const target = event.target as HTMLElement | null;
      if (event.key !== "/" || target?.matches("input, textarea, select, [contenteditable=true]")) return;
      event.preventDefault();
      searchRef.current?.focus();
    }
    window.addEventListener("keydown", focusSearch);
    return () => window.removeEventListener("keydown", focusSearch);
  }, []);

  useEffect(() => {
    if (!selectedWorkspaceId || !activeEditorPath) return;
    setCollapsedProjectIds((current) => {
      if (!current.has(selectedWorkspaceId)) return current;
      const next = new Set(current);
      next.delete(selectedWorkspaceId);
      return next;
    });
  }, [selectedWorkspaceId, activeEditorPath]);

  useEffect(() => {
    const existing = new Set(workspaces.map((workspace) => workspace.id));
    setPinnedIds((current) => {
      const next = current.filter((id) => existing.has(id));
      window.localStorage.setItem(PINNED_KEY, JSON.stringify(next));
      return next;
    });
    setRecentIds((current) => {
      const next = current.filter((id) => existing.has(id));
      window.localStorage.setItem(RECENT_KEY, JSON.stringify(next));
      return next;
    });
  }, [workspaces]);

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return workspaces;
    return workspaces.filter((workspace) =>
      `${workspace.name} ${workspace.path}`.toLowerCase().includes(needle),
    );
  }, [query, workspaces]);

  const pinnedSet = useMemo(() => new Set(pinnedIds), [pinnedIds]);
  const byId = useMemo(() => new Map(filtered.map((workspace) => [workspace.id, workspace])), [filtered]);

  const pinned = useMemo(
    () => pinnedIds.map((id) => byId.get(id)).filter((workspace): workspace is Workspace => Boolean(workspace)),
    [pinnedIds, byId],
  );

  const recent = useMemo(() => {
    const ordered: Workspace[] = [];
    const seen = new Set<string>();
    for (const id of recentIds) {
      const workspace = byId.get(id);
      if (workspace && !pinnedSet.has(id)) {
        ordered.push(workspace);
        seen.add(id);
      }
    }
    for (const workspace of [...filtered].sort((a, b) => b.addedAt - a.addedAt)) {
      if (!pinnedSet.has(workspace.id) && !seen.has(workspace.id)) ordered.push(workspace);
    }
    return ordered;
  }, [filtered, recentIds, pinnedSet, byId]);

  function selectProject(workspace: Workspace) {
    const nextRecent = [workspace.id, ...recentIds.filter((id) => id !== workspace.id)].slice(0, 20);
    setRecentIds(nextRecent);
    window.localStorage.setItem(RECENT_KEY, JSON.stringify(nextRecent));
    setCollapsedProjectIds((current) => {
      if (!current.has(workspace.id)) return current;
      const next = new Set(current);
      next.delete(workspace.id);
      return next;
    });
    onSelectWorkspace(workspace.id);
  }

  function toggleProjectTree(workspaceId: string) {
    setCollapsedProjectIds((current) => {
      const next = new Set(current);
      if (next.has(workspaceId)) next.delete(workspaceId);
      else next.add(workspaceId);
      return next;
    });
  }

  function togglePinned(workspace: Workspace) {
    const next = pinnedSet.has(workspace.id)
      ? pinnedIds.filter((id) => id !== workspace.id)
      : [workspace.id, ...pinnedIds.filter((id) => id !== workspace.id)];
    setPinnedIds(next);
    window.localStorage.setItem(PINNED_KEY, JSON.stringify(next));
  }

  function projectRow(workspace: Workspace) {
    const selected = workspace.id === selectedWorkspaceId;
    const pinnedProject = pinnedSet.has(workspace.id);
    const treeCollapsed = collapsedProjectIds.has(workspace.id);
    const health = workspaceHealth[workspace.id];
    const unavailable = health?.available === false;
    return (
      <div key={workspace.id} className={`project-rail-project ${selected ? "selected" : ""}`}>
        <div className={`project-rail-item ${selected ? "selected" : ""}`} title={workspace.path}>
          <button type="button" className="project-rail-main" onClick={() => selectProject(workspace)}>
            <span className="project-folder-icon"><NavIcon name="folder" size={19} /></span>
            <span className="project-rail-copy">
              <strong>{workspace.name}</strong>
              <small>{compactPath(workspace.path)}</small>
            </span>
            {selected ? <span className={`project-ready-dot ${unavailable ? "missing" : ""}`} title={unavailable ? "Project path unavailable" : "Selected project"} /> : null}
          </button>
          <div className="project-rail-actions">
            {selected ? (
              <button
                type="button"
                className="project-rail-action project-tree-toggle"
                onClick={() => toggleProjectTree(workspace.id)}
                aria-label={treeCollapsed ? `Expand files for ${workspace.name}` : `Collapse files for ${workspace.name}`}
                title={treeCollapsed ? "Expand project files" : "Collapse project files"}
              >
                {treeCollapsed ? "⌄" : "⌃"}
              </button>
            ) : null}
            <button
              type="button"
              className={`project-rail-action ${pinnedProject ? "pinned" : ""}`}
              onClick={() => togglePinned(workspace)}
              aria-label={pinnedProject ? `Unpin ${workspace.name}` : `Pin ${workspace.name}`}
              title={pinnedProject ? "Unpin project" : "Pin project"}
            >
              {pinnedProject ? "★" : "☆"}
            </button>
            <button
              type="button"
              className="project-rail-action remove"
              onClick={() => onRemoveWorkspace(workspace)}
              aria-label={`Remove ${workspace.name}`}
              title="Remove project"
            >
              ×
            </button>
          </div>
        </div>
        {selected && !treeCollapsed ? (
          unavailable ? (
            <div className="project-rail-recovery">
              <strong>Project path unavailable</strong>
              <span>{health?.message ?? "RepoTunnel cannot reach this project folder."}</span>
              <div>
                <button type="button" onClick={() => onRelocateWorkspace(workspace)} disabled={relocatingWorkspaceId === workspace.id}>
                  {relocatingWorkspaceId === workspace.id ? "Locating…" : "Locate again"}
                </button>
                <button type="button" onClick={() => onRetryWorkspace(workspace.id)}>Recheck</button>
              </div>
            </div>
          ) : (
            <ProjectExplorerTree
              workspace={workspace}
              activePath={activeEditorPath}
              refreshToken={refreshToken}
              gitChanges={gitChanges}
              onOpenFile={onOpenFile}
              onEntryRemoved={onEntryRemoved}
              onEntryRenamed={onEntryRenamed}
              onNotice={onNotice}
            />
          )
        ) : null}
      </div>
    );
  }

  return (
    <aside className="project-rail">
      <div className="project-rail-header">
        <strong>Projects</strong>
      </div>

      <div className="project-search">
        <NavIcon name="search" size={17} />
        <input
          ref={searchRef}
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search projects..."
          aria-label="Search projects"
        />
        <kbd>/</kbd>
      </div>

      <div className="project-rail-list">
        {pinned.length > 0 ? (
          <>
            <div className="project-group-label">Pinned</div>
            {pinned.map(projectRow)}
          </>
        ) : null}

        {recent.length > 0 ? (
          <>
            <div className="project-group-label">Recent projects</div>
            {recent.map(projectRow)}
          </>
        ) : null}

        {filtered.length === 0 ? (
          <div className="project-rail-empty">
            <span>{workspaces.length === 0 ? "No projects yet" : "No matching projects"}</span>
          </div>
        ) : null}
      </div>
    </aside>
  );
}

export default ProjectRail;
