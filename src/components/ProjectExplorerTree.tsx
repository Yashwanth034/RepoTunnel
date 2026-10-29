import { Fragment, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { DirectoryEntry, GitFileChange, Workspace } from "../types";
import {
  createEditorDirectory,
  createEditorFile,
  deleteEditorEntry,
  getFileInfo,
  listDirectory,
  listDirectoryPage,
  openWorkspacePathLocal,
  renameEditorEntry,
} from "../lib/filesystem";
import ConfirmationDialog from "./ConfirmationDialog";
import { NavIcon } from "./AppSidebar";
import { mapBounded } from "../lib/asyncWork";

type ProjectExplorerTreeProps = {
  workspace: Workspace;
  activePath: string | null;
  refreshToken: number;
  gitChanges: GitFileChange[];
  onOpenFile: (workspace: Workspace, entry: DirectoryEntry) => void;
  onEntryRemoved: (workspaceId: string, path: string, kind: DirectoryEntry["kind"]) => void;
  onEntryRenamed: (
    workspaceId: string,
    oldPath: string,
    newPath: string,
    kind: DirectoryEntry["kind"],
  ) => void;
  onNotice: (message: string) => void;
};

const EXPLORER_RENDER_PAGE_SIZE = 200;

type PromptMode = "file" | "folder" | "rename";

type PromptState = {
  mode: PromptMode;
  parentPath: string;
  target: DirectoryEntry | null;
  value: string;
};

type ContextMenuState = {
  workspaceId: string;
  entry: DirectoryEntry;
  x: number;
  y: number;
};

type DeleteTargetState = {
  workspaceId: string;
  entry: DirectoryEntry;
};

function parentPath(path: string): string {
  const index = path.lastIndexOf("/");
  return index < 0 ? "" : path.slice(0, index);
}

function joinPath(parent: string, name: string): string {
  return parent ? `${parent}/${name}` : name;
}

function fileGlyph(entry: DirectoryEntry): string {
  if (entry.kind === "directory") return "▸";
  const ext = entry.name.split(".").pop()?.toLowerCase() ?? "";
  if (["ts", "tsx", "js", "jsx", "mjs", "cjs"].includes(ext)) return "JS";
  if (ext === "py") return "PY";
  if (ext === "rs") return "RS";
  if (["html", "htm", "xml", "svg"].includes(ext)) return "<>";
  if (["css", "scss", "sass", "less"].includes(ext)) return "#";
  if (["json", "yaml", "yml", "toml"].includes(ext)) return "{}";
  if (["md", "mdx"].includes(ext)) return "MD";
  if (["png", "jpg", "jpeg", "gif", "webp", "bmp"].includes(ext)) return "IMG";
  return "·";
}

function sortEntries(entries: DirectoryEntry[]): DirectoryEntry[] {
  return [...entries].sort((left, right) => {
    if (left.kind === "directory" && right.kind !== "directory") return -1;
    if (left.kind !== "directory" && right.kind === "directory") return 1;
    return left.name.localeCompare(right.name, undefined, { sensitivity: "base" });
  });
}

function gitIndicator(entry: DirectoryEntry, changes: GitFileChange[]): { label: string; className: string; title: string } | null {
  const relevant = entry.kind === "directory"
    ? changes.filter((change) => change.path.startsWith(`${entry.path}/`))
    : changes.filter((change) => change.path === entry.path);
  if (relevant.length === 0) return null;
  if (relevant.some((change) => change.conflicted)) return { label: "!", className: "conflict", title: "Git conflict" };
  if (entry.kind === "directory") return { label: "•", className: "changed", title: `${relevant.length} changed item${relevant.length === 1 ? "" : "s"}` };
  if (relevant.some((change) => change.untracked)) return { label: "U", className: "untracked", title: "Untracked" };
  if (relevant.some((change) => change.staged && !change.unstaged)) return { label: "S", className: "staged", title: "Staged" };
  return { label: "M", className: "changed", title: "Modified" };
}

function ProjectExplorerTree({
  workspace,
  activePath,
  refreshToken,
  gitChanges,
  onOpenFile,
  onEntryRemoved,
  onEntryRenamed,
  onNotice,
}: ProjectExplorerTreeProps) {
  const [entriesByPath, setEntriesByPath] = useState<Record<string, DirectoryEntry[]>>({});
  // Keep very large folders navigable without mounting thousands of buttons at once.
  // All directory entries remain available for operations and refreshes.
  const [visibleWindows, setVisibleWindows] = useState<Record<string, { start: number; count: number }>>({});
  const [moreCursors, setMoreCursors] = useState<Record<string, string | null>>({});
  const pagedReadsRef = useRef(new Set<string>());
  const [expanded, setExpanded] = useState<Set<string>>(new Set([""]));
  const [loading, setLoading] = useState<Set<string>>(new Set());
  const [selectedEntry, setSelectedEntry] = useState<DirectoryEntry | null>(null);
  const [prompt, setPrompt] = useState<PromptState | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<DeleteTargetState | null>(null);
  const [busy, setBusy] = useState(false);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const activeWorkspaceRef = useRef(workspace.id);
  const workspaceGenerationRef = useRef(0);
  if (activeWorkspaceRef.current !== workspace.id) {
    activeWorkspaceRef.current = workspace.id;
    workspaceGenerationRef.current++;
  }
  const latestDirectoryReadRef = useRef(new Map<string, number>());
  const expandedRef = useRef(expanded);
  expandedRef.current = expanded;

  useEffect(() => {
    if (!contextMenu) return;
    const close = () => setContextMenu(null);
    const escape = (event: KeyboardEvent) => { if (event.key === "Escape") close(); };
    window.addEventListener("pointerdown", close);
    window.addEventListener("keydown", escape);
    return () => {
      window.removeEventListener("pointerdown", close);
      window.removeEventListener("keydown", escape);
    };
  }, [contextMenu]);

  const load = useCallback(async (path: string) => {
    const key = JSON.stringify([workspace.id, path]);
    const revision = (latestDirectoryReadRef.current.get(key) ?? 0) + 1;
    const generation = workspaceGenerationRef.current;
    latestDirectoryReadRef.current.set(key, revision);
    const currentRead = () => activeWorkspaceRef.current === workspace.id
      && workspaceGenerationRef.current === generation
      && latestDirectoryReadRef.current.get(key) === revision;
    setLoading((current) => new Set(current).add(path));
    try {
      let entries: DirectoryEntry[];
      let nextCursor: string | null = null;
      try {
        entries = await listDirectory(workspace.id, path);
      } catch (error) {
        // This legacy desktop API intentionally refuses folders with >1,000
        // items. Reuse the already-bounded Rust cursor reader instead of
        // silently showing an empty Explorer or lifting the backend limit.
        const message = error instanceof Error ? error.message : String(error);
        if (!message.includes("accessible entries. Narrow the request to a subfolder.")) throw error;
        const page = await listDirectoryPage(workspace.id, path, null, EXPLORER_RENDER_PAGE_SIZE);
        if (page.busy) throw new Error("Directory reader is busy; refresh this folder to retry.");
        entries = page.entries;
        nextCursor = page.nextCursor;
      }
      entries = sortEntries(entries);
      // A slower previous read or an earlier project must not replace a
      // newer directory listing after a refresh, rename, or project switch.
      if (currentRead()) {
        setEntriesByPath((current) => ({ ...current, [path]: entries }));
        setMoreCursors((current) => ({ ...current, [path]: nextCursor }));
        return entries;
      }
    } catch (error) {
      if (currentRead()) onNotice(`Project explorer: ${error instanceof Error ? error.message : String(error)}`);
    } finally {
      if (currentRead()) {
        setLoading((current) => {
          const next = new Set(current);
          next.delete(path);
          return next;
        });
      }
    }
  }, [workspace.id, onNotice]);

  const appendPage = useCallback(async (path: string) => {
    const cursor = moreCursors[path];
    if (!cursor) return;
    const generation = workspaceGenerationRef.current;
    const revisionKey = JSON.stringify([workspace.id, path]);
    const revision = latestDirectoryReadRef.current.get(revisionKey);
    const flightKey = JSON.stringify([workspace.id, path, generation]);
    if (pagedReadsRef.current.has(flightKey)) return;
    pagedReadsRef.current.add(flightKey);
    const stillCurrent = () => activeWorkspaceRef.current === workspace.id
      && workspaceGenerationRef.current === generation
      && latestDirectoryReadRef.current.get(revisionKey) === revision;
    setLoading((current) => new Set(current).add(path));
    try {
      const page = await listDirectoryPage(workspace.id, path, cursor, EXPLORER_RENDER_PAGE_SIZE);
      if (!stillCurrent()) return;
      if (page.busy) {
        onNotice("Project explorer: directory reader is busy. Try Show more again.");
        return;
      }
      setEntriesByPath((current) => {
        const existing = current[path] ?? [];
        const knownPaths = new Set(existing.map((entry) => entry.path));
        // An earlier page must not jump around when another batch arrives.
        // Keep loaded rows stable and sort only the newly appended page.
        return { ...current, [path]: [
          ...existing,
          ...sortEntries(page.entries.filter((entry) => !knownPaths.has(entry.path))),
        ] };
      });
      setMoreCursors((current) => ({ ...current, [path]: page.nextCursor }));
      setVisibleWindows((current) => ({
        ...current,
        [path]: {
          start: current[path]?.start ?? 0,
          count: (current[path]?.count ?? EXPLORER_RENDER_PAGE_SIZE) + EXPLORER_RENDER_PAGE_SIZE,
        },
      }));
    } catch (error) {
      if (stillCurrent()) {
        const message = error instanceof Error ? error.message : String(error);
        if (message.includes("Directory-page cursor is unknown or expired")) {
          // The backend intentionally expires inactive cursor sessions. A
          // previously opened large directory must recover without requiring
          // the user to retry a permanently invalid cursor.
          setMoreCursors((current) => ({ ...current, [path]: null }));
          setVisibleWindows((current) => {
            const next = { ...current };
            delete next[path];
            return next;
          });
          const refreshed = await load(path);
          if (activeWorkspaceRef.current === workspace.id
              && workspaceGenerationRef.current === generation && refreshed) {
            onNotice("Project explorer: directory page expired; refreshed the folder. Use Show more to continue.");
          }
        } else {
          onNotice(`Project explorer: ${message}`);
        }
      }
    } finally {
      pagedReadsRef.current.delete(flightKey);
      if (stillCurrent()) {
        setLoading((current) => {
          const next = new Set(current);
          next.delete(path);
          return next;
        });
      }
    }
  }, [workspace.id, moreCursors, onNotice]);

  useEffect(() => {
    setEntriesByPath({});
    setVisibleWindows({});
    setMoreCursors({});
    setExpanded(new Set([""]));
    setLoading(new Set());
    setSelectedEntry(null);
    setPrompt(null);
    setDeleteTarget(null);
    setContextMenu(null);
    setBusy(false);
    void load("");
  }, [workspace.id, load]);

  useEffect(() => {
    if (refreshToken === 0) return;
    // Directory expansion already reads the newly opened folder. Only a real
    // filesystem refresh token should reload *all* expanded directories.
    // Otherwise expanding N folders requests O(N²) directory listings.
    const visiblePaths = Array.from(expandedRef.current);
    void mapBounded(visiblePaths, load, 4);
  }, [refreshToken, load]);

  useEffect(() => {
    if (!activePath) return;
    const parent = parentPath(activePath);
    const segments = parent ? parent.split("/") : [];
    const paths: string[] = [];
    let current = "";
    for (const segment of segments) {
      current = current ? `${current}/${segment}` : segment;
      paths.push(current);
    }
    if (paths.length === 0) return;
    setExpanded((existing) => {
      const next = new Set(existing);
      next.add("");
      for (const path of paths) next.add(path);
      return next;
    });
    void mapBounded(paths, load, 4);
  }, [activePath, load]);

  async function toggleDirectory(entry: DirectoryEntry) {
    const isOpen = expanded.has(entry.path);
    if (!isOpen && !entriesByPath[entry.path]) await load(entry.path);
    setExpanded((current) => {
      const next = new Set(current);
      if (isOpen) next.delete(entry.path);
      else next.add(entry.path);
      return next;
    });
    setSelectedEntry(entry);
  }

  function creationParent(): string {
    if (!selectedEntry) return "";
    return selectedEntry.kind === "directory" ? selectedEntry.path : parentPath(selectedEntry.path);
  }

  function beginCreate(mode: "file" | "folder") {
    setPrompt({ mode, parentPath: creationParent(), target: null, value: "" });
  }

  function beginRename(entry: DirectoryEntry) {
    setSelectedEntry(entry);
    setPrompt({ mode: "rename", parentPath: parentPath(entry.path), target: entry, value: entry.name });
  }

  function validName(name: string): boolean {
    return Boolean(name && name !== "." && name !== ".." && !name.includes("/") && !name.includes("\\"));
  }

  async function submitPrompt() {
    if (!prompt || busy) return;
    const generation = workspaceGenerationRef.current;
    const stillCurrent = () => activeWorkspaceRef.current === workspace.id
      && workspaceGenerationRef.current === generation;
    const name = prompt.value.trim();
    if (!validName(name)) {
      onNotice("Project explorer: enter one valid file or folder name.");
      return;
    }

    setBusy(true);
    try {
      if (prompt.mode === "file") {
        const path = joinPath(prompt.parentPath, name);
        await createEditorFile(workspace.id, path, "");
        if (!stillCurrent()) return;
        const entries = await load(prompt.parentPath);
        if (!stillCurrent()) return;
        let entry = entries?.find((item) => item.path === path);
        if (!entry) {
          // A successful create can fall outside the first page of a large
          // folder, or the folder refresh can fail independently. Read only
          // the newly approved file rather than scanning unrelated entries.
          try {
            const info = await getFileInfo(workspace.id, path);
            if (!stillCurrent()) return;
            if (info.kind === "file") {
              entry = { name, path, kind: "file", size: info.size, modifiedAt: info.modifiedAt };
            }
          } catch {
            if (!stillCurrent()) return;
          }
        }
        if (entry) onOpenFile(workspace, entry);
        onNotice(entry ? `Created ${path}.` : `Created ${path}, but could not open it automatically.`);
      } else if (prompt.mode === "folder") {
        const path = joinPath(prompt.parentPath, name);
        await createEditorDirectory(workspace.id, path);
        if (!stillCurrent()) return;
        await load(prompt.parentPath);
        if (!stillCurrent()) return;
        setExpanded((current) => new Set(current).add(path));
        onNotice(`Created folder ${path}.`);
      } else if (prompt.target) {
        const oldPath = prompt.target.path;
        await renameEditorEntry(workspace.id, oldPath, name);
        if (!stillCurrent()) return;
        const newPath = joinPath(prompt.parentPath, name);
        await load(prompt.parentPath);
        if (!stillCurrent()) return;
        onEntryRenamed(workspace.id, oldPath, newPath, prompt.target.kind);
        onNotice(`Renamed ${oldPath} to ${newPath}.`);
      }
      if (stillCurrent()) setPrompt(null);
    } catch (error) {
      if (stillCurrent()) onNotice(`Project explorer: ${error instanceof Error ? error.message : String(error)}`);
    } finally {
      if (stillCurrent()) setBusy(false);
    }
  }

  async function confirmDelete() {
    if (!deleteTarget || deleteTarget.workspaceId !== workspace.id || busy) return;
    const generation = workspaceGenerationRef.current;
    const stillCurrent = () => activeWorkspaceRef.current === workspace.id
      && workspaceGenerationRef.current === generation;
    const target = deleteTarget.entry;
    setBusy(true);
    try {
      await deleteEditorEntry(workspace.id, target.path, target.kind === "directory");
      // Report a successful deletion to the project-scoped editor state even
      // if its original project is no longer selected.
      onEntryRemoved(workspace.id, target.path, target.kind);
      if (!stillCurrent()) return;
      await load(parentPath(target.path));
      if (!stillCurrent()) return;
      setSelectedEntry(null);
      setDeleteTarget(null);
      onNotice(`Deleted ${target.path}.`);
    } catch (error) {
      if (stillCurrent()) onNotice(`Project explorer: ${error instanceof Error ? error.message : String(error)}`);
    } finally {
      if (stillCurrent()) setBusy(false);
    }
  }

  async function refreshAll() {
    await mapBounded(Array.from(expanded), load, 4);
    onNotice(`Refreshed ${workspace.name}.`);
  }

  async function copyPath(entry: DirectoryEntry) {
    try {
      await navigator.clipboard.writeText(entry.path);
      onNotice(`Copied ${entry.path}.`);
    } catch {
      onNotice(`Relative path: ${entry.path}`);
    }
  }

  async function openExternal(entry: DirectoryEntry) {
    try {
      await openWorkspacePathLocal(workspace.id, entry.path);
      onNotice(`Opened ${entry.path}.`);
    } catch (error) {
      onNotice(`Could not open ${entry.path}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function openProjectRoot() {
    try {
      await openWorkspacePathLocal(workspace.id, "");
      onNotice(`Opened ${workspace.name}.`);
    } catch (error) {
      onNotice(`Could not open ${workspace.name}: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  function openEntryMenu(entry: DirectoryEntry, x: number, y: number) {
    setSelectedEntry(entry);
    setContextMenu({ workspaceId: workspace.id, entry, x, y });
  }

  const rootEntries = entriesByPath[""] ?? [];
  const empty = useMemo(
    () => rootEntries.length === 0 && !loading.has("") && !moreCursors[""],
    [rootEntries.length, loading, moreCursors],
  );

  function renderEntries(path: string, depth: number): ReactNode {
    const entries = entriesByPath[path] ?? [];
    // A restored/active editor tab must remain locatable even when it is
    // alphabetically beyond the initial page. Include its ancestor directory
    // as well, without eagerly rendering the rest of that large folder.
    const focusedPath = activePath ?? selectedEntry?.path;
    const focusedIndex = focusedPath
      ? entries.findIndex((entry) =>
        entry.path === focusedPath || (entry.kind === "directory" && focusedPath.startsWith(`${entry.path}/`)))
      : -1;
    // A tab restored far down a 20k-file folder should not force thousands
    // of earlier rows into the DOM just to keep that file visible.
    const window = visibleWindows[path];
    const start = window?.start ?? (focusedIndex >= EXPLORER_RENDER_PAGE_SIZE
      ? Math.floor(focusedIndex / EXPLORER_RENDER_PAGE_SIZE) * EXPLORER_RENDER_PAGE_SIZE
      : 0);
    const count = window?.count ?? EXPLORER_RENDER_PAGE_SIZE;
    const shown = Math.min(entries.length, start + count);
    return (
      <>
        {start > 0 ? (
          <button
            type="button"
            className="explorer-show-earlier"
            style={{ marginLeft: `${10 + depth * 13}px` }}
            onClick={() => setVisibleWindows((current) => ({
              ...current,
              [path]: {
                start: Math.max(0, start - EXPLORER_RENDER_PAGE_SIZE),
                count: count + Math.min(start, EXPLORER_RENDER_PAGE_SIZE),
              },
            }))}
          >
            Show {Math.min(EXPLORER_RENDER_PAGE_SIZE, start)} earlier ({start} earlier)
          </button>
        ) : null}
        {entries.slice(start, shown).map((entry) => {
      const isDirectory = entry.kind === "directory";
      const isOpen = isDirectory && expanded.has(entry.path);
      const selected = selectedEntry?.path === entry.path || activePath === entry.path;
      const git = gitIndicator(entry, gitChanges);
      return (
        <Fragment key={`${entry.kind}:${entry.path}`}>
          <div
            className={`explorer-row ${selected ? "selected" : ""}`}
            style={{ paddingLeft: `${7 + depth * 13}px` }}
            title={entry.path}
            onContextMenu={(event) => {
              event.preventDefault();
              openEntryMenu(entry, event.clientX, event.clientY);
            }}
          >
            <button
              type="button"
              className="explorer-entry-main"
              onClick={() => {
                setSelectedEntry(entry);
                if (isDirectory) void toggleDirectory(entry);
                else onOpenFile(workspace, entry);
              }}
            >
              <span className={`explorer-entry-icon ${isDirectory ? "folder" : "file"}`}>
                {isDirectory ? (isOpen ? "▾" : "▸") : fileGlyph(entry)}
              </span>
              <span className="explorer-entry-name">{entry.name}</span>
              {git ? <span className={`explorer-git-indicator ${git.className}`} title={git.title}>{git.label}</span> : null}
            </button>
            <div className="explorer-row-actions">
              <button
                type="button"
                title="File actions"
                aria-label={`Actions for ${entry.name}`}
                onClick={(event) => {
                  const rect = event.currentTarget.getBoundingClientRect();
                  openEntryMenu(entry, rect.right - 2, rect.bottom + 4);
                }}
              >⋯</button>
            </div>
          </div>
          {isDirectory && isOpen ? (
            <div className="explorer-children">
              {loading.has(entry.path) ? (
                <div className="explorer-inline-state" style={{ paddingLeft: `${24 + depth * 13}px` }}>Loading…</div>
              ) : null}
              {renderEntries(entry.path, depth + 1)}
              {!loading.has(entry.path) && !moreCursors[entry.path] && (entriesByPath[entry.path]?.length ?? 0) === 0 ? (
                <div className="explorer-inline-state" style={{ paddingLeft: `${24 + depth * 13}px` }}>Empty folder</div>
              ) : null}
            </div>
          ) : null}
        </Fragment>
      );
        })}
        {shown < entries.length || moreCursors[path] ? (
          <button
            type="button"
            className="explorer-load-more"
            style={{ marginLeft: `${10 + depth * 13}px` }}
            disabled={loading.has(path)}
            onClick={() => {
              if (shown < entries.length) {
                setVisibleWindows((current) => ({
                  ...current,
                  [path]: { start, count: count + EXPLORER_RENDER_PAGE_SIZE },
                }));
              } else {
                void appendPage(path);
              }
            }}
          >
            Show {shown < entries.length ? Math.min(EXPLORER_RENDER_PAGE_SIZE, entries.length - shown) : EXPLORER_RENDER_PAGE_SIZE} more
            {" "}({shown < entries.length ? `${entries.length - shown} remaining` : "more files available"})
          </button>
        ) : null}
      </>
    );
  }

  return (
    <div className="project-explorer-tree">
      <div className="explorer-toolbar">
        <button type="button" onClick={() => beginCreate("file")} disabled={workspace.accessMode !== "readWrite"}><span>+</span> File</button>
        <button type="button" onClick={() => beginCreate("folder")} disabled={workspace.accessMode !== "readWrite"}><span>+</span> Folder</button>
        <button type="button" className="icon-only push-right" onClick={() => void openProjectRoot()} title="Open project externally">↗</button>
        <button type="button" className="icon-only" onClick={() => void refreshAll()} title="Refresh tree"><NavIcon name="history" size={13} /></button>
      </div>

      {prompt ? (
        <div className="explorer-inline-prompt">
          <small>
            {prompt.mode === "rename" ? "Rename" : prompt.mode === "file" ? "New file" : "New folder"}
            {prompt.parentPath ? ` · ${prompt.parentPath}` : " · project root"}
          </small>
          <div>
            <input
              autoFocus
              value={prompt.value}
              onChange={(event) => setPrompt({ ...prompt, value: event.target.value })}
              onKeyDown={(event) => {
                if (event.key === "Enter") void submitPrompt();
                if (event.key === "Escape") setPrompt(null);
              }}
              placeholder={prompt.mode === "folder" ? "folder-name" : "file-name.ext"}
              disabled={busy}
            />
            <button type="button" onClick={() => void submitPrompt()} disabled={busy || !prompt.value.trim()}>✓</button>
            <button type="button" onClick={() => setPrompt(null)} disabled={busy}>×</button>
          </div>
        </div>
      ) : null}

      <div className="explorer-tree-scroll">
        {loading.has("") ? <div className="explorer-inline-state">Loading files…</div> : null}
        {empty ? <div className="explorer-inline-state">This project is empty.</div> : null}
        {renderEntries("", 0)}
      </div>

      {contextMenu?.workspaceId === workspace.id ? (
        <div
          className="explorer-context-menu"
          style={{ left: Math.min(contextMenu.x, window.innerWidth - 220), top: Math.min(contextMenu.y, window.innerHeight - 290) }}
          onPointerDown={(event) => event.stopPropagation()}
          role="menu"
          aria-label={`Actions for ${contextMenu.entry.name}`}
        >
          <div className="explorer-context-title">
            <strong>{contextMenu.entry.name}</strong>
            <span>{contextMenu.entry.kind === "directory" ? "Folder" : "File"}</span>
          </div>
          {contextMenu.entry.kind === "directory" && workspace.accessMode === "readWrite" ? (
            <>
              <button type="button" onClick={() => { setSelectedEntry(contextMenu.entry); setPrompt({ mode: "file", parentPath: contextMenu.entry.path, target: null, value: "" }); setContextMenu(null); }}><span>＋</span> New File</button>
              <button type="button" onClick={() => { setSelectedEntry(contextMenu.entry); setPrompt({ mode: "folder", parentPath: contextMenu.entry.path, target: null, value: "" }); setContextMenu(null); }}><span>＋</span> New Folder</button>
              <div className="explorer-context-separator" />
            </>
          ) : null}
          <button type="button" onClick={() => { void copyPath(contextMenu.entry); setContextMenu(null); }}><span>⧉</span> Copy Relative Path</button>
          <button type="button" onClick={() => { void openExternal(contextMenu.entry); setContextMenu(null); }}><span>↗</span> Open Externally</button>
          {workspace.accessMode === "readWrite" ? (
            <>
              <div className="explorer-context-separator" />
              <button type="button" onClick={() => { beginRename(contextMenu.entry); setContextMenu(null); }}><span>✎</span> Rename</button>
              <button type="button" className="danger" onClick={() => { setDeleteTarget({ workspaceId: workspace.id, entry: contextMenu.entry }); setContextMenu(null); }}><span>×</span> Delete</button>
            </>
          ) : null}
        </div>
      ) : null}

      {deleteTarget?.workspaceId === workspace.id ? (
        <ConfirmationDialog
          title={`Delete ${deleteTarget.entry.kind === "directory" ? "folder" : "file"}?`}
          message={`“${deleteTarget.entry.path}” will be deleted from the project. RepoTunnel will keep this manual action in version/recovery History.`}
          confirmLabel="Delete"
          busy={busy}
          onCancel={() => setDeleteTarget(null)}
          onConfirm={confirmDelete}
        />
      ) : null}
    </div>
  );
}

export default ProjectExplorerTree;
