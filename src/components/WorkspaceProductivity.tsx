import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { AppView } from "./AppSidebar";
import { inspectProject, runLocalTerminalCommand } from "../lib/backend";
import { searchFiles } from "../lib/filesystem";
import type { ProjectEntry, SearchMatch, Workspace } from "../types";

export type ProductivityMode = "quick" | "search" | "command";

type ActiveFile = {
  workspaceId: string;
  path: string;
  dirty: boolean;
} | null;

type WorkspaceProductivityProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  activeFile: ActiveFile;
  onOpenPath: (workspaceId: string, path: string, line?: number, column?: number) => void;
  onSaveActive: () => void;
  onOpenExternalActive: () => void;
  onRefreshProject: () => void;
  onNavigate: (view: AppView) => void;
  onNotice: (message: string) => void;
  focusMode: boolean;
  onToggleFocusMode: () => void;
};

type CommandItem = {
  id: string;
  label: string;
  detail: string;
  keywords: string;
  disabled?: boolean;
  run: () => void | Promise<void>;
};

function basename(path: string): string {
  return path.split("/").pop() ?? path;
}

function recentEditorPaths(workspaceId: string): string[] {
  try {
    const value = JSON.parse(window.localStorage.getItem(`repotunnel.editorRecent.${workspaceId}`) ?? "[]");
    return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string").slice(0, 40) : [];
  } catch {
    return [];
  }
}

function scorePath(path: string, query: string): number {
  const needle = query.trim().toLowerCase();
  if (!needle) return 1;
  const lower = path.toLowerCase();
  const name = basename(path).toLowerCase();
  const terms = needle.split(/\s+/).filter(Boolean);
  if (!terms.every((term) => lower.includes(term))) return -1;
  let score = 0;
  if (name === needle) score += 1000;
  else if (name.startsWith(needle)) score += 700;
  else if (name.includes(needle)) score += 500;
  if (lower.startsWith(needle)) score += 260;
  score += Math.max(0, 180 - lower.length);
  return score;
}

function shortcutLabel(mode: ProductivityMode): string {
  if (mode === "quick") return "Ctrl+P";
  if (mode === "search") return "Ctrl+Shift+F";
  return "Ctrl+Shift+P";
}

export default function WorkspaceProductivity({
  workspaces,
  selectedWorkspaceId,
  activeFile,
  onOpenPath,
  onSaveActive,
  onOpenExternalActive,
  onRefreshProject,
  onNavigate,
  onNotice,
  focusMode,
  onToggleFocusMode,
}: WorkspaceProductivityProps) {
  const [mode, setMode] = useState<ProductivityMode | null>(null);
  const [query, setQuery] = useState("");
  const [projectFiles, setProjectFiles] = useState<ProjectEntry[]>([]);
  const [searchResults, setSearchResults] = useState<SearchMatch[]>([]);
  const [loading, setLoading] = useState(false);
  const [selectedIndex, setSelectedIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const searchGeneration = useRef(0);

  const workspace = useMemo(
    () => workspaces.find((item) => item.id === (activeFile?.workspaceId ?? selectedWorkspaceId)) ?? null,
    [workspaces, selectedWorkspaceId, activeFile?.workspaceId],
  );

  const close = useCallback(() => {
    setMode(null);
    setQuery("");
    setSearchResults([]);
    setSelectedIndex(0);
  }, []);

  const openMode = useCallback((nextMode: ProductivityMode) => {
    if (!workspace && nextMode !== "command") {
      onNotice("Select an approved project first.");
      return;
    }
    setMode(nextMode);
    setQuery("");
    setSearchResults([]);
    setSelectedIndex(0);
  }, [workspace, onNotice]);

  useEffect(() => {
    function handleProductivityEvent(event: Event) {
      const detail = (event as CustomEvent<ProductivityMode>).detail;
      if (detail === "quick" || detail === "search" || detail === "command") openMode(detail);
    }
    window.addEventListener("repotunnel:productivity", handleProductivityEvent);
    return () => window.removeEventListener("repotunnel:productivity", handleProductivityEvent);
  }, [openMode]);

  useEffect(() => {
    function handleShortcut(event: KeyboardEvent) {
      if (!(event.ctrlKey || event.metaKey)) return;
      const key = event.key.toLowerCase();
      if (key === "p" && event.shiftKey) {
        event.preventDefault();
        openMode("command");
      } else if (key === "p") {
        event.preventDefault();
        openMode("quick");
      } else if (key === "f" && event.shiftKey) {
        event.preventDefault();
        openMode("search");
      }
    }
    window.addEventListener("keydown", handleShortcut, true);
    return () => window.removeEventListener("keydown", handleShortcut, true);
  }, [openMode]);

  useEffect(() => {
    if (!mode) return;
    const timer = window.setTimeout(() => inputRef.current?.focus(), 0);
    return () => window.clearTimeout(timer);
  }, [mode]);

  useEffect(() => {
    if (mode !== "quick" || !workspace) return;
    let cancelled = false;
    setLoading(true);
    inspectProject(workspace.id, 4000)
      .then((snapshot) => {
        if (cancelled) return;
        setProjectFiles(snapshot.entries.filter((entry) => entry.kind === "file"));
      })
      .catch((error) => {
        if (!cancelled) onNotice(`Quick Open: ${error instanceof Error ? error.message : String(error)}`);
      })
      .finally(() => { if (!cancelled) setLoading(false); });
    return () => { cancelled = true; };
  }, [mode, workspace?.id, onNotice]);

  useEffect(() => {
    if (mode !== "search" || !workspace) return;
    const needle = query.trim();
    const generation = ++searchGeneration.current;
    if (needle.length < 2) {
      setSearchResults([]);
      setLoading(false);
      return;
    }
    setLoading(true);
    const timer = window.setTimeout(() => {
      searchFiles(workspace.id, needle)
        .then((results) => {
          if (generation === searchGeneration.current) setSearchResults(results.slice(0, 200));
        })
        .catch((error) => {
          if (generation === searchGeneration.current) onNotice(`Project search: ${error instanceof Error ? error.message : String(error)}`);
        })
        .finally(() => {
          if (generation === searchGeneration.current) setLoading(false);
        });
    }, 220);
    return () => window.clearTimeout(timer);
  }, [mode, query, workspace?.id, onNotice]);

  const quickRecentPaths = useMemo(() => new Set(workspace ? recentEditorPaths(workspace.id) : []), [workspace?.id, mode]);

  const quickResults = useMemo(() => {
    if (mode !== "quick") return [];
    const recent = workspace ? recentEditorPaths(workspace.id) : [];
    const recentRank = new Map(recent.map((path, index) => [path, recent.length - index]));
    return projectFiles
      .map((entry) => ({ entry, score: scorePath(entry.path, query), recent: recentRank.get(entry.path) ?? 0 }))
      .filter((item) => item.score >= 0)
      .sort((left, right) => {
        if (!query.trim() && left.recent !== right.recent) return right.recent - left.recent;
        return right.score - left.score || left.entry.path.localeCompare(right.entry.path);
      })
      .slice(0, 80)
      .map((item) => item.entry);
  }, [mode, projectFiles, query, workspace?.id]);

  const commands = useMemo<CommandItem[]>(() => {
    const selectedWorkspace = workspace;
    const runCommand = async (command: string, label: string) => {
      if (!selectedWorkspace) {
        onNotice("Select an approved project first.");
        return;
      }
      close();
      try {
        const outcome = await runLocalTerminalCommand(selectedWorkspace.id, command, undefined, 900);
        onNotice(`${label}: ${outcome.command.status}${outcome.command.exitCode !== null ? ` · exit ${outcome.command.exitCode}` : ""}.`);
      } catch (error) {
        onNotice(`${label}: ${error instanceof Error ? error.message : String(error)}`);
      }
    };

    return [
      { id: "quick", label: "Quick Open File", detail: "Find a file by name or path", keywords: "file open ctrl p", run: () => openMode("quick") },
      { id: "search", label: "Search in Project", detail: "Search text across accessible project files", keywords: "global find search ctrl shift f", run: () => openMode("search") },
      { id: "save", label: "Save Active File", detail: activeFile?.dirty ? "Save current unsaved changes" : "Current file has no unsaved changes", keywords: "save ctrl s", disabled: !activeFile || !activeFile.dirty, run: () => { close(); onSaveActive(); } },
      { id: "external", label: "Open Active File Externally", detail: "Open with the system application", keywords: "external reveal open", disabled: !activeFile, run: () => { close(); onOpenExternalActive(); } },
      { id: "refresh", label: "Refresh Project", detail: "Refresh explorer and Git state", keywords: "refresh reload files git", disabled: !selectedWorkspace, run: () => { close(); onRefreshProject(); } },
      { id: "home", label: "Go to Home", detail: "Show the RepoTunnel dashboard", keywords: "dashboard home", run: () => { close(); onNavigate("overview"); } },
      { id: "projects", label: "Go to Projects", detail: "Manage approved projects", keywords: "projects workspace", run: () => { close(); onNavigate("projects"); } },
      { id: "team", label: "Go to Team", detail: "Open two-AI Team Mode coordination", keywords: "team multi ai agents collaborate tasks review", run: () => { close(); onNavigate("team"); } },
      { id: "history", label: "Go to History", detail: "Open Changes & History", keywords: "history changes restore checkpoint", run: () => { close(); onNavigate("changes"); } },
      { id: "commands", label: "Go to Commands", detail: "Open commands and automation controls", keywords: "terminal browser process commands", run: () => { close(); onNavigate("commands"); } },
      { id: "settings", label: "Go to Settings", detail: "Open RepoTunnel settings", keywords: "settings configuration", run: () => { close(); onNavigate("system"); } },
      { id: "focus", label: focusMode ? "Exit Focus Mode" : "Enter Focus Mode", detail: "Hide navigation and project rails for a distraction-free workspace · Ctrl+Shift+Enter", keywords: "focus zen fullscreen distraction workspace", run: () => { close(); onToggleFocusMode(); } },
      { id: "npm-check", label: "Run npm check", detail: "Run the project's type/check script", keywords: "npm typecheck verify", disabled: !selectedWorkspace, run: () => runCommand("npm run check", "npm check") },
      { id: "npm-build", label: "Run npm build", detail: "Build the selected project", keywords: "npm build compile", disabled: !selectedWorkspace, run: () => runCommand("npm run build", "npm build") },
      { id: "cargo-check", label: "Run cargo check", detail: "Check the selected Rust project", keywords: "rust cargo check compile", disabled: !selectedWorkspace, run: () => runCommand("cargo check", "cargo check") },
    ];
  }, [workspace, activeFile, close, onNotice, onSaveActive, onOpenExternalActive, onRefreshProject, onNavigate, openMode, focusMode, onToggleFocusMode]);

  const commandResults = useMemo(() => {
    if (mode !== "command") return [];
    const needle = query.trim().toLowerCase();
    if (!needle) return commands;
    return commands.filter((command) => `${command.label} ${command.detail} ${command.keywords}`.toLowerCase().includes(needle));
  }, [mode, query, commands]);

  useEffect(() => setSelectedIndex(0), [query, mode]);

  if (!mode) return null;

  const resultCount = mode === "quick" ? quickResults.length : mode === "search" ? searchResults.length : commandResults.length;

  function choose(index: number) {
    if (mode === "quick") {
      const entry = quickResults[index];
      if (!entry || !workspace) return;
      close();
      onOpenPath(workspace.id, entry.path);
      return;
    }
    if (mode === "search") {
      const result = searchResults[index];
      if (!result || !workspace) return;
      close();
      onOpenPath(workspace.id, result.path, result.line, result.column);
      return;
    }
    const command = commandResults[index];
    if (!command || command.disabled) return;
    void command.run();
  }

  return (
    <div className="productivity-overlay" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) close(); }}>
      <section className="productivity-panel" role="dialog" aria-modal="true" aria-label={mode === "quick" ? "Quick Open" : mode === "search" ? "Search in Project" : "Command Palette"}>
        <div className="productivity-input-row">
          <span className="productivity-mode-icon">{mode === "quick" ? "FILE" : mode === "search" ? "⌕" : ">_"}</span>
          <input
            ref={inputRef}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") { event.preventDefault(); close(); }
              if (event.key === "ArrowDown") { event.preventDefault(); setSelectedIndex((current) => Math.min(Math.max(0, resultCount - 1), current + 1)); }
              if (event.key === "ArrowUp") { event.preventDefault(); setSelectedIndex((current) => Math.max(0, current - 1)); }
              if (event.key === "Enter") { event.preventDefault(); choose(selectedIndex); }
            }}
            placeholder={mode === "quick" ? `Quick open in ${workspace?.name ?? "project"}…` : mode === "search" ? `Search ${workspace?.name ?? "project"}…` : "Type a command…"}
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
          />
          <kbd>{shortcutLabel(mode)}</kbd>
          <button type="button" onClick={close} aria-label="Close">×</button>
        </div>

        <div className="productivity-context">
          <span>{workspace ? workspace.name : "RepoTunnel"}</span>
          <span>{loading ? "Searching…" : `${resultCount} result${resultCount === 1 ? "" : "s"}`}</span>
        </div>

        <div className="productivity-results">
          {mode === "quick" ? quickResults.map((entry, index) => (
            <button type="button" key={entry.path} className={index === selectedIndex ? "selected" : ""} onMouseEnter={() => setSelectedIndex(index)} onClick={() => choose(index)}>
              <span className="productivity-file-badge">{entry.language?.slice(0, 3).toUpperCase() || "FILE"}</span>
              <span className="productivity-result-copy"><strong>{basename(entry.path)}</strong><small>{entry.path}</small></span>
              <span className="productivity-result-meta">{quickRecentPaths.has(entry.path) && !query.trim() ? "Recent" : entry.large ? "Large" : entry.binary ? "Binary" : entry.language ?? "file"}</span>
            </button>
          )) : null}

          {mode === "search" ? searchResults.map((result, index) => (
            <button type="button" key={`${result.path}:${result.line}:${result.column}:${index}`} className={index === selectedIndex ? "selected" : ""} onMouseEnter={() => setSelectedIndex(index)} onClick={() => choose(index)}>
              <span className="productivity-search-line">{result.line}</span>
              <span className="productivity-result-copy"><strong>{result.path}</strong><small>{result.preview}</small></span>
              <span className="productivity-result-meta">{result.line}:{result.column}</span>
            </button>
          )) : null}

          {mode === "command" ? commandResults.map((command, index) => (
            <button type="button" key={command.id} disabled={command.disabled} className={index === selectedIndex ? "selected" : ""} onMouseEnter={() => setSelectedIndex(index)} onClick={() => choose(index)}>
              <span className="productivity-command-mark">›</span>
              <span className="productivity-result-copy"><strong>{command.label}</strong><small>{command.detail}</small></span>
            </button>
          )) : null}

          {!loading && resultCount === 0 ? (
            <div className="productivity-empty">
              {mode === "search" && query.trim().length < 2 ? "Type at least two characters to search the project." : "No matching results."}
            </div>
          ) : null}
        </div>

        <footer className="productivity-footer"><span>↑↓ Navigate</span><span>Enter Open</span><span>Esc Close</span></footer>
      </section>
    </div>
  );
}
