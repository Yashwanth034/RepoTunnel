import { Fragment, useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import type { DirectoryEntry, GitFileChange, Workspace } from "../types";
import {
  createEditorDirectory,
  createEditorFile,
  deleteEditorEntry,
  listDirectory,
  openWorkspacePathLocal,
  renameEditorEntry,
} from "../lib/filesystem";
import ConfirmationDialog from "./ConfirmationDialog";
import { NavIcon } from "./AppSidebar";

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

type PromptMode = "file" | "folder" | "rename";

type PromptState = {
  mode: PromptMode;
  parentPath: string;
  target: DirectoryEntry | null;
  value: string;
};

type ContextMenuState = {
  entry: DirectoryEntry;
  x: number;
  y: number;
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
  const [expanded, setExpanded] = useState<Set<string>>(new Set([""]));
  const [loading, setLoading] = useState<Set<string>>(new Set());
  const [selectedEntry, setSelectedEntry] = useState<DirectoryEntry | null>(null);
  const [prompt, setPrompt] = useState<PromptState | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<DirectoryEntry | null>(null);
  const [busy, setBusy] = useState(false);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);

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
    setLoading((current) => new Set(current).add(path));
    try {
      const entries = sortEntries(await listDirectory(workspace.id, path));
      setEntriesByPath((current) => ({ ...current, [path]: entries }));
    } catch (error) {
      onNotice(`Project explorer: ${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setLoading((current) => {
        const next = new Set(current);
        next.delete(path);
        return next;
      });
    }
  }, [workspace.id, onNotice]);

  useEffect(() => {
    setEntriesByPath({});
    setExpanded(new Set([""]));
    setSelectedEntry(null);
    setPrompt(null);
    void load("");
  }, [workspace.id, load]);

  useEffect(() => {
    if (refreshToken === 0) return;
    const visiblePaths = Array.from(expanded);
    void Promise.all(visiblePaths.map((path) => load(path)));
  }, [refreshToken, expanded, load]);

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
    void Promise.all(paths.map((path) => load(path)));
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
    if (!prompt) return;
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
        const entries = sortEntries(await listDirectory(workspace.id, prompt.parentPath));
        setEntriesByPath((current) => ({ ...current, [prompt.parentPath]: entries }));
        const entry = entries.find((item) => item.path === path);
        if (entry) onOpenFile(workspace, entry);
        onNotice(`Created ${path}.`);
      } else if (prompt.mode === "folder") {
        const path = joinPath(prompt.parentPath, name);
        await createEditorDirectory(workspace.id, path);
        await load(prompt.parentPath);
        setExpanded((current) => new Set(current).add(path));
        onNotice(`Created folder ${path}.`);
      } else if (prompt.target) {
        const oldPath = prompt.target.path;
        await renameEditorEntry(workspace.id, oldPath, name);
        const newPath = joinPath(prompt.parentPath, name);
        await load(prompt.parentPath);
        onEntryRenamed(workspace.id, oldPath, newPath, prompt.target.kind);
        onNotice(`Renamed ${oldPath} to ${newPath}.`);
      }
      setPrompt(null);
    } catch (error) {
      onNotice(`Project explorer: ${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function confirmDelete() {
    if (!deleteTarget) return;
    setBusy(true);
    try {
      const target = deleteTarget;
      await deleteEditorEntry(workspace.id, target.path, target.kind === "directory");
      await load(parentPath(target.path));
      onEntryRemoved(workspace.id, target.path, target.kind);
      setSelectedEntry(null);
      setDeleteTarget(null);
      onNotice(`Deleted ${target.path}.`);
    } catch (error) {
      onNotice(`Project explorer: ${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function refreshAll() {
    await Promise.all(Array.from(expanded).map((path) => load(path)));
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
    setContextMenu({ entry, x, y });
  }

  const rootEntries = entriesByPath[""] ?? [];
  const empty = useMemo(
    () => rootEntries.length === 0 && !loading.has(""),
    [rootEntries.length, loading],
  );

  function renderEntries(path: string, depth: number): ReactNode {
    const entries = entriesByPath[path] ?? [];
    return entries.map((entry) => {
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
              {!loading.has(entry.path) && (entriesByPath[entry.path]?.length ?? 0) === 0 ? (
                <div className="explorer-inline-state" style={{ paddingLeft: `${24 + depth * 13}px` }}>Empty folder</div>
              ) : null}
            </div>
          ) : null}
        </Fragment>
      );
    });
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

      {contextMenu ? (
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
              <button type="button" className="danger" onClick={() => { setDeleteTarget(contextMenu.entry); setContextMenu(null); }}><span>×</span> Delete</button>
            </>
          ) : null}
        </div>
      ) : null}

      {deleteTarget ? (
        <ConfirmationDialog
          title={`Delete ${deleteTarget.kind === "directory" ? "folder" : "file"}?`}
          message={`“${deleteTarget.path}” will be deleted from the project. RepoTunnel will keep this manual action in version/recovery History.`}
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
