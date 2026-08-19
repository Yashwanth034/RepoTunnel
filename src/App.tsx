import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import AppHeader from "./components/AppHeader";
import AppErrorBoundary from "./components/AppErrorBoundary";
import AppSidebar, { type AppView } from "./components/AppSidebar";
import ChangeHistoryPanel from "./components/ChangeHistoryPanel";
import PendingChangeReview from "./components/PendingChangeReview";
import CheckpointManager from "./components/CheckpointManager";
import ChatConnectionPanel from "./components/ChatConnectionPanel";
import ConfirmationDialog from "./components/ConfirmationDialog";
import ExecutionPanel from "./components/ExecutionPanel";
import GatewayPanel from "./components/GatewayPanel";
import GitPanel from "./components/GitPanel";
import ProductionPanel from "./components/ProductionPanel";
import HelpPanel from "./components/HelpPanel";
import HomeWorkspace from "./components/HomeWorkspace";
import HomeFeatureDialog from "./components/HomeFeatureDialog";
import ProjectRail from "./components/ProjectRail";
import TeamPanel from "./components/TeamPanel";
import WindowChrome from "./components/WindowChrome";
import ProjectOverviewPanel from "./components/ProjectOverviewPanel";
import PublicTunnelPanel from "./components/PublicTunnelPanel";
import WorkspaceList from "./components/WorkspaceList";
import WorkflowPanel from "./components/WorkflowPanel";
import WorkspaceEditor, { type EditorDocument, type EditorRevealLocation } from "./components/WorkspaceEditor";
import WorkspaceProductivity from "./components/WorkspaceProductivity";
import {
  getFileInfo,
  openWorkspacePathLocal,
  previewWorkspaceImage,
  readFile,
  saveEditorFile,
} from "./lib/filesystem";
import {
  addWorkspace,
  approveChange,
  createCheckpoint,
  clearVersionHistory,
  getAiAccessStatus,
  getChatConnectionStatus,
  getGatewayStatus,
  getWorkspaceHealth,
  getActivityTimeline,
  getGitStatus,
  getPublicTunnelStatus,
  getVersionTimeline,
  listChanges,
  listWorkspaces,
  restoreVersion,
  runSafetyScan,
  rejectChange,
  removeWorkspace,
  relocateWorkspace,
  restartPublicTunnel,
  clearPublicTunnel,
  revokeMcpAccess,
  configurePublicTunnel,
  selectWorkspace,
  startChatConnection,
  startGateway,
  stopChatConnection,
  stopGateway,
  setAiAccessPaused,
  updateWorkspaceAccess,
  updateWorkspaceChangePolicy,
  updateWorkspaceCommandPolicy,
} from "./lib/backend";
import type {
  ActivityTimeline,
  ChangeRecord,
  CheckpointSummary,
  ChatConnectionStatus,
  CommandPolicy,
  DirectoryEntry,
  GatewayStatus,
  GitRepositoryStatus,
  PublicTunnelStatus,
  SafetyScanResult,
  Workspace,
  WorkspaceHealth,
  WorkspaceAccessMode,
  WorkspaceChangePolicy,
  VersionTimeline,
} from "./types";

const initialStatus: GatewayStatus = {
  running: false,
  port: null,
  workspaceCount: 0,
};


const initialPublicTunnelStatus: PublicTunnelStatus = {
  configured: false,
  running: false,
  ready: false,
  publicUrl: null,
  mcpUrl: null,
  autoStart: false,
  requestCount: 0,
  lastRemoteRequestAt: null,
  message: null,
};

const initialConnectionStatus: ChatConnectionStatus = {
  clientAvailable: false,
  clientVersion: null,
  running: false,
  ready: false,
  tunnelId: null,
  adminUrl: null,
  message: null,
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function editorKey(workspaceId: string, path: string): string {
  return `${workspaceId}:${path}`;
}

function fileExtension(path: string): string {
  const name = path.split("/").pop() ?? path;
  const index = name.lastIndexOf(".");
  return index < 0 ? "" : name.slice(index + 1).toLowerCase();
}

function editorLanguage(path: string): string {
  const ext = fileExtension(path);
  const languages: Record<string, string> = {
    js: "javascript", mjs: "javascript", cjs: "javascript",
    ts: "typescript", tsx: "tsx", jsx: "jsx",
    py: "python", rs: "rust", html: "html", htm: "html",
    css: "css", scss: "css", sass: "css", less: "css",
    json: "json", md: "markdown", mdx: "markdown",
    yaml: "yaml", yml: "yaml", toml: "toml", sql: "sql",
    sh: "shell", bash: "shell", zsh: "shell", txt: "text",
  };
  return languages[ext] ?? "text";
}

function isImageFile(path: string): boolean {
  return ["png", "jpg", "jpeg", "gif", "webp", "bmp"].includes(fileExtension(path));
}

function isKnownBinary(path: string): boolean {
  return [
    "pdf", "zip", "gz", "tar", "7z", "rar", "exe", "dll", "so", "dylib",
    "bin", "class", "jar", "wasm", "mp3", "wav", "ogg", "mp4", "mov", "avi",
    "woff", "woff2", "ttf", "otf", "ico", "psd", "sqlite", "db",
  ].includes(fileExtension(path));
}

const EDITOR_SESSION_KEY = "repotunnel.editorSession.v1";
const MAX_RESTORED_TABS = 16;
const MAX_DRAFT_BYTES = 700 * 1024;
const EDITOR_RECENT_PREFIX = "repotunnel.editorRecent.";
const UI_SCALE_KEY = "repotunnel.uiScale.v1";
const UI_SCALE_STEPS = [100, 110, 125, 140, 150] as const;

function initialUiScale(): number {
  try {
    const stored = Number(window.localStorage.getItem(UI_SCALE_KEY));
    return UI_SCALE_STEPS.includes(stored as (typeof UI_SCALE_STEPS)[number]) ? stored : 100;
  } catch {
    return 100;
  }
}

function rememberRecentEditorFile(workspaceId: string, path: string) {
  try {
    const key = `${EDITOR_RECENT_PREFIX}${workspaceId}`;
    const parsed = JSON.parse(window.localStorage.getItem(key) ?? "[]");
    const current = Array.isArray(parsed) ? parsed.filter((item): item is string => typeof item === "string") : [];
    const next = [path, ...current.filter((item) => item !== path)].slice(0, 40);
    window.localStorage.setItem(key, JSON.stringify(next));
  } catch {
    // Recent-file history is convenience-only.
  }
}

type PersistedEditorTab = {
  workspaceId: string;
  path: string;
  dirty: boolean;
  draftContent?: string;
  savedContent?: string;
};

type PersistedEditorSession = {
  tabs: PersistedEditorTab[];
  activeKey: string | null;
  secondaryKey: string | null;
};

async function loadEditorDocument(workspace: Workspace, entry: DirectoryEntry): Promise<EditorDocument> {
  const key = editorKey(workspace.id, entry.path);
  const info = await getFileInfo(workspace.id, entry.path);
  if (info.kind !== "file") throw new Error("Only regular project files can be opened in the editor.");
  const size = info.size ?? entry.size ?? 0;

  if (isImageFile(entry.path)) {
    const image = await previewWorkspaceImage(workspace.id, entry.path);
    return {
      key, workspaceId: workspace.id, workspaceName: workspace.name, path: entry.path, name: entry.name,
      kind: "image", language: "image", content: "", savedContent: "", size: image.size,
      modifiedAt: info.modifiedAt, imageDataUrl: `data:${image.mimeType};base64,${image.dataBase64}`,
      dirty: false, externalContent: null, externalModifiedAt: null, conflict: false,
      externalDeleted: false, updatedExternally: false, readonly: workspace.accessMode === "readOnly",
    };
  }

  if (isKnownBinary(entry.path) || size > 2 * 1024 * 1024) {
    return {
      key, workspaceId: workspace.id, workspaceName: workspace.name, path: entry.path, name: entry.name,
      kind: "binary", language: "binary", content: "", savedContent: "", size,
      modifiedAt: info.modifiedAt, imageDataUrl: null, dirty: false, externalContent: null,
      externalModifiedAt: null, conflict: false, externalDeleted: false, updatedExternally: false,
      readonly: workspace.accessMode === "readOnly",
    };
  }

  try {
    const file = await readFile(workspace.id, entry.path);
    return {
      key, workspaceId: workspace.id, workspaceName: workspace.name, path: entry.path, name: entry.name,
      kind: "text", language: editorLanguage(entry.path), content: file.content, savedContent: file.content,
      size: file.size, modifiedAt: file.modifiedAt, imageDataUrl: null, dirty: false,
      externalContent: null, externalModifiedAt: null, conflict: false, externalDeleted: false,
      updatedExternally: false, readonly: workspace.accessMode === "readOnly",
    };
  } catch (error) {
    const message = errorMessage(error);
    if (!/utf-?8|binary|text file/i.test(message)) throw error;
    return {
      key, workspaceId: workspace.id, workspaceName: workspace.name, path: entry.path, name: entry.name,
      kind: "binary", language: "binary", content: "", savedContent: "", size,
      modifiedAt: info.modifiedAt, imageDataUrl: null, dirty: false, externalContent: null,
      externalModifiedAt: null, conflict: false, externalDeleted: false, updatedExternally: false,
      readonly: workspace.accessMode === "readOnly",
    };
  }
}

function App() {
  const [activeView, setActiveView] = useState<AppView>("overview");
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [selectedWorkspaceId, setSelectedWorkspaceId] = useState<string | null>(null);
  const [changes, setChanges] = useState<ChangeRecord[]>([]);
  const [versionTimeline, setVersionTimeline] = useState<VersionTimeline>({ records: [], currentVersionId: null });
  const [activityTimeline, setActivityTimeline] = useState<ActivityTimeline>({ groups: [] });
  const [versionBusy, setVersionBusy] = useState(false);
  const [gatewayStatus, setGatewayStatus] = useState<GatewayStatus>(initialStatus);
  const [connectionStatus, setConnectionStatus] = useState<ChatConnectionStatus>(
    initialConnectionStatus,
  );
  const [publicTunnelStatus, setPublicTunnelStatus] = useState<PublicTunnelStatus>(initialPublicTunnelStatus);
  const [publicTunnelBusy, setPublicTunnelBusy] = useState(false);
  const [loading, setLoading] = useState(true);
  const [adding, setAdding] = useState(false);
  const [gatewayBusy, setGatewayBusy] = useState(false);
  const [connectionBusy, setConnectionBusy] = useState(false);
  const [workspaceToRemove, setWorkspaceToRemove] = useState<Workspace | null>(null);
  const [removingId, setRemovingId] = useState<string | null>(null);
  const [updatingId, setUpdatingId] = useState<string | null>(null);
  const [changeBusyId, setChangeBusyId] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [uiScale, setUiScale] = useState(initialUiScale);
  const [focusMode, setFocusMode] = useState(false);
  const [workspaceHealth, setWorkspaceHealth] = useState<Record<string, WorkspaceHealth>>({});
  const [relocatingWorkspaceId, setRelocatingWorkspaceId] = useState<string | null>(null);
  const [pendingWorkspaceSelection, setPendingWorkspaceSelection] = useState<string | null>(null);
  const [confirmStopGateway, setConfirmStopGateway] = useState(false);
  const [confirmForgetPublicTunnel, setConfirmForgetPublicTunnel] = useState(false);
  const [confirmRevokeMcpAccess, setConfirmRevokeMcpAccess] = useState(false);
  const [aiAccessPaused, setAiAccessPausedState] = useState(false);
  const [checkpointBusy, setCheckpointBusy] = useState(false);
  const [safetyBusy, setSafetyBusy] = useState(false);
  const [aiAccessBusy, setAiAccessBusy] = useState(false);
  const [checkpointResult, setCheckpointResult] = useState<CheckpointSummary | null>(null);
  const [safetyScanResult, setSafetyScanResult] = useState<SafetyScanResult | null>(null);
  const [editorTabs, setEditorTabs] = useState<EditorDocument[]>([]);
  const [activeEditorKey, setActiveEditorKey] = useState<string | null>(null);
  const [editorSavingKey, setEditorSavingKey] = useState<string | null>(null);
  const [projectTreeRefreshToken, setProjectTreeRefreshToken] = useState(0);
  const [gitStatus, setGitStatus] = useState<GitRepositoryStatus | null>(null);
  const [editorRevealLocation, setEditorRevealLocation] = useState<EditorRevealLocation | null>(null);
  const [secondaryEditorKey, setSecondaryEditorKey] = useState<string | null>(null);
  const [closedEditorTabs, setClosedEditorTabs] = useState<Array<{ workspaceId: string; path: string }>>([]);
  const editorTabsRef = useRef<EditorDocument[]>([]);
  const editorSavingKeyRef = useRef<string | null>(null);
  const editorSessionRestoredRef = useRef(false);

  const pendingCount = useMemo(
    () => changes.filter((change) => change.status === "pending").length,
    [changes],
  );

  const activeEditorDocument = useMemo(
    () => editorTabs.find((tab) => tab.key === activeEditorKey) ?? null,
    [editorTabs, activeEditorKey],
  );

  const workspacePathById = useMemo(
    () => Object.fromEntries(workspaces.map((workspace) => [workspace.id, workspace.path])),
    [workspaces],
  );

  useEffect(() => {
    editorTabsRef.current = editorTabs;
  }, [editorTabs]);

  useEffect(() => {
    editorSavingKeyRef.current = editorSavingKey;
  }, [editorSavingKey]);

  const handleExecutionError = useCallback((message: string) => {
    setNotice(`Command execution: ${message}`);
  }, []);

  const refreshChanges = useCallback(async () => {
    const savedChanges = await listChanges(undefined, 100);
    setChanges(savedChanges);
    if (selectedWorkspaceId) {
      const [versions, activities] = await Promise.all([
        getVersionTimeline(selectedWorkspaceId),
        getActivityTimeline(selectedWorkspaceId),
      ]);
      setVersionTimeline(versions);
      setActivityTimeline(activities);
    } else {
      setVersionTimeline({ records: [], currentVersionId: null });
      setActivityTimeline({ groups: [] });
    }
  }, [selectedWorkspaceId]);

  const refresh = useCallback(async () => {
    const [savedWorkspaces, status, publicStatus, chatStatus, savedChanges, aiStatus] = await Promise.all([
      listWorkspaces(),
      getGatewayStatus(),
      getPublicTunnelStatus(),
      getChatConnectionStatus(),
      listChanges(undefined, 100),
      getAiAccessStatus(),
    ]);
    setWorkspaces(savedWorkspaces);
    setGatewayStatus(status);
    setPublicTunnelStatus(publicStatus);
    setConnectionStatus(chatStatus);
    setChanges(savedChanges);
    setAiAccessPausedState(aiStatus.paused);
  }, []);

  const refreshWorkspaceHealth = useCallback(async () => {
    if (workspaces.length === 0) {
      setWorkspaceHealth({});
      return;
    }
    const entries = await Promise.all(workspaces.map(async (workspace) => {
      try {
        return [workspace.id, await getWorkspaceHealth(workspace.id)] as const;
      } catch (error) {
        return [workspace.id, { workspaceId: workspace.id, available: false, message: errorMessage(error) }] as const;
      }
    }));
    setWorkspaceHealth(Object.fromEntries(entries));
  }, [workspaces]);

  const refreshGitStatus = useCallback(async () => {
    if (!selectedWorkspaceId) {
      setGitStatus(null);
      return;
    }
    try {
      setGitStatus(await getGitStatus(selectedWorkspaceId));
    } catch {
      setGitStatus(null);
    }
  }, [selectedWorkspaceId]);

  const syncEditorFiles = useCallback(async () => {
    const tabs = editorTabsRef.current;
    if (tabs.length === 0) return;

    const updates = await Promise.all(tabs.map(async (tab): Promise<EditorDocument> => {
      if (editorSavingKeyRef.current === tab.key) return tab;
      try {
        const info = await getFileInfo(tab.workspaceId, tab.path);
        if (info.kind !== "file") return { ...tab, externalDeleted: true };
        if (info.modifiedAt === tab.modifiedAt && !tab.externalDeleted) return tab;

        if (tab.kind === "image") {
          const image = await previewWorkspaceImage(tab.workspaceId, tab.path);
          return {
            ...tab,
            size: image.size,
            modifiedAt: info.modifiedAt,
            imageDataUrl: `data:${image.mimeType};base64,${image.dataBase64}`,
            externalDeleted: false,
            updatedExternally: true,
          };
        }
        if (tab.kind === "binary") {
          return { ...tab, size: info.size ?? tab.size, modifiedAt: info.modifiedAt, externalDeleted: false };
        }

        const remote = await readFile(tab.workspaceId, tab.path);
        if (remote.content === tab.savedContent) {
          return { ...tab, size: remote.size, modifiedAt: remote.modifiedAt, externalDeleted: false };
        }
        if (tab.dirty && remote.content !== tab.content) {
          return {
            ...tab,
            size: remote.size,
            modifiedAt: remote.modifiedAt,
            externalContent: remote.content,
            externalModifiedAt: remote.modifiedAt,
            conflict: true,
            externalDeleted: false,
          };
        }
        return {
          ...tab,
          content: remote.content,
          savedContent: remote.content,
          size: remote.size,
          modifiedAt: remote.modifiedAt,
          dirty: false,
          externalContent: null,
          externalModifiedAt: null,
          conflict: false,
          externalDeleted: false,
          updatedExternally: true,
        };
      } catch {
        return { ...tab, externalDeleted: true };
      }
    }));

    setEditorTabs((current) => {
      const byKey = new Map(updates.map((tab) => [tab.key, tab]));
      return current.map((tab) => byKey.get(tab.key) ?? tab);
    });
  }, []);

  useEffect(() => {
    refresh()
      .catch((error) => setNotice(`RepoTunnel could not initialize: ${errorMessage(error)}`))
      .finally(() => setLoading(false));
  }, [refresh]);

  useEffect(() => {
    if (!notice) return;

    const timer = window.setTimeout(() => setNotice(null), 3800);
    return () => window.clearTimeout(timer);
  }, [notice]);

  useEffect(() => {
    try {
      window.localStorage.setItem(UI_SCALE_KEY, String(uiScale));
    } catch {
      // Interface scaling remains available for this session even if storage is unavailable.
    }
    document.documentElement.style.setProperty("--repotunnel-ui-scale", String(uiScale / 100));
    document.documentElement.style.setProperty("--repotunnel-ui-extent", `${10_000 / uiScale}%`);
  }, [uiScale]);

  useEffect(() => {
    function handleAppShortcuts(event: KeyboardEvent) {
      if (!(event.ctrlKey || event.metaKey)) return;
      if (event.key === "+" || event.key === "=") {
        event.preventDefault();
        setUiScale((current) => UI_SCALE_STEPS.find((value) => value > current) ?? UI_SCALE_STEPS[UI_SCALE_STEPS.length - 1]);
        return;
      }
      if (event.key === "-") {
        event.preventDefault();
        setUiScale((current) => [...UI_SCALE_STEPS].reverse().find((value) => value < current) ?? UI_SCALE_STEPS[0]);
        return;
      }
      if (event.key === "0") {
        event.preventDefault();
        setUiScale(100);
        return;
      }
      if (event.shiftKey && event.key === "Enter") {
        event.preventDefault();
        setFocusMode((current) => !current);
      }
    }
    window.addEventListener("keydown", handleAppShortcuts, true);
    return () => window.removeEventListener("keydown", handleAppShortcuts, true);
  }, []);

  useEffect(() => {
    void refreshWorkspaceHealth();
    if (workspaces.length === 0) return;
    const timer = window.setInterval(() => void refreshWorkspaceHealth(), 12_000);
    return () => window.clearInterval(timer);
  }, [workspaces.length, refreshWorkspaceHealth]);

  useEffect(() => {
    if (workspaces.length === 0) {
      setSelectedWorkspaceId(null);
      return;
    }

    setSelectedWorkspaceId((current) =>
      current && workspaces.some((workspace) => workspace.id === current)
        ? current
        : workspaces[0].id,
    );
  }, [workspaces]);

  useEffect(() => {
    if (loading || editorSessionRestoredRef.current) return;

    let session: PersistedEditorSession | null = null;
    try {
      const raw = window.localStorage.getItem(EDITOR_SESSION_KEY);
      if (raw) session = JSON.parse(raw) as PersistedEditorSession;
    } catch {
      window.localStorage.removeItem(EDITOR_SESSION_KEY);
    }
    if (!session || !Array.isArray(session.tabs) || session.tabs.length === 0) {
      editorSessionRestoredRef.current = true;
      return;
    }

    let cancelled = false;
    async function restoreSession() {
      const restored: EditorDocument[] = [];
      for (const saved of session!.tabs.slice(0, MAX_RESTORED_TABS)) {
        const workspace = workspaces.find((item) => item.id === saved.workspaceId);
        if (!workspace || typeof saved.path !== "string" || !saved.path) continue;
        try {
          const info = await getFileInfo(workspace.id, saved.path);
          if (info.kind !== "file") continue;
          const entry: DirectoryEntry = {
            name: saved.path.split("/").pop() ?? saved.path,
            path: saved.path,
            kind: "file",
            size: info.size,
            modifiedAt: info.modifiedAt,
          };
          let document = await loadEditorDocument(workspace, entry);
          if (document.kind === "text" && saved.dirty && typeof saved.draftContent === "string" && typeof saved.savedContent === "string") {
            if (saved.savedContent === document.savedContent) {
              document = { ...document, content: saved.draftContent, dirty: saved.draftContent !== document.savedContent };
            } else if (saved.draftContent === document.content) {
              document = { ...document, dirty: false };
            } else {
              document = {
                ...document,
                content: saved.draftContent,
                savedContent: saved.savedContent,
                dirty: true,
                externalContent: document.content,
                externalModifiedAt: document.modifiedAt,
                conflict: true,
              };
            }
          }
          restored.push(document);
        } catch {
          // A removed/protected file is skipped; session restore must never block app startup.
        }
      }
      if (cancelled) return;
      editorSessionRestoredRef.current = true;
      if (restored.length === 0) {
        window.localStorage.removeItem(EDITOR_SESSION_KEY);
        return;
      }
      const keys = new Set(restored.map((tab) => tab.key));
      const activeKey = session!.activeKey && keys.has(session!.activeKey) ? session!.activeKey : restored[restored.length - 1].key;
      const secondaryKey = session!.secondaryKey && keys.has(session!.secondaryKey) && session!.secondaryKey !== activeKey ? session!.secondaryKey : null;
      setEditorTabs(restored);
      setActiveEditorKey(activeKey);
      setSecondaryEditorKey(secondaryKey);
      const activeTab = restored.find((tab) => tab.key === activeKey) ?? restored[0];
      setSelectedWorkspaceId(activeTab.workspaceId);
      setActiveView("editor");
    }
    void restoreSession();
    return () => { cancelled = true; };
  }, [loading, workspaces]);

  useEffect(() => {
    if (!editorSessionRestoredRef.current) return;
    const persisted: PersistedEditorSession = {
      tabs: editorTabs.slice(0, MAX_RESTORED_TABS).map((tab) => {
        const keepDraft = tab.kind === "text" && tab.dirty && new Blob([tab.content]).size <= MAX_DRAFT_BYTES;
        return {
          workspaceId: tab.workspaceId,
          path: tab.path,
          dirty: keepDraft,
          draftContent: keepDraft ? tab.content : undefined,
          savedContent: keepDraft ? tab.savedContent : undefined,
        };
      }),
      activeKey: activeEditorKey,
      secondaryKey: secondaryEditorKey,
    };
    try {
      if (persisted.tabs.length === 0) window.localStorage.removeItem(EDITOR_SESSION_KEY);
      else window.localStorage.setItem(EDITOR_SESSION_KEY, JSON.stringify(persisted));
    } catch {
      // Storage quotas should not interfere with editing; fall back to restoring file paths next time.
      try {
        const lightweight = { ...persisted, tabs: persisted.tabs.map((tab) => ({ workspaceId: tab.workspaceId, path: tab.path, dirty: false })) };
        window.localStorage.setItem(EDITOR_SESSION_KEY, JSON.stringify(lightweight));
      } catch {
        // Ignore unavailable local storage.
      }
    }
  }, [editorTabs, activeEditorKey, secondaryEditorKey]);

  useEffect(() => {
    if (!selectedWorkspaceId) {
      setVersionTimeline({ records: [], currentVersionId: null });
      setActivityTimeline({ groups: [] });
      return;
    }
    Promise.all([getVersionTimeline(selectedWorkspaceId), getActivityTimeline(selectedWorkspaceId)])
      .then(([versions, activities]) => {
        setVersionTimeline(versions);
        setActivityTimeline(activities);
      })
      .catch((error) => setNotice(`Could not load history: ${errorMessage(error)}`));
  }, [selectedWorkspaceId]);

  useEffect(() => {
    void refreshGitStatus();
    if (!selectedWorkspaceId) return;
    const timer = window.setInterval(() => void refreshGitStatus(), 3000);
    return () => window.clearInterval(timer);
  }, [selectedWorkspaceId, refreshGitStatus]);

  useEffect(() => {
    if (!connectionStatus.running) return;

    const timer = window.setInterval(() => {
      getChatConnectionStatus().then(setConnectionStatus).catch(() => undefined);
    }, 5000);

    return () => window.clearInterval(timer);
  }, [connectionStatus.running, connectionStatus.ready]);

  useEffect(() => {
    if (!publicTunnelStatus.configured) return;
    const timer = window.setInterval(() => {
      getPublicTunnelStatus().then(setPublicTunnelStatus).catch(() => undefined);
    }, 4000);
    return () => window.clearInterval(timer);
  }, [publicTunnelStatus.configured]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;

    listen("repotunnel://changes-updated", () => {
      refreshChanges().catch(() => undefined);
      syncEditorFiles().catch(() => undefined);
      setProjectTreeRefreshToken((current) => current + 1);
      refreshGitStatus().catch(() => undefined);
    })
      .then((stopListening) => {
        if (disposed) stopListening();
        else unlisten = stopListening;
      })
      .catch(() => undefined);

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [refreshChanges, syncEditorFiles, refreshGitStatus]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;

    listen("repotunnel://activity-updated", () => {
      if (!selectedWorkspaceId) return;
      getActivityTimeline(selectedWorkspaceId).then(setActivityTimeline).catch(() => undefined);
    })
      .then((stopListening) => {
        if (disposed) stopListening();
        else unlisten = stopListening;
      })
      .catch(() => undefined);

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [selectedWorkspaceId]);

  useEffect(() => {
    if (!gatewayStatus.running) return;

    const timer = window.setInterval(() => {
      refreshChanges().catch(() => undefined);
    }, 5000);

    return () => window.clearInterval(timer);
  }, [gatewayStatus.running, refreshChanges]);

  useEffect(() => {
    if (editorTabs.length === 0) return;
    const timer = window.setInterval(() => {
      syncEditorFiles().catch(() => undefined);
    }, 3500);
    return () => window.clearInterval(timer);
  }, [editorTabs.length, syncEditorFiles]);

  async function handleAddWorkspace() {
    setNotice(null);
    setAdding(true);

    try {
      const selected = await selectWorkspace();
      if (!selected) return;
      await addWorkspace(selected);
      await refresh();
      setActiveView("projects");
    } catch (error) {
      setNotice(`Could not add that project: ${errorMessage(error)}`);
    } finally {
      setAdding(false);
    }
  }

  async function handleRelocateWorkspace(workspace: Workspace) {
    setNotice(null);
    setRelocatingWorkspaceId(workspace.id);
    try {
      const selected = await selectWorkspace();
      if (!selected) return;
      const updated = await relocateWorkspace(workspace.id, selected);
      replaceWorkspace(updated);
      setProjectTreeRefreshToken((current) => current + 1);
      setNotice(`Project path repaired: ${updated.name}.`);
      await refreshWorkspaceHealth();
      await syncEditorFiles().catch(() => undefined);
    } catch (error) {
      setNotice(`Could not repair that project path: ${errorMessage(error)}`);
    } finally {
      setRelocatingWorkspaceId(null);
    }
  }

  async function handleWorkspaceHealthRetry(workspaceId: string) {
    try {
      const health = await getWorkspaceHealth(workspaceId);
      setWorkspaceHealth((current) => ({ ...current, [workspaceId]: health }));
      setNotice(health.available ? "Project folder is available again." : (health.message ?? "Project folder is still unavailable."));
      if (health.available) setProjectTreeRefreshToken((current) => current + 1);
    } catch (error) {
      setNotice(`Could not recheck that project: ${errorMessage(error)}`);
    }
  }

  async function confirmWorkspaceRemoval() {
    if (!workspaceToRemove) return;

    setNotice(null);
    setRemovingId(workspaceToRemove.id);

    try {
      const removedWorkspaceId = workspaceToRemove.id;
      const updated = await removeWorkspace(removedWorkspaceId);
      setWorkspaces(updated);
      setEditorTabs((current) => current.filter((tab) => tab.workspaceId !== removedWorkspaceId));
      if (activeEditorDocument?.workspaceId === removedWorkspaceId) setActiveEditorKey(null);
      setGatewayStatus((current) => ({ ...current, workspaceCount: updated.length }));
      setWorkspaceToRemove(null);
    } catch (error) {
      setNotice(`Could not remove that project: ${errorMessage(error)}`);
    } finally {
      setRemovingId(null);
    }
  }

  function replaceWorkspace(updated: Workspace) {
    setWorkspaces((current) =>
      current.map((workspace) => (workspace.id === updated.id ? updated : workspace)),
    );
  }

  async function handleAccessChange(workspace: Workspace, accessMode: WorkspaceAccessMode) {
    setNotice(null);
    setUpdatingId(workspace.id);
    try {
      replaceWorkspace(await updateWorkspaceAccess(workspace.id, accessMode));
    } catch (error) {
      setNotice(`Could not change project access: ${errorMessage(error)}`);
    } finally {
      setUpdatingId(null);
    }
  }

  async function handleChangePolicyChange(
    workspace: Workspace,
    changePolicy: WorkspaceChangePolicy,
  ) {
    setNotice(null);
    setUpdatingId(workspace.id);
    try {
      replaceWorkspace(await updateWorkspaceChangePolicy(workspace.id, changePolicy));
      setNotice(
        changePolicy === "automatic"
          ? `${workspace.name}: AI edits will apply automatically with version protection.`
          : `${workspace.name}: AI edits will wait for local review.`,
      );
    } catch (error) {
      setNotice(`Could not change the AI edit policy: ${errorMessage(error)}`);
    } finally {
      setUpdatingId(null);
    }
  }

  async function handlePendingChange(
    change: ChangeRecord,
    action: "approve" | "reject",
  ) {
    setNotice(null);
    setChangeBusyId(change.id);
    try {
      if (action === "approve") {
        await approveChange(change.id);
        setNotice(`Applied ${change.primaryPath} and saved it to version history.`);
      } else {
        await rejectChange(change.id);
        setNotice(`Rejected ${change.primaryPath}.`);
      }
      await refreshChanges();
    } catch (error) {
      const verb = action === "approve" ? "apply" : "reject";
      setNotice(`Could not ${verb} that AI change: ${errorMessage(error)}`);
      await refreshChanges().catch(() => undefined);
    } finally {
      setChangeBusyId(null);
    }
  }

  async function handleCommandPolicyChange(workspace: Workspace, commandPolicy: CommandPolicy) {
    setNotice(null);
    setUpdatingId(workspace.id);
    try {
      replaceWorkspace(await updateWorkspaceCommandPolicy(workspace.id, commandPolicy));
    } catch (error) {
      setNotice(`Could not change the command policy: ${errorMessage(error)}`);
    } finally {
      setUpdatingId(null);
    }
  }

  async function handlePublicTunnelConfigure(authtoken: string) {
    setNotice(null);
    setPublicTunnelBusy(true);
    try {
      setPublicTunnelStatus(await configurePublicTunnel(authtoken));
      setGatewayStatus(await getGatewayStatus());
      setNotice("Public ChatGPT connection is ready. Copy the MCP URL and connect it in ChatGPT once.");
    } catch (error) {
      setNotice(`Public connection failed: ${errorMessage(error)}`);
      setPublicTunnelStatus(await getPublicTunnelStatus().catch(() => initialPublicTunnelStatus));
      throw error;
    } finally {
      setPublicTunnelBusy(false);
    }
  }

  async function handlePublicTunnelRestart() {
    setNotice(null);
    setPublicTunnelBusy(true);
    try {
      setPublicTunnelStatus(await restartPublicTunnel());
      setGatewayStatus(await getGatewayStatus());
      setNotice("Public ChatGPT connection restarted.");
    } catch (error) {
      setNotice(`Could not restart public connection: ${errorMessage(error)}`);
      setPublicTunnelStatus(await getPublicTunnelStatus().catch(() => initialPublicTunnelStatus));
    } finally {
      setPublicTunnelBusy(false);
    }
  }

  async function handlePublicTunnelForget() {
    setConfirmForgetPublicTunnel(true);
  }

  async function handleRevokeMcpAccess() {
    setConfirmRevokeMcpAccess(true);
  }

  async function performRevokeMcpAccess() {
    setNotice(null);
    setPublicTunnelBusy(true);
    try {
      await revokeMcpAccess();
      setNotice("MCP access revoked. ChatGPT will need to sign in with RepoTunnel again.");
    } catch (error) {
      setNotice(`Could not revoke MCP access: ${errorMessage(error)}`);
    } finally {
      setPublicTunnelBusy(false);
      setConfirmRevokeMcpAccess(false);
    }
  }

  async function performPublicTunnelForget() {
    setNotice(null);
    setPublicTunnelBusy(true);
    try {
      setPublicTunnelStatus(await clearPublicTunnel());
      setNotice("Public connection setup removed from this device.");
    } catch (error) {
      setNotice(`Could not remove public connection setup: ${errorMessage(error)}`);
    } finally {
      setPublicTunnelBusy(false);
      setConfirmForgetPublicTunnel(false);
    }
  }

  async function handleChatConnect(tunnelId: string, apiKey: string) {
    setNotice(null);
    setConnectionBusy(true);
    try {
      const status = await startChatConnection(tunnelId, apiKey);
      setConnectionStatus(status);
      setGatewayStatus(await getGatewayStatus());
    } catch (error) {
      setNotice(`ChatGPT connection failed: ${errorMessage(error)}`);
    } finally {
      setConnectionBusy(false);
    }
  }

  async function handleChatDisconnect() {
    setNotice(null);
    setConnectionBusy(true);
    try {
      setConnectionStatus(await stopChatConnection());
    } catch (error) {
      setNotice(`Could not stop the ChatGPT connection: ${errorMessage(error)}`);
    } finally {
      setConnectionBusy(false);
    }
  }

  function selectedWorkspace(): Workspace | null {
    if (!selectedWorkspaceId) return workspaces[0] ?? null;
    return workspaces.find((workspace) => workspace.id === selectedWorkspaceId) ?? workspaces[0] ?? null;
  }

  async function handleCreateCheckpoint() {
    const workspace = selectedWorkspace();
    if (!workspace) {
      setNotice("Open a project before creating a checkpoint.");
      return;
    }

    setNotice(null);
    setCheckpointBusy(true);
    try {
      const result = await createCheckpoint(workspace.id);
      setSafetyScanResult(null);
      setCheckpointResult(result);
    } catch (error) {
      setNotice(`Could not create checkpoint: ${errorMessage(error)}`);
    } finally {
      setCheckpointBusy(false);
    }
  }

  async function handleSafetyScan() {
    const workspace = selectedWorkspace();
    if (!workspace) {
      setNotice("Open a project before running a safety scan.");
      return;
    }

    setNotice(null);
    setSafetyBusy(true);
    try {
      const result = await runSafetyScan(workspace.id);
      setCheckpointResult(null);
      setSafetyScanResult(result);
    } catch (error) {
      setNotice(`Safety scan failed: ${errorMessage(error)}`);
    } finally {
      setSafetyBusy(false);
    }
  }

  async function handleAiAccessToggle() {
    setNotice(null);
    setAiAccessBusy(true);
    try {
      const result = await setAiAccessPaused(!aiAccessPaused);
      setAiAccessPausedState(result.paused);
      setNotice(result.paused
        ? "AI access paused. MCP clients cannot read or change approved project files until you resume access."
        : "AI access resumed. Connected MCP clients can use approved projects again.");
    } catch (error) {
      setNotice(`Could not update AI access: ${errorMessage(error)}`);
    } finally {
      setAiAccessBusy(false);
    }
  }

  async function handleRestoreVersion(versionId: string | null) {
    const workspace = selectedWorkspace();
    if (!workspace) {
      setNotice("Select a project before restoring version history.");
      return;
    }

    setNotice(null);
    setVersionBusy(true);
    try {
      const result = await restoreVersion(workspace.id, versionId);
      setVersionTimeline(await getVersionTimeline(workspace.id));
      await refreshChanges().catch(() => undefined);
      setNotice(`Version restored. ${result.restoredFiles} files restored${result.removedFiles ? `, ${result.removedFiles} newer files removed` : ""}. Later versions remain saved.`);
    } catch (error) {
      setNotice(`Could not restore version: ${errorMessage(error)}`);
      throw error;
    } finally {
      setVersionBusy(false);
    }
  }

  async function handleClearVersionHistory() {
    const workspace = selectedWorkspace();
    if (!workspace) {
      setNotice("Select a project before clearing version history.");
      return;
    }

    setNotice(null);
    try {
      const result = await clearVersionHistory(workspace.id);
      await refreshChanges();
      const removedRecords = result.removedChanges + result.removedActivities + result.removedOperationalRecords;
      setNotice(
        `History cleared. Removed ${result.removedVersions} saved ${result.removedVersions === 1 ? "version" : "versions"} and ${removedRecords} activity ${removedRecords === 1 ? "record" : "records"}. Current project files were not changed.`,
      );
    } catch (error) {
      setNotice(`Could not clear history: ${errorMessage(error)}`);
      throw error;
    }
  }

  async function handleOpenEditorFile(workspace: Workspace, entry: DirectoryEntry) {
    if (entry.kind !== "file") return;
    const key = editorKey(workspace.id, entry.path);
    const existing = editorTabsRef.current.find((tab) => tab.key === key);
    setSelectedWorkspaceId(workspace.id);
    rememberRecentEditorFile(workspace.id, entry.path);
    setActiveView("editor");
    if (existing) {
      setActiveEditorKey(key);
      return;
    }

    try {
      const document = await loadEditorDocument(workspace, entry);
      setEditorTabs((current) => current.some((tab) => tab.key === key) ? current : [...current, document]);
      setActiveEditorKey(key);
    } catch (error) {
      setNotice(`Could not open ${entry.path}: ${errorMessage(error)}`);
    }
  }

  async function handleOpenEditorPath(workspaceId: string, path: string, line = 1, column = 1) {
    const workspace = workspaces.find((item) => item.id === workspaceId);
    if (!workspace) {
      setNotice("That file belongs to a project that is no longer approved.");
      return;
    }
    try {
      const info = await getFileInfo(workspaceId, path);
      if (info.kind !== "file") throw new Error("The selected path is not a regular file.");
      const entry: DirectoryEntry = {
        name: path.split("/").pop() ?? path,
        path,
        kind: "file",
        size: info.size,
        modifiedAt: info.modifiedAt,
      };
      await handleOpenEditorFile(workspace, entry);
      const key = editorKey(workspaceId, path);
      setActiveEditorKey(key);
      if (line > 1 || column > 1) setEditorRevealLocation({ key, line, column, token: Date.now() });
    } catch (error) {
      setNotice(`Could not open ${path}: ${errorMessage(error)}`);
    }
  }

  async function handleOpenEditorProblem(workspaceId: string, path: string, line: number, column: number) {
    const workspace = workspaces.find((item) => item.id === workspaceId);
    if (!workspace) {
      setNotice("That problem belongs to a project that is no longer approved.");
      return;
    }
    try {
      const info = await getFileInfo(workspaceId, path);
      if (info.kind !== "file") throw new Error("The diagnostic path is not a regular file.");
      const entry: DirectoryEntry = {
        name: path.split("/").pop() ?? path,
        path,
        kind: "file",
        size: info.size,
        modifiedAt: info.modifiedAt,
      };
      await handleOpenEditorFile(workspace, entry);
      const key = editorKey(workspaceId, path);
      setActiveEditorKey(key);
      setEditorRevealLocation({ key, line, column, token: Date.now() });
    } catch (error) {
      setNotice(`Could not open diagnostic ${path}: ${errorMessage(error)}`);
    }
  }

  function handleEditorSelect(key: string) {
    setActiveEditorKey(key);
    const tab = editorTabsRef.current.find((item) => item.key === key);
    if (tab) setSelectedWorkspaceId(tab.workspaceId);
  }

  function handleEditorChange(key: string, content: string) {
    setEditorTabs((current) => current.map((tab) => tab.key === key && !tab.readonly
      ? { ...tab, content, dirty: content !== tab.savedContent, updatedExternally: false }
      : tab));
  }

  async function handleEditorSave(key: string): Promise<boolean> {
    const tab = editorTabsRef.current.find((item) => item.key === key);
    if (!tab || tab.kind !== "text" || tab.readonly || tab.externalDeleted) return false;
    setEditorSavingKey(key);
    try {
      const result = await saveEditorFile(tab.workspaceId, tab.path, tab.content, tab.savedContent);
      if (!result.applied) throw new Error("RepoTunnel did not apply the manual save.");
      const file = await readFile(tab.workspaceId, tab.path);
      setEditorTabs((current) => current.map((item) => item.key === key ? {
        ...item,
        content: file.content,
        savedContent: file.content,
        size: file.size,
        modifiedAt: file.modifiedAt,
        dirty: false,
        conflict: false,
        externalContent: null,
        externalModifiedAt: null,
        externalDeleted: false,
        updatedExternally: false,
      } : item));
      setProjectTreeRefreshToken((current) => current + 1);
      await refreshChanges().catch(() => undefined);
      await refreshGitStatus().catch(() => undefined);
      setNotice(`Saved ${tab.path}. A restore point was added to History.`);
      return true;
    } catch (error) {
      const message = errorMessage(error);
      if (/changed externally/i.test(message)) {
        editorSavingKeyRef.current = null;
        await syncEditorFiles().catch(() => undefined);
      }
      setNotice(`Could not save ${tab.path}: ${message}`);
      return false;
    } finally {
      setEditorSavingKey(null);
    }
  }

  function handleEditorClose(key: string) {
    const tab = editorTabsRef.current.find((item) => item.key === key);
    if (!tab) return;
    setClosedEditorTabs((current) => [
      { workspaceId: tab.workspaceId, path: tab.path },
      ...current.filter((item) => !(item.workspaceId === tab.workspaceId && item.path === tab.path)),
    ].slice(0, 20));
    setEditorTabs((current) => current.filter((item) => item.key !== key));
    if (secondaryEditorKey === key) setSecondaryEditorKey(null);
    if (activeEditorKey === key) {
      const remaining = editorTabsRef.current.filter((item) => item.key !== key);
      setActiveEditorKey(remaining.at(-1)?.key ?? null);
    }
  }

  async function handleReopenClosedEditor() {
    const target = closedEditorTabs[0];
    if (!target) return;
    const workspace = workspaces.find((item) => item.id === target.workspaceId);
    if (!workspace) {
      setClosedEditorTabs((current) => current.slice(1));
      setNotice("That recently closed file belongs to a project that is no longer approved.");
      return;
    }
    try {
      const info = await getFileInfo(target.workspaceId, target.path);
      if (info.kind !== "file") throw new Error("The recently closed path is no longer a file.");
      const entry: DirectoryEntry = {
        name: target.path.split("/").pop() ?? target.path,
        path: target.path,
        kind: "file",
        size: info.size,
        modifiedAt: info.modifiedAt,
      };
      setClosedEditorTabs((current) => current.slice(1));
      await handleOpenEditorFile(workspace, entry);
    } catch (error) {
      setClosedEditorTabs((current) => current.slice(1));
      setNotice(`Could not reopen ${target.path}: ${errorMessage(error)}`);
    }
  }

  function handleSelectWorkspaceSafe(workspaceId: string) {
    if (workspaceId === selectedWorkspaceId) return;
    const dirty = editorTabsRef.current.filter((tab) => tab.workspaceId === selectedWorkspaceId && tab.dirty);
    if (dirty.length > 0) {
      setPendingWorkspaceSelection(workspaceId);
      return;
    }
    setSelectedWorkspaceId(workspaceId);
  }

  async function handleEditorOpenExternal(key: string) {
    const tab = editorTabsRef.current.find((item) => item.key === key);
    if (!tab) return;
    try {
      await openWorkspacePathLocal(tab.workspaceId, tab.path);
      setNotice(`Opened ${tab.path} with the system application.`);
    } catch (error) {
      setNotice(`Could not open ${tab.path}: ${errorMessage(error)}`);
    }
  }

  function handleReloadExternal(key: string) {
    setEditorTabs((current) => current.map((tab) => tab.key === key && tab.externalContent !== null ? {
      ...tab,
      content: tab.externalContent,
      savedContent: tab.externalContent,
      modifiedAt: tab.externalModifiedAt,
      dirty: false,
      conflict: false,
      externalContent: null,
      externalModifiedAt: null,
      updatedExternally: true,
    } : tab));
  }

  function handleKeepLocal(key: string) {
    setEditorTabs((current) => current.map((tab) => tab.key === key ? {
      ...tab,
      savedContent: tab.externalContent ?? tab.savedContent,
      modifiedAt: tab.externalModifiedAt ?? tab.modifiedAt,
      dirty: tab.content !== (tab.externalContent ?? tab.savedContent),
      conflict: false,
      externalContent: null,
      externalModifiedAt: null,
    } : tab));
  }

  function handleEntryRemoved(workspaceId: string, path: string, kind: DirectoryEntry["kind"]) {
    const prefix = `${path}/`;
    const removed = (tab: EditorDocument) => tab.workspaceId === workspaceId
      && (tab.path === path || (kind === "directory" && tab.path.startsWith(prefix)));
    const previousTabs = editorTabsRef.current;
    const nextTabs = previousTabs.filter((tab) => !removed(tab));
    setEditorTabs(nextTabs);
    const active = previousTabs.find((tab) => tab.key === activeEditorKey);
    if (active && removed(active)) setActiveEditorKey(nextTabs.at(-1)?.key ?? null);
    const secondary = previousTabs.find((tab) => tab.key === secondaryEditorKey);
    if (secondary && removed(secondary)) setSecondaryEditorKey(null);
    setProjectTreeRefreshToken((current) => current + 1);
    refreshChanges().catch(() => undefined);
    refreshGitStatus().catch(() => undefined);
  }

  function handleEntryRenamed(workspaceId: string, oldPath: string, newPath: string, kind: DirectoryEntry["kind"]) {
    const prefix = `${oldPath}/`;
    const nextTabs = editorTabsRef.current.map((tab) => {
      if (tab.workspaceId !== workspaceId) return tab;
      if (tab.path !== oldPath && !(kind === "directory" && tab.path.startsWith(prefix))) return tab;
      const nextPath = tab.path === oldPath ? newPath : `${newPath}/${tab.path.slice(prefix.length)}`;
      return { ...tab, key: editorKey(workspaceId, nextPath), path: nextPath, name: nextPath.split("/").pop() ?? nextPath, language: editorLanguage(nextPath) };
    });
    const previousActive = editorTabsRef.current.find((tab) => tab.key === activeEditorKey);
    const nextActive = previousActive ? nextTabs.find((tab) => tab.workspaceId === previousActive.workspaceId && (
      previousActive.path === oldPath ? tab.path === newPath :
      kind === "directory" && previousActive.path.startsWith(prefix) ? tab.path === `${newPath}/${previousActive.path.slice(prefix.length)}` :
      tab.path === previousActive.path
    )) : null;
    const previousSecondary = editorTabsRef.current.find((tab) => tab.key === secondaryEditorKey);
    const nextSecondary = previousSecondary ? nextTabs.find((tab) => tab.workspaceId === previousSecondary.workspaceId && (
      previousSecondary.path === oldPath ? tab.path === newPath :
      kind === "directory" && previousSecondary.path.startsWith(prefix) ? tab.path === `${newPath}/${previousSecondary.path.slice(prefix.length)}` :
      tab.path === previousSecondary.path
    )) : null;
    setEditorTabs(nextTabs);
    if (nextActive) setActiveEditorKey(nextActive.key);
    if (nextSecondary) setSecondaryEditorKey(nextSecondary.key);
    setProjectTreeRefreshToken((current) => current + 1);
    refreshChanges().catch(() => undefined);
    refreshGitStatus().catch(() => undefined);
  }

  function handleGatewayToggle() {
    if (gatewayStatus.running) {
      setConfirmStopGateway(true);
      return;
    }
    void performGatewayToggle(false);
  }

  async function performGatewayToggle(stop: boolean) {
    setNotice(null);
    setGatewayBusy(true);
    try {
      const status = stop ? await stopGateway() : await startGateway();
      setGatewayStatus(status);
      setPublicTunnelStatus(await getPublicTunnelStatus());
      setConnectionStatus(await getChatConnectionStatus());
      setNotice(stop ? "Gateway stopped cleanly." : "Gateway started.");
    } catch (error) {
      setNotice(`Gateway action failed: ${errorMessage(error)}`);
    } finally {
      setGatewayBusy(false);
      setConfirmStopGateway(false);
    }
  }

  function renderPage() {
    if (activeView === "projects") {
      return (
        <div className="page-stack">
          <WorkspaceList
            workspaces={workspaces}
            adding={adding}
            removingId={removingId}
            updatingId={updatingId}
            workspaceHealth={workspaceHealth}
            relocatingWorkspaceId={relocatingWorkspaceId}
            onAdd={handleAddWorkspace}
            onRemove={setWorkspaceToRemove}
            onAccessChange={handleAccessChange}
            onChangePolicyChange={handleChangePolicyChange}
            onCommandPolicyChange={handleCommandPolicyChange}
            onRelocate={(workspace) => void handleRelocateWorkspace(workspace)}
            onRetryHealth={(workspaceId) => void handleWorkspaceHealthRetry(workspaceId)}
          />
          <ProjectOverviewPanel workspaces={workspaces} />
        </div>
      );
    }

    if (activeView === "team") {
      return (
        <TeamPanel
          workspaces={workspaces}
          selectedWorkspaceId={selectedWorkspaceId}
          onSelectWorkspace={handleSelectWorkspaceSafe}
          onNotice={setNotice}
        />
      );
    }

    if (activeView === "changes") {
      return (
        <div className="page-stack">
          <PendingChangeReview
            changes={changes}
            busyId={changeBusyId}
            onApprove={(change) => void handlePendingChange(change, "approve")}
            onReject={(change) => void handlePendingChange(change, "reject")}
          />
          <ChangeHistoryPanel
            timeline={versionTimeline}
            activityTimeline={activityTimeline}
            workspaceName={selectedWorkspace()?.name ?? null}
            busy={versionBusy}
            onRestore={handleRestoreVersion}
            onClear={handleClearVersionHistory}
            onRefresh={() => {
              if (!selectedWorkspaceId) return;
              Promise.all([getVersionTimeline(selectedWorkspaceId), getActivityTimeline(selectedWorkspaceId)])
                .then(([versions, activities]) => {
                  setVersionTimeline(versions);
                  setActivityTimeline(activities);
                })
                .catch((error) => setNotice(`Could not refresh history: ${errorMessage(error)}`));
            }}
          />
          <CheckpointManager
            workspaces={workspaces}
            selectedWorkspaceId={selectedWorkspaceId}
            onError={(message) => setNotice(`Checkpoint: ${message}`)}
            onNotice={setNotice}
            onRestored={() => {
              refreshChanges().catch(() => undefined);
            }}
          />
        </div>
      );
    }

    if (activeView === "checks") {
      return (
        <WorkflowPanel
          workspaces={workspaces}
          gatewayRunning={gatewayStatus.running}
          chatConnected={publicTunnelStatus.ready || connectionStatus.ready}
        />
      );
    }

    if (activeView === "commands") {
      return (
        <ExecutionPanel
          workspaces={workspaces}
          gatewayRunning={gatewayStatus.running}
          onError={handleExecutionError}
        />
      );
    }

    if (activeView === "git") {
      return (
        <GitPanel
          workspaces={workspaces}
          gatewayRunning={gatewayStatus.running}
          onError={(message) => setNotice(`Git: ${message}`)}
          onChangeQueued={() => refreshChanges().catch(() => undefined)}
        />
      );
    }

    if (activeView === "connections") {
      return (
        <div className="page-stack">
          <GatewayPanel
            status={gatewayStatus}
            connection={connectionStatus}
            publicTunnel={publicTunnelStatus}
            aiAccessPaused={aiAccessPaused}
            busy={gatewayBusy}
            onToggle={handleGatewayToggle}
          />
          <PublicTunnelPanel
            status={publicTunnelStatus}
            gatewayRunning={gatewayStatus.running}
            busy={publicTunnelBusy}
            onConfigure={handlePublicTunnelConfigure}
            onRestart={handlePublicTunnelRestart}
            onRevoke={handleRevokeMcpAccess}
            onForget={handlePublicTunnelForget}
          />
          <ChatConnectionPanel
            status={connectionStatus}
            gatewayRunning={gatewayStatus.running}
            busy={connectionBusy}
            aiAccessPaused={aiAccessPaused}
            onConnect={handleChatConnect}
            onDisconnect={handleChatDisconnect}
          />
        </div>
      );
    }

    if (activeView === "help") return <HelpPanel />;
    return (
      <ProductionPanel
        onError={(message) => setNotice(`Runtime: ${message}`)}
        onNotice={setNotice}
        uiScale={uiScale}
        onUiScaleChange={setUiScale}
      />
    );
  }

  return (
    <div className="desktop-app">
      <WindowChrome />
      <div className={`desktop-shell ${focusMode ? "focus-mode" : ""}`}>
        <AppSidebar
          activeView={activeView}
          pendingCount={pendingCount}
          onNavigate={setActiveView}
        />

        <ProjectRail
          workspaces={workspaces}
          selectedWorkspaceId={selectedWorkspaceId}
          activeEditorPath={activeEditorDocument?.workspaceId === selectedWorkspaceId ? activeEditorDocument.path : null}
          refreshToken={projectTreeRefreshToken}
          gitChanges={gitStatus?.available ? gitStatus.changes : []}
          workspaceHealth={workspaceHealth}
          relocatingWorkspaceId={relocatingWorkspaceId}
          onSelectWorkspace={handleSelectWorkspaceSafe}
          onRemoveWorkspace={setWorkspaceToRemove}
          onOpenFile={(workspace, entry) => void handleOpenEditorFile(workspace, entry)}
          onEntryRemoved={handleEntryRemoved}
          onEntryRenamed={handleEntryRenamed}
          onRelocateWorkspace={(workspace) => void handleRelocateWorkspace(workspace)}
          onRetryWorkspace={(workspaceId) => void handleWorkspaceHealthRetry(workspaceId)}
          onNotice={setNotice}
        />

        <main className={`main-view ${activeView === "overview" ? "home-main-view" : ""} ${activeView === "editor" ? "editor-main-view" : ""}`}>
          <AppHeader
            view={activeView}
            gatewayRunning={gatewayStatus.running}
            connectionReady={publicTunnelStatus.ready || connectionStatus.ready}
            aiAccessPaused={aiAccessPaused}
            focusMode={focusMode}
            onToggleFocusMode={() => setFocusMode((current) => !current)}
          />

          {notice ? (
            <div className="notice desktop-notice" role="alert">
              <span>{notice}</span>
              <button type="button" onClick={() => setNotice(null)} aria-label="Dismiss message">×</button>
            </div>
          ) : null}

          <AppErrorBoundary
            resetKey={`${activeView}:${selectedWorkspaceId ?? "none"}`}
            onGoHome={() => setActiveView("overview")}
          >
          {loading ? (
            <section className="loading-state" aria-live="polite">
              <div className="loader" aria-hidden="true" />
              <span>Loading RepoTunnel…</span>
            </section>
          ) : activeView === "overview" ? (
            <HomeWorkspace
              gateway={gatewayStatus}
              connection={connectionStatus}
              publicTunnel={publicTunnelStatus}
              workspaces={workspaces}
              changes={changes}
              gatewayBusy={gatewayBusy}
              adding={adding}
              checkpointBusy={checkpointBusy}
              safetyBusy={safetyBusy}
              aiAccessBusy={aiAccessBusy}
              aiAccessPaused={aiAccessPaused}
              onToggleGateway={handleGatewayToggle}
              onAddProject={handleAddWorkspace}
              onCreateCheckpoint={handleCreateCheckpoint}
              onSafetyScan={handleSafetyScan}
              onToggleAiAccess={handleAiAccessToggle}
              onNavigate={setActiveView}
            />
          ) : activeView === "editor" ? (
            <WorkspaceEditor
              tabs={editorTabs}
              activeKey={activeEditorKey}
              savingKey={editorSavingKey}
              workspacePathById={workspacePathById}
              gitChanges={gitStatus?.available ? gitStatus.changes : []}
              revealLocation={editorRevealLocation}
              secondaryKey={secondaryEditorKey}
              onSecondaryChange={setSecondaryEditorKey}
              onSelect={handleEditorSelect}
              onClose={handleEditorClose}
              onChange={handleEditorChange}
              onSave={handleEditorSave}
              onOpenExternal={(key) => void handleEditorOpenExternal(key)}
              onReloadExternal={handleReloadExternal}
              onKeepLocal={handleKeepLocal}
              onDismissExternalNotice={(key) => setEditorTabs((current) => current.map((tab) => tab.key === key ? { ...tab, updatedExternally: false } : tab))}
              onOpenProblem={(workspaceId, path, line, column) => void handleOpenEditorProblem(workspaceId, path, line, column)}
              canReopenClosed={closedEditorTabs.length > 0}
              onReopenClosed={() => void handleReopenClosedEditor()}
              onNotice={setNotice}
            />
          ) : (
            <div className="page-scroll">
              <div className="page-content">{renderPage()}</div>
            </div>
          )}
          </AppErrorBoundary>
        </main>
      </div>

      <WorkspaceProductivity
        workspaces={workspaces}
        selectedWorkspaceId={selectedWorkspaceId}
        activeFile={activeView === "editor" && activeEditorDocument ? { workspaceId: activeEditorDocument.workspaceId, path: activeEditorDocument.path, dirty: activeEditorDocument.dirty } : null}
        onOpenPath={(workspaceId, path, line, column) => void handleOpenEditorPath(workspaceId, path, line, column)}
        onSaveActive={() => { if (activeEditorKey) void handleEditorSave(activeEditorKey); }}
        onOpenExternalActive={() => { if (activeEditorKey) void handleEditorOpenExternal(activeEditorKey); }}
        onRefreshProject={() => { setProjectTreeRefreshToken((current) => current + 1); void refreshGitStatus(); }}
        onNavigate={setActiveView}
        onNotice={setNotice}
        focusMode={focusMode}
        onToggleFocusMode={() => setFocusMode((current) => !current)}
      />

      {workspaceToRemove ? (
        <ConfirmationDialog
          title="Remove project?"
          message={`RepoTunnel will forget “${workspaceToRemove.name}”. Nothing inside the project folder will be deleted.`}
          confirmLabel="Remove project"
          busy={removingId === workspaceToRemove.id}
          onCancel={() => setWorkspaceToRemove(null)}
          onConfirm={confirmWorkspaceRemoval}
        />
      ) : null}

      {pendingWorkspaceSelection ? (
        <ConfirmationDialog
          title="Switch projects with unsaved edits?"
          message="One or more files in the current project have unsaved edits. RepoTunnel will keep those drafts session-protected and leave their tabs open while you switch projects."
          confirmLabel="Switch project"
          variant="primary"
          onCancel={() => setPendingWorkspaceSelection(null)}
          onConfirm={() => {
            setSelectedWorkspaceId(pendingWorkspaceSelection);
            setPendingWorkspaceSelection(null);
          }}
        />
      ) : null}

      {confirmStopGateway ? (
        <ConfirmationDialog
          title="Stop the MCP gateway?"
          message="This cleanly stops the local MCP gateway and managed remote connections. Your projects, Team sessions, History, and saved public setup remain unchanged."
          confirmLabel="Stop gateway"
          busy={gatewayBusy}
          busyLabel="Stopping…"
          onCancel={() => setConfirmStopGateway(false)}
          onConfirm={() => void performGatewayToggle(true)}
        />
      ) : null}

      {confirmRevokeMcpAccess ? (
        <ConfirmationDialog
          title="Revoke MCP access?"
          message="This immediately invalidates the current MCP access and refresh credentials. Your public endpoint, ngrok setup, projects, and history remain unchanged. ChatGPT will need to sign in with RepoTunnel again when it next connects."
          confirmLabel="Revoke access"
          busy={publicTunnelBusy}
          busyLabel="Revoking…"
          onCancel={() => setConfirmRevokeMcpAccess(false)}
          onConfirm={() => void performRevokeMcpAccess()}
        />
      ) : null}

      {confirmForgetPublicTunnel ? (
        <ConfirmationDialog
          title="Forget public connection setup?"
          message="This removes the saved ngrok authtoken and stable public endpoint identity from this device. Your project files and RepoTunnel history are not affected."
          confirmLabel="Forget setup"
          busy={publicTunnelBusy}
          busyLabel="Removing…"
          onCancel={() => setConfirmForgetPublicTunnel(false)}
          onConfirm={() => void performPublicTunnelForget()}
        />
      ) : null}

      <HomeFeatureDialog
        checkpoint={checkpointResult}
        safetyScan={safetyScanResult}
        onClose={() => {
          setCheckpointResult(null);
          setSafetyScanResult(null);
        }}
      />
    </div>
  );
}

export default App;
