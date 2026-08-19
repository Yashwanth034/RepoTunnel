import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import type { ManagedProcessRecord, TerminalCommandRecord } from "../types";
import {
  inspectProject,
  listManagedProcesses,
  listTerminalHistory,
  readManagedProcessOutput,
  restartManagedProcess,
  runLocalTerminalCommand,
  startLocalManagedProcess,
  stopManagedProcess,
} from "../lib/backend";
import { listDirectory, readFile } from "../lib/filesystem";
import { NavIcon } from "./AppSidebar";

export type EditorProblem = {
  key: string;
  path: string;
  line: number;
  column: number;
  severity: "error" | "warning";
  message: string;
  source: string;
};

type QuickCheck = {
  label: string;
  command: string;
};

type DeveloperDockProps = {
  workspaceId: string;
  workspaceName: string;
  workspacePath: string;
  onOpenProblem: (path: string, line: number, column: number) => void;
  onProblemsChange?: (problems: EditorProblem[]) => void;
  onNotice: (message: string) => void;
};

type PersistedDockState = {
  collapsed: boolean;
  activeTab: "terminal" | "problems";
  height: number;
};

type TerminalSessionEntry = {
  id: string;
  command: string;
  stdout: string;
  stderr: string;
  status: "running" | "completed" | "failed" | "timedOut" | "process";
  exitCode: number | null;
  cwd: string;
};

function commandHistoryStorageKey(workspaceId: string): string {
  return `repotunnel.terminalCommandHistory.${workspaceId}`;
}

function readCommandHistory(workspaceId: string): string[] {
  try {
    const raw = window.localStorage.getItem(commandHistoryStorageKey(workspaceId));
    const parsed = raw ? JSON.parse(raw) : [];
    return Array.isArray(parsed) ? parsed.filter((value): value is string => typeof value === "string" && value.trim().length > 0).slice(-120) : [];
  } catch {
    return [];
  }
}

function shellDisplayPath(workspacePath: string): string {
  const normalized = workspacePath.replace(/\\/g, "/").replace(/\/$/, "");
  const homeMatch = normalized.match(/^\/home\/[^/]+(\/.*)?$/);
  if (homeMatch) return `~${homeMatch[1] ?? ""}`;
  return normalized || ".";
}

function shellUser(workspacePath: string): string {
  const normalized = workspacePath.replace(/\\/g, "/");
  return normalized.match(/^\/home\/([^/]+)/)?.[1] ?? "user";
}


function unquoteShellPath(value: string): string {
  const trimmed = value.trim();
  if (trimmed.length >= 2 && ((trimmed.startsWith('"') && trimmed.endsWith('"')) || (trimmed.startsWith("'") && trimmed.endsWith("'")))) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
}

function normalizeTerminalCwd(workspacePath: string, currentRelative: string, rawTarget: string): string | null {
  const root = workspacePath.replace(/\\/g, "/").replace(/\/$/, "");
  let target = unquoteShellPath(rawTarget).replace(/\\/g, "/");
  let absoluteWithinWorkspace = false;
  if (!target || target === "~") return "";
  if (target.startsWith("~/")) {
    const userHome = root.match(/^(\/home\/[^/]+)/)?.[1];
    if (!userHome) return null;
    target = `${userHome}/${target.slice(2)}`;
  }
  if (target.startsWith("/")) {
    if (target === root) return "";
    if (!target.startsWith(`${root}/`)) return null;
    target = target.slice(root.length + 1);
    absoluteWithinWorkspace = true;
  }
  const parts = absoluteWithinWorkspace ? [] : currentRelative.split("/").filter(Boolean);
  for (const part of target.split("/")) {
    if (!part || part === ".") continue;
    if (part === "..") {
      if (parts.length === 0) return null;
      parts.pop();
    } else {
      parts.push(part);
    }
  }
  return parts.join("/");
}

function terminalDisplayCwd(workspacePath: string, relative: string): string {
  const root = workspacePath.replace(/\\/g, "/").replace(/\/$/, "");
  return shellDisplayPath(relative ? `${root}/${relative}` : root);
}


function dockStorageKey(workspaceId: string): string {
  return `repotunnel.developerDock.${workspaceId}`;
}

function clampDockHeight(value: number): number {
  const viewportLimit = typeof window === "undefined" ? 520 : Math.max(220, Math.floor(window.innerHeight * 0.62));
  return Math.max(180, Math.min(viewportLimit, Number.isFinite(value) ? value : 245));
}

function readDockState(workspaceId: string): PersistedDockState {
  try {
    const raw = window.localStorage.getItem(dockStorageKey(workspaceId));
    if (!raw) return { collapsed: true, activeTab: "terminal", height: 245 };
    const parsed = JSON.parse(raw) as Partial<PersistedDockState>;
    return {
      collapsed: typeof parsed.collapsed === "boolean" ? parsed.collapsed : true,
      activeTab: parsed.activeTab === "problems" ? "problems" : "terminal",
      height: clampDockHeight(typeof parsed.height === "number" ? parsed.height : 245),
    };
  } catch {
    return { collapsed: true, activeTab: "terminal", height: 245 };
  }
}

function compactCommand(command: string): string {
  const first = command.trim().split("\n")[0] ?? command;
  return first.length > 90 ? `${first.slice(0, 87)}…` : first;
}

function normalizeProblemPath(raw: string, workspacePath: string): string | null {
  let path = raw.trim().replace(/^['"]|['"]$/g, "").replace(/\\/g, "/");
  const root = workspacePath.replace(/\\/g, "/").replace(/\/$/, "");
  if (path.startsWith(`${root}/`)) path = path.slice(root.length + 1);
  path = path.replace(/^\.\//, "");
  if (!path || path.startsWith("../") || path.startsWith("/")) return null;
  return path;
}

function stripAnsi(value: string): string {
  return value.replace(/\u001B\[[0-?]*[ -/]*[@-~]/g, "");
}

export function parseProblems(records: TerminalCommandRecord[], workspacePath: string): EditorProblem[] {
  const problems: EditorProblem[] = [];
  const seen = new Set<string>();

  function add(rawPath: string, lineRaw: string, columnRaw: string | undefined, severityRaw: string | undefined, message: string, source: string) {
    const path = normalizeProblemPath(rawPath, workspacePath);
    if (!path) return;
    const line = Math.max(1, Number(lineRaw) || 1);
    const column = Math.max(1, Number(columnRaw) || 1);
    const severity = severityRaw?.toLowerCase().includes("warning") ? "warning" : "error";
    const text = message.trim() || "Problem reported by command output";
    const key = `${path}:${line}:${column}:${severity}:${text}`;
    if (seen.has(key)) return;
    seen.add(key);
    problems.push({ key, path, line, column, severity, message: text, source });
  }

  for (const record of records) {
    const source = compactCommand(record.command);
    const text = stripAnsi(`${record.stdout}\n${record.stderr}`);
    const lines = text.split(/\r?\n/);
    let eslintPath: string | null = null;
    for (let index = 0; index < lines.length; index += 1) {
      const line = lines[index];
      const trimmed = line.trim();
      if (/^(?:\.?\/?|[A-Za-z]:[\\/]).+\.[A-Za-z0-9]+$/.test(trimmed) && !/\s/.test(trimmed)) {
        eslintPath = trimmed;
      }
      let match = line.match(/^(.+?)\((\d+),(\d+)\):\s*(error|warning)\b[:\s]*(.*)$/i);
      if (match) {
        add(match[1], match[2], match[3], match[4], match[5], source);
        continue;
      }
      match = line.match(/^(.+?):(\d+):(\d+)(?:\s*:\s*|\s+-\s+)(error|warning)\b[:\s]*(.*)$/i);
      if (match) {
        add(match[1], match[2], match[3], match[4], match[5], source);
        continue;
      }
      match = line.match(/^\s*-->\s+(.+?):(\d+):(\d+)/);
      if (match) {
        const previous = lines.slice(Math.max(0, index - 4), index).reverse().find((candidate) => /^(?:error|warning)(?:\[[^\]]+\])?:/i.test(candidate.trim()));
        const diagnostic = previous ?? "Rust compiler diagnostic";
        add(match[1], match[2], match[3], /warning/i.test(diagnostic) ? "warning" : "error", diagnostic.trim(), source);
        continue;
      }
      match = line.match(/^\s*(\d+):(\d+)\s+(error|warning)\s+(.*)$/i);
      if (match && eslintPath) {
        add(eslintPath, match[1], match[2], match[3], match[4], source);
        continue;
      }
      match = line.match(/^\s*File\s+"(.+?)",\s+line\s+(\d+)/);
      if (match) {
        const next = lines[index + 1] ?? "Python traceback";
        add(match[1], match[2], "1", "error", next.trim() || "Python traceback", source);
      }
    }
  }

  return problems.slice(0, 200);
}

function statusClass(status: string): string {
  if (["completed", "exited", "stopped"].includes(status)) return "success";
  if (["failed", "timedOut", "rejected"].includes(status)) return "danger";
  if (["running", "pending"].includes(status)) return "active";
  return "";
}

export default function DeveloperDock({
  workspaceId,
  workspaceName,
  workspacePath,
  onOpenProblem,
  onProblemsChange,
  onNotice,
}: DeveloperDockProps) {
  const initialDockState = readDockState(workspaceId);
  const [collapsed, setCollapsed] = useState(() => initialDockState.collapsed);
  const [activeTab, setActiveTab] = useState<"terminal" | "problems">(() => initialDockState.activeTab);
  const [dockHeight, setDockHeight] = useState(() => initialDockState.height);
  const [command, setCommand] = useState("");
  const [busy, setBusy] = useState(false);
  const [records, setRecords] = useState<TerminalCommandRecord[]>([]);
  const [processes, setProcesses] = useState<ManagedProcessRecord[]>([]);
  const [processOutput, setProcessOutput] = useState<Record<string, string>>({});
  const [quickChecks, setQuickChecks] = useState<QuickCheck[]>([]);
  const [sessionEntries, setSessionEntries] = useState<TerminalSessionEntry[]>([]);
  const [commandHistory, setCommandHistory] = useState<string[]>(() => readCommandHistory(workspaceId));
  const [historyCursor, setHistoryCursor] = useState<number | null>(null);
  const [showProcesses, setShowProcesses] = useState(false);
  const [cwdRelative, setCwdRelative] = useState("");
  const [dismissedProblemKeys, setDismissedProblemKeys] = useState<Set<string>>(new Set());
  const [previousCwdRelative, setPreviousCwdRelative] = useState<string | null>(null);
  const [terminalSearchOpen, setTerminalSearchOpen] = useState(false);
  const [terminalSearchQuery, setTerminalSearchQuery] = useState("");
  const [terminalSearchIndex, setTerminalSearchIndex] = useState(0);
  const terminalScreenRef = useRef<HTMLDivElement | null>(null);
  const terminalInputRef = useRef<HTMLInputElement | null>(null);
  const terminalUser = useMemo(() => shellUser(workspacePath), [workspacePath]);
  const terminalCwd = useMemo(() => terminalDisplayCwd(workspacePath, cwdRelative), [workspacePath, cwdRelative]);
  const runningProcessCount = processes.filter((process) => process.status === "running").length;
  const recordIdentity = useMemo(() => records.map((record) => record.id).join("|"), [records]);
  const previousRecordIdentityRef = useRef("");

  useEffect(() => {
    if (previousRecordIdentityRef.current && previousRecordIdentityRef.current !== recordIdentity) setDismissedProblemKeys(new Set());
    previousRecordIdentityRef.current = recordIdentity;
  }, [recordIdentity]);

  useEffect(() => {
    const state = readDockState(workspaceId);
    setCollapsed(state.collapsed);
    setActiveTab(state.activeTab);
    setDockHeight(state.height);
    setCommand("");
    setRecords([]);
    setProcesses([]);
    setProcessOutput({});
    setSessionEntries([]);
    setCommandHistory(readCommandHistory(workspaceId));
    setHistoryCursor(null);
    setShowProcesses(false);
    setCwdRelative("");
    setPreviousCwdRelative(null);
    setDismissedProblemKeys(new Set());
    setTerminalSearchOpen(false);
    setTerminalSearchQuery("");
    setTerminalSearchIndex(0);
  }, [workspaceId]);

  useEffect(() => {
    try {
      window.localStorage.setItem(dockStorageKey(workspaceId), JSON.stringify({ collapsed, activeTab, height: dockHeight } satisfies PersistedDockState));
    } catch {
      // Dock layout persistence is best-effort only.
    }
  }, [workspaceId, collapsed, activeTab, dockHeight]);

  useEffect(() => {
    const fitToViewport = () => setDockHeight((current) => clampDockHeight(current));
    window.addEventListener("resize", fitToViewport);
    return () => window.removeEventListener("resize", fitToViewport);
  }, []);

  useEffect(() => {
    const element = terminalScreenRef.current;
    if (!element) return;
    element.scrollTop = element.scrollHeight;
  }, [sessionEntries, busy]);

  const refresh = useCallback(async () => {
    try {
      const [nextRecords, nextProcesses] = await Promise.all([
        listTerminalHistory(workspaceId, 30),
        listManagedProcesses(workspaceId, 30),
      ]);
      setRecords(nextRecords);
      setProcesses(nextProcesses);
    } catch (error) {
      onNotice(`Developer terminal: ${error instanceof Error ? error.message : String(error)}`);
    }
  }, [workspaceId, onNotice]);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 2500);
    return () => window.clearInterval(timer);
  }, [workspaceId, refresh]);

  useEffect(() => {
    let cancelled = false;
    async function discoverChecks() {
      try {
        const snapshot = await inspectProject(workspaceId, 120);
        const manifests = new Set(snapshot.overview.manifests.map((item) => item.toLowerCase()));
        const checks: QuickCheck[] = [];
        if (manifests.has("package.json")) {
          try {
            const pkg = JSON.parse((await readFile(workspaceId, "package.json")).content) as { scripts?: Record<string, string> };
            for (const script of ["check", "lint", "test", "build"]) {
              if (pkg.scripts?.[script]) checks.push({ label: `npm ${script}`, command: `npm run ${script}` });
            }
          } catch {
            // Invalid package metadata simply means no package quick checks are offered.
          }
        }
        if (manifests.has("cargo.toml")) checks.push({ label: "cargo check", command: "cargo check" });
        if (manifests.has("pyproject.toml") || manifests.has("requirements.txt")) {
          checks.push({ label: "python compile", command: "python3 -m compileall ." });
        }
        if (manifests.has("go.mod")) checks.push({ label: "go test", command: "go test ./..." });
        if (!cancelled) setQuickChecks(checks.slice(0, 6));
      } catch {
        if (!cancelled) setQuickChecks([]);
      }
    }
    void discoverChecks();
    return () => { cancelled = true; };
  }, [workspaceId]);

  const parsedProblems = useMemo(() => parseProblems(records, workspacePath), [records, workspacePath]);
  const problems = useMemo(
    () => parsedProblems.filter((problem) => !dismissedProblemKeys.has(problem.key)),
    [parsedProblems, dismissedProblemKeys],
  );
  const errorCount = problems.filter((problem) => problem.severity === "error").length;
  const warningCount = problems.length - errorCount;
  const groupedProblems = useMemo(() => {
    const groups = new Map<string, EditorProblem[]>();
    for (const problem of problems) {
      const current = groups.get(problem.path) ?? [];
      current.push(problem);
      groups.set(problem.path, current);
    }
    return [...groups.entries()];
  }, [problems]);

  useEffect(() => {
    onProblemsChange?.(problems);
    return () => onProblemsChange?.([]);
  }, [problems, onProblemsChange]);

  function clearProblems() {
    setDismissedProblemKeys((current) => new Set([...current, ...parsedProblems.map((problem) => problem.key)]));
  }

  function rememberCommand(next: string) {
    setCommandHistory((current) => {
      const withoutDuplicate = current.filter((item) => item !== next);
      const updated = [...withoutDuplicate, next].slice(-120);
      try {
        window.localStorage.setItem(commandHistoryStorageKey(workspaceId), JSON.stringify(updated));
      } catch {
        // Shell command history persistence is best-effort only.
      }
      return updated;
    });
    setHistoryCursor(null);
  }

  function clearTerminal() {
    setSessionEntries([]);
    setCommand("");
    setHistoryCursor(null);
    window.requestAnimationFrame(() => terminalInputRef.current?.focus());
  }

  async function changeDirectory(commandText: string, rawTarget: string) {
    const target = rawTarget.trim() === "-" ? previousCwdRelative : normalizeTerminalCwd(workspacePath, cwdRelative, rawTarget);
    const entryId = `local-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    const entryCwd = terminalCwd;
    if (target === null) {
      setSessionEntries((current) => [...current, {
        id: entryId,
        command: commandText,
        stdout: "",
        stderr: "cd: RepoTunnel keeps the persistent working directory inside the approved project.",
        status: "failed",
        exitCode: 1,
        cwd: entryCwd,
      }]);
      return;
    }
    try {
      await listDirectory(workspaceId, target);
      const oldCwd = cwdRelative;
      setPreviousCwdRelative(oldCwd);
      setCwdRelative(target);
      setSessionEntries((current) => [...current, {
        id: entryId,
        command: commandText,
        stdout: rawTarget.trim() === "-" ? terminalDisplayCwd(workspacePath, target) : "",
        stderr: "",
        status: "completed",
        exitCode: 0,
        cwd: entryCwd,
      }]);
    } catch (error) {
      setSessionEntries((current) => [...current, {
        id: entryId,
        command: commandText,
        stdout: "",
        stderr: `cd: ${error instanceof Error ? error.message : String(error)}`,
        status: "failed",
        exitCode: 1,
        cwd: entryCwd,
      }]);
    }
  }

  async function run(commandText: string, asProcess = false) {
    const next = commandText.trim();
    if (!next || busy) return;
    if (!asProcess && (next === "clear" || next === "cls")) {
      rememberCommand(next);
      clearTerminal();
      return;
    }
    const cdMatch = !asProcess ? next.match(/^cd(?:\s+(.*))?$/) : null;
    if (cdMatch && !/[;&|`$]/.test(cdMatch[1] ?? "")) {
      rememberCommand(next);
      setCommand("");
      await changeDirectory(next, cdMatch[1] ?? "");
      window.requestAnimationFrame(() => terminalInputRef.current?.focus());
      return;
    }

    const entryId = `local-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    rememberCommand(next);
    setCommand("");
    setBusy(true);
    setSessionEntries((current) => [
      ...current,
      { id: entryId, command: next, stdout: "", stderr: "", status: "running", exitCode: null, cwd: terminalCwd },
    ]);

    try {
      if (asProcess) {
        const outcome = await startLocalManagedProcess(workspaceId, next, cwdRelative || undefined, compactCommand(next));
        const pidCopy = outcome.process.pid ? ` · pid ${outcome.process.pid}` : "";
        const processStarted = outcome.process.status === "running";
        setSessionEntries((current) => current.map((entry) => entry.id === entryId ? {
          ...entry,
          status: processStarted ? "process" : "failed",
          stdout: processStarted ? `Started managed process: ${outcome.process.label}${pidCopy}\nUse Processes to inspect, stop, or restart it.` : "",
          stderr: processStarted ? "" : (outcome.process.error || `Process ${outcome.process.status}.`),
          exitCode: processStarted ? null : (outcome.process.exitCode ?? 1),
        } : entry));
        onNotice(processStarted ? `Started ${outcome.process.label}.` : `Process ${outcome.process.status}.`);
      } else {
        const outcome = await runLocalTerminalCommand(workspaceId, next, cwdRelative || undefined, 600);
        const status = outcome.command.status === "completed"
          ? "completed"
          : outcome.command.status === "timedOut"
            ? "timedOut"
            : "failed";
        setSessionEntries((current) => current.map((entry) => entry.id === entryId ? {
          ...entry,
          stdout: stripAnsi(outcome.command.stdout),
          stderr: stripAnsi(outcome.command.stderr || outcome.command.error || ""),
          status,
          exitCode: outcome.command.exitCode,
        } : entry));
      }
      await refresh();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setSessionEntries((current) => current.map((entry) => entry.id === entryId ? {
        ...entry,
        status: "failed",
        stderr: message,
      } : entry));
      onNotice(`Developer terminal: ${message}`);
    } finally {
      setBusy(false);
      window.requestAnimationFrame(() => terminalInputRef.current?.focus());
    }
  }

  function navigateCommandHistory(direction: -1 | 1) {
    if (commandHistory.length === 0) return;
    let nextIndex: number;
    if (historyCursor === null) {
      if (direction > 0) return;
      nextIndex = commandHistory.length - 1;
    } else {
      nextIndex = Math.min(commandHistory.length, Math.max(0, historyCursor + direction));
    }
    if (nextIndex >= commandHistory.length) {
      setHistoryCursor(null);
      setCommand("");
      return;
    }
    setHistoryCursor(nextIndex);
    setCommand(commandHistory[nextIndex] ?? "");
  }

  const terminalSearchCount = useMemo(() => {
    const query = terminalSearchQuery.trim().toLowerCase();
    if (!query) return 0;
    let total = 0;
    for (const entry of sessionEntries) {
      const haystack = `${entry.command}\n${entry.stdout}\n${entry.stderr}`.toLowerCase();
      let cursor = 0;
      while (cursor <= haystack.length - query.length) {
        const found = haystack.indexOf(query, cursor);
        if (found < 0) break;
        total += 1;
        cursor = found + Math.max(1, query.length);
      }
    }
    return total;
  }, [sessionEntries, terminalSearchQuery]);

  useEffect(() => {
    if (terminalSearchCount === 0) setTerminalSearchIndex(0);
    else setTerminalSearchIndex((current) => Math.min(current, terminalSearchCount - 1));
  }, [terminalSearchCount]);

  useEffect(() => {
    if (!terminalSearchOpen || terminalSearchCount === 0) return;
    const timer = window.requestAnimationFrame(() => {
      const hits = terminalScreenRef.current?.querySelectorAll<HTMLElement>(".terminal-search-hit") ?? [];
      hits[terminalSearchIndex]?.scrollIntoView({ block: "center" });
    });
    return () => window.cancelAnimationFrame(timer);
  }, [terminalSearchOpen, terminalSearchIndex, terminalSearchCount, terminalSearchQuery]);

  function stepTerminalSearch(direction: -1 | 1) {
    if (terminalSearchCount === 0) return;
    setTerminalSearchIndex((current) => (current + direction + terminalSearchCount) % terminalSearchCount);
  }

  let terminalRenderMatchIndex = 0;
  function renderTerminalText(value: string): ReactNode {
    const query = terminalSearchOpen ? terminalSearchQuery.trim() : "";
    if (!query || !value) return value;
    const lower = value.toLowerCase();
    const needle = query.toLowerCase();
    const nodes: ReactNode[] = [];
    let cursor = 0;
    while (cursor < value.length) {
      const found = lower.indexOf(needle, cursor);
      if (found < 0) {
        nodes.push(value.slice(cursor));
        break;
      }
      if (found > cursor) nodes.push(value.slice(cursor, found));
      const matchIndex = terminalRenderMatchIndex++;
      nodes.push(
        <mark className={`terminal-search-hit ${matchIndex === terminalSearchIndex ? "current" : ""}`} key={`${matchIndex}-${found}`}>
          {value.slice(found, found + query.length)}
        </mark>,
      );
      cursor = found + query.length;
    }
    return nodes;
  }

  async function inspectProcess(process: ManagedProcessRecord) {
    try {
      const output = await readManagedProcessOutput(process.id, 0, 0, 64 * 1024);
      setProcessOutput((current) => ({ ...current, [process.id]: `${stripAnsi(output.stdout)}${output.stderr ? `${output.stdout ? "\n" : ""}${stripAnsi(output.stderr)}` : ""}` }));
    } catch (error) {
      onNotice(`Process output: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function stopProcess(process: ManagedProcessRecord) {
    try {
      await stopManagedProcess(process.id, false);
      await refresh();
    } catch (error) {
      onNotice(`Stop process: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function restartProcess(process: ManagedProcessRecord) {
    try {
      await restartManagedProcess(process.id);
      await refresh();
    } catch (error) {
      onNotice(`Restart process: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  function resizeDock(next: number) {
    setDockHeight(clampDockHeight(next));
  }

  function beginDockResize(event: ReactPointerEvent<HTMLDivElement>) {
    if (collapsed) return;
    event.preventDefault();
    const startY = event.clientY;
    const startHeight = dockHeight;
    const pointerId = event.pointerId;
    event.currentTarget.setPointerCapture?.(pointerId);

    const move = (moveEvent: PointerEvent) => resizeDock(startHeight + (startY - moveEvent.clientY));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up, { once: true });
  }

  const dockStyle = collapsed
    ? undefined
    : ({ flexBasis: `${dockHeight}px`, minHeight: `${dockHeight}px`, maxHeight: `${dockHeight}px` } as CSSProperties);

  return (
    <section className={`developer-dock ${collapsed ? "collapsed" : ""}`} style={dockStyle}>
      {!collapsed ? (
        <div
          className="developer-dock-resizer"
          role="separator"
          aria-label="Resize terminal and problems panel"
          aria-orientation="horizontal"
          tabIndex={0}
          onPointerDown={beginDockResize}
          onDoubleClick={() => resizeDock(245)}
          onKeyDown={(event) => {
            if (event.key === "ArrowUp") { event.preventDefault(); resizeDock(dockHeight + 20); }
            if (event.key === "ArrowDown") { event.preventDefault(); resizeDock(dockHeight - 20); }
            if (event.key === "Home") { event.preventDefault(); resizeDock(245); }
          }}
          title="Drag to resize panel · double-click to reset"
        />
      ) : null}
      <header className="developer-dock-header">
        <div className="developer-dock-tabs">
          <button type="button" className={activeTab === "terminal" ? "active" : ""} onClick={() => { setCollapsed(false); setActiveTab("terminal"); }}>
            <NavIcon name="terminal" size={14} /> Terminal
          </button>
          <button type="button" className={activeTab === "problems" ? "active" : ""} onClick={() => { setCollapsed(false); setActiveTab("problems"); }}>
            Problems {problems.length > 0 ? <span>{problems.length}</span> : null}
          </button>
        </div>
        <div className="developer-dock-title">{workspaceName}</div>
        <button type="button" className="developer-dock-collapse" onClick={() => setCollapsed((current) => !current)} title={collapsed ? "Expand panel" : "Collapse panel"}>
          {collapsed ? "⌃" : "⌄"}
        </button>
      </header>

      {!collapsed && activeTab === "terminal" ? (
        <div className="developer-terminal-body terminal-real-layout">
          <div className="terminal-shell-toolbar">
            <div className="terminal-shell-identity">
              <span className="terminal-shell-dot" />
              <strong>bash</strong>
              <span>{terminalCwd}</span>
            </div>
            <div className="terminal-shell-actions">
              {quickChecks.map((check) => (
                <button type="button" key={check.command} disabled={busy} onClick={() => void run(check.command)}>{check.label}</button>
              ))}
              <button type="button" className={showProcesses ? "active" : ""} onClick={() => setShowProcesses((current) => !current)}>
                Processes{runningProcessCount > 0 ? ` ${runningProcessCount}` : ""}
              </button>
              <button type="button" className={terminalSearchOpen ? "active" : ""} onClick={() => { setTerminalSearchOpen((current) => !current); setTerminalSearchIndex(0); }} title="Search terminal output · Ctrl+F">Search</button>
              <button type="button" onClick={clearTerminal}>Clear</button>
            </div>
          </div>

          {terminalSearchOpen ? (
            <div className="terminal-search-bar">
              <NavIcon name="search" size={13} />
              <input
                autoFocus
                value={terminalSearchQuery}
                onChange={(event) => { setTerminalSearchQuery(event.target.value); setTerminalSearchIndex(0); }}
                onKeyDown={(event) => {
                  if (event.key === "Enter") stepTerminalSearch(event.shiftKey ? -1 : 1);
                  if (event.key === "Escape") { setTerminalSearchOpen(false); window.requestAnimationFrame(() => terminalInputRef.current?.focus()); }
                }}
                placeholder="Search terminal output"
                aria-label="Search terminal output"
              />
              <span>{terminalSearchCount > 0 ? `${terminalSearchIndex + 1}/${terminalSearchCount}` : "0/0"}</span>
              <button type="button" onClick={() => stepTerminalSearch(-1)} disabled={terminalSearchCount === 0} title="Previous match">↑</button>
              <button type="button" onClick={() => stepTerminalSearch(1)} disabled={terminalSearchCount === 0} title="Next match">↓</button>
              <button type="button" onClick={() => { setTerminalSearchOpen(false); window.requestAnimationFrame(() => terminalInputRef.current?.focus()); }} title="Close search">×</button>
            </div>
          ) : null}

          <div
            className="terminal-shell-screen"
            ref={terminalScreenRef}
            onClick={() => {
              if (!window.getSelection()?.toString()) terminalInputRef.current?.focus();
            }}
            role="presentation"
          >
            <div className="terminal-shell-welcome">
              RepoTunnel Terminal · bash · {workspaceName}
              <span>Enter runs · ↑/↓ history · Ctrl+L clears · Ctrl+Enter starts a managed process</span>
            </div>

            {sessionEntries.map((entry) => (
              <div className="terminal-shell-entry" key={entry.id}>
                <div className="terminal-shell-command">
                  <span className="terminal-shell-user">{terminalUser}@repotunnel</span>
                  <span className="terminal-shell-separator">:</span>
                  <span className="terminal-shell-path">{entry.cwd}</span>
                  <span className="terminal-shell-dollar">$</span>
                  <span className="terminal-shell-command-text">{renderTerminalText(entry.command)}</span>
                </div>
                {entry.stdout ? <pre className={entry.status === "process" ? "terminal-process-output" : "terminal-stdout"}>{renderTerminalText(entry.stdout)}</pre> : null}
                {entry.stderr ? <pre className="terminal-stderr">{renderTerminalText(entry.stderr)}</pre> : null}
                {entry.status === "running" ? <div className="terminal-running-line"><span /> Running…</div> : null}
                {entry.status === "failed" || entry.status === "timedOut" ? (
                  <div className="terminal-exit-line">[{entry.status === "timedOut" ? "timed out" : `exit ${entry.exitCode ?? 1}`}]</div>
                ) : null}
              </div>
            ))}

            <div className={`terminal-shell-input-line ${busy ? "busy" : ""}`}>
              <span className="terminal-shell-user">{terminalUser}@repotunnel</span>
              <span className="terminal-shell-separator">:</span>
              <span className="terminal-shell-path">{terminalCwd}</span>
              <span className="terminal-shell-dollar">$</span>
              <input
                ref={terminalInputRef}
                value={command}
                onChange={(event) => { setCommand(event.target.value); setHistoryCursor(null); }}
                onKeyDown={(event) => {
                  if (event.key === "ArrowUp") {
                    event.preventDefault();
                    navigateCommandHistory(-1);
                    return;
                  }
                  if (event.key === "ArrowDown") {
                    event.preventDefault();
                    navigateCommandHistory(1);
                    return;
                  }
                  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "l") {
                    event.preventDefault();
                    clearTerminal();
                    return;
                  }
                  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "f") {
                    event.preventDefault();
                    setTerminalSearchOpen(true);
                    setTerminalSearchIndex(0);
                    return;
                  }
                  if (event.key === "c" && event.ctrlKey && !busy) {
                    event.preventDefault();
                    setCommand("");
                    setHistoryCursor(null);
                    return;
                  }
                  if (event.key === "Enter") {
                    event.preventDefault();
                    void run(command, event.ctrlKey || event.metaKey);
                  }
                }}
                placeholder={busy ? "Command running…" : ""}
                disabled={busy}
                spellCheck={false}
                autoCapitalize="off"
                autoCorrect="off"
                aria-label="Terminal command"
              />
              <span className="terminal-caret" aria-hidden="true" />
            </div>
          </div>

          <div className="terminal-shell-statusbar">
            <span>bash</span>
            <span>{busy ? "Running command…" : "Ready"}</span>
            <span>{runningProcessCount > 0 ? `${runningProcessCount} managed process${runningProcessCount === 1 ? "" : "es"} running` : "No managed processes running"}</span>
          </div>

          {showProcesses ? (
            <aside className="terminal-process-drawer">
              <div className="terminal-process-drawer-header">
                <strong>Managed processes</strong>
                <button type="button" onClick={() => setShowProcesses(false)}>×</button>
              </div>
              <div className="developer-process-list">
                {processes.map((process) => (
                  <div className="developer-process-row" key={process.id}>
                    <button type="button" className="process-main" onClick={() => void inspectProcess(process)}>
                      <span>{process.label}</span>
                      <i className={statusClass(process.status)}>{process.status}</i>
                    </button>
                    <div className="process-actions">
                      {process.status === "running" ? <button type="button" onClick={() => void stopProcess(process)}>Stop</button> : null}
                      {["stopped", "exited", "failed"].includes(process.status) ? <button type="button" onClick={() => void restartProcess(process)}>Restart</button> : null}
                    </div>
                    {processOutput[process.id] !== undefined ? <pre>{processOutput[process.id] || "No output."}</pre> : null}
                  </div>
                ))}
                {processes.length === 0 ? <p>No managed processes.</p> : null}
              </div>
            </aside>
          ) : null}
        </div>
      ) : null}

      {!collapsed && activeTab === "problems" ? (
        <div className="developer-problems-body">
          <div className="developer-problems-summary">
            <div className="developer-problems-counts">
              <strong>{problems.length === 0 ? "No problems" : `${problems.length} problem${problems.length === 1 ? "" : "s"}`}</strong>
              <span className="problem-count error">Errors {errorCount}</span>
              <span className="problem-count warning">Warnings {warningCount}</span>
            </div>
            <span>Build, lint, test and compiler diagnostics appear here and inline in the editor.</span>
            {problems.length > 0 ? <button type="button" className="developer-problems-clear" onClick={clearProblems}>Clear</button> : null}
          </div>
          <div className="developer-problem-list">
            {groupedProblems.map(([path, fileProblems]) => (
              <section className="developer-problem-group" key={path}>
                <div className="developer-problem-group-title">
                  <strong>{path.split("/").pop() ?? path}</strong>
                  <span>{path} · {fileProblems.length}</span>
                </div>
                {fileProblems.map((problem) => (
                  <button type="button" key={problem.key} onClick={() => onOpenProblem(problem.path, problem.line, problem.column)}>
                    <span className={`problem-severity ${problem.severity}`}>{problem.severity === "error" ? "×" : "!"}</span>
                    <span className="problem-copy">
                      <strong>{problem.message}</strong>
                      <small>Ln {problem.line}, Col {problem.column} · {problem.source}</small>
                    </span>
                  </button>
                ))}
              </section>
            ))}
            {problems.length === 0 ? (
              <div className="developer-problems-empty">
                <p>Run a project check, build, lint, test, Cargo check, or another compiler command from Terminal. File/line diagnostics will appear here automatically.</p>
              </div>
            ) : null}
          </div>
        </div>
      ) : null}
    </section>
  );
}
