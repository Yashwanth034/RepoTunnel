import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import {
  createVideoProject,
  deleteVideoProject,
  importVideoProjectFolder,
  listVideoProjectFiles,
  listVideoProjects,
  prepareVideoProjectFilePreview,
  prepareVideoProjectPreview,
  readVideoProjectTextFile,
  renderVideoProjectTimeline,
  setVideoProjectPinned,
} from "../lib/backend";
import type {
  VideoPreviewSource,
  VideoProductionProject,
  VideoProjectFile,
  Workspace,
} from "../types";

type VideoProductionPanelProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  onNotice: (message: string) => void;
};

type AspectRatio = "16:9" | "9:16" | "1:1" | "4:5";
type AudioMode = "keep" | "mute" | "replace";

type FileTreeNode = {
  name: string;
  path: string;
  kind: "folder" | VideoProjectFile["kind"];
  file?: VideoProjectFile;
  children: FileTreeNode[];
};

type EditSegment = {
  id: string;
  sourcePath: string;
  startSeconds: number;
  endSeconds: number | null;
};

type EditState = {
  segments: EditSegment[];
  audioMode: AudioMode;
  replacementAudioPath: string;
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
}

function fileIcon(kind: FileTreeNode["kind"]): string {
  switch (kind) {
    case "folder": return "▸";
    case "video": return "▶";
    case "audio": return "♪";
    case "subtitle": return "CC";
    case "image": return "▧";
    case "text": return "≡";
    default: return "·";
  }
}

function buildFileTree(project: VideoProductionProject, files: VideoProjectFile[]): FileTreeNode[] {
  const root: FileTreeNode = {
    name: project.name,
    path: "",
    kind: "folder",
    children: [],
  };
  const prefix = `${project.relativePath}/`;

  for (const file of files) {
    const inside = file.relativePath.startsWith(prefix)
      ? file.relativePath.slice(prefix.length)
      : file.relativePath;
    const parts = inside.split("/").filter(Boolean);
    let parent = root;
    let path = "";

    parts.forEach((part, index) => {
      path = path ? `${path}/${part}` : part;
      const isFile = index === parts.length - 1;
      if (isFile) {
        parent.children.push({
          name: part,
          path,
          kind: file.kind,
          file,
          children: [],
        });
        return;
      }

      let folder = parent.children.find((item) => item.kind === "folder" && item.name === part);
      if (!folder) {
        folder = { name: part, path, kind: "folder", children: [] };
        parent.children.push(folder);
      }
      parent = folder;
    });
  }

  const sort = (nodes: FileTreeNode[]) => {
    nodes.sort((left, right) => {
      if (left.kind === "folder" && right.kind !== "folder") return -1;
      if (left.kind !== "folder" && right.kind === "folder") return 1;
      return left.name.localeCompare(right.name, undefined, { numeric: true });
    });
    nodes.forEach((node) => sort(node.children));
  };
  sort(root.children);
  return root.children;
}

function cloneEditState(state: EditState): EditState {
  return {
    audioMode: state.audioMode,
    replacementAudioPath: state.replacementAudioPath,
    segments: state.segments.map((segment) => ({ ...segment })),
  };
}

function VideoProductionPanel({
  workspaces,
  selectedWorkspaceId,
  onNotice,
}: VideoProductionPanelProps) {
  const initialWorkspaceId =
    selectedWorkspaceId && workspaces.some((workspace) => workspace.id === selectedWorkspaceId)
      ? selectedWorkspaceId
      : workspaces[0]?.id ?? "";

  const [workspaceId, setWorkspaceId] = useState(initialWorkspaceId);
  const [projects, setProjects] = useState<VideoProductionProject[]>([]);
  const [selectedProjectId, setSelectedProjectId] = useState<string | null>(null);
  const [search, setSearch] = useState("");
  const [newProjectName, setNewProjectName] = useState("");
  const [aspectRatio, setAspectRatio] = useState<AspectRatio>("16:9");
  const [creating, setCreating] = useState(false);
  const [importing, setImporting] = useState(false);
  const [deletingProjectId, setDeletingProjectId] = useState<string | null>(null);
  const [pendingDeleteProject, setPendingDeleteProject] = useState<VideoProductionProject | null>(null);
  const [pinningProjectId, setPinningProjectId] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const [files, setFiles] = useState<VideoProjectFile[]>([]);
  const [filesLoading, setFilesLoading] = useState(false);
  const [projectTreeCollapsed, setProjectTreeCollapsed] = useState(false);
  const [expandedFolders, setExpandedFolders] = useState<Set<string>>(new Set());
  const [selectedFilePath, setSelectedFilePath] = useState<string | null>(null);
  const [textPreview, setTextPreview] = useState<{ path: string; content: string } | null>(null);
  const [previewSource, setPreviewSource] = useState<VideoPreviewSource | null>(null);
  const [previewLoading, setPreviewLoading] = useState(false);

  const [editState, setEditState] = useState<EditState | null>(null);
  const [undoStack, setUndoStack] = useState<EditState[]>([]);
  const [redoStack, setRedoStack] = useState<EditState[]>([]);
  const [rendering, setRendering] = useState(false);
  const videoRef = useRef<HTMLVideoElement | null>(null);

  const selectedWorkspace = useMemo(
    () => workspaces.find((workspace) => workspace.id === workspaceId) ?? null,
    [workspaceId, workspaces],
  );
  const selectedProject = useMemo(
    () => projects.find((project) => project.id === selectedProjectId) ?? null,
    [projects, selectedProjectId],
  );
  const filteredProjects = useMemo(() => {
    const query = search.trim().toLocaleLowerCase();
    return projects.filter((project) =>
      !query || [project.name, project.slug, project.status].some((value) =>
        value.toLocaleLowerCase().includes(query),
      ),
    );
  }, [projects, search]);
  const fileTree = useMemo(
    () => selectedProject ? buildFileTree(selectedProject, files) : [],
    [files, selectedProject],
  );
  const videoFiles = useMemo(() => files.filter((file) => file.kind === "video"), [files]);
  const audioFiles = useMemo(() => files.filter((file) => file.kind === "audio"), [files]);

  useEffect(() => {
    if (workspaces.some((workspace) => workspace.id === workspaceId)) return;
    setWorkspaceId(initialWorkspaceId);
  }, [initialWorkspaceId, workspaceId, workspaces]);

  const refreshProjects = useCallback(async (preferredProjectId?: string | null) => {
    if (workspaces.length === 0) {
      setProjects([]);
      setSelectedProjectId(null);
      return;
    }
    setLoading(true);
    try {
      const groups = await Promise.all(
        workspaces.map(async (workspace) => {
          try {
            return await listVideoProjects(workspace.id);
          } catch {
            return [];
          }
        }),
      );
      const next = groups.flat().sort((left, right) => {
        if (left.pinned !== right.pinned) return left.pinned ? -1 : 1;
        return right.updatedAt - left.updatedAt;
      });
      setProjects(next);
      const preferred = preferredProjectId ?? selectedProjectId;
      setSelectedProjectId(
        (preferred && next.some((project) => project.id === preferred) ? preferred : null)
          ?? next[0]?.id
          ?? null,
      );
    } catch (error) {
      onNotice(`Video Projects: ${errorMessage(error)}`);
    } finally {
      setLoading(false);
    }
  }, [onNotice, selectedProjectId, workspaces]);

  const refreshFiles = useCallback(async (project: VideoProductionProject): Promise<VideoProjectFile[]> => {
    setFilesLoading(true);
    try {
      const next = await listVideoProjectFiles(project.workspaceId, project.id);
      setFiles(next);
      const topFolders = new Set<string>();
      const prefix = `${project.relativePath}/`;
      next.forEach((file) => {
        const inside = file.relativePath.startsWith(prefix)
          ? file.relativePath.slice(prefix.length)
          : file.relativePath;
        const first = inside.split("/")[0];
        if (first && inside.includes("/")) topFolders.add(first);
      });
      setExpandedFolders(topFolders);
      return next;
    } catch (error) {
      setFiles([]);
      onNotice(`Video Project files: ${errorMessage(error)}`);
      return [];
    } finally {
      setFilesLoading(false);
    }
  }, [onNotice]);

  const loadMainPreview = useCallback(async (
    project: VideoProductionProject,
    projectFiles: VideoProjectFile[] = [],
  ) => {
    setPreviewSource(null);
    setTextPreview(null);
    const manifestPreview = project.currentPreview ?? project.finalExport ?? project.latestDraft;
    const fallback = projectFiles.find((file) => file.kind === "video")
      ?? projectFiles.find((file) => file.kind === "audio")
      ?? projectFiles.find((file) => file.kind === "image");
    if (!manifestPreview && !fallback) return;

    setPreviewLoading(true);
    try {
      const source = manifestPreview
        ? await prepareVideoProjectPreview(project.workspaceId, project.id)
        : await prepareVideoProjectFilePreview(
            project.workspaceId,
            project.id,
            fallback!.relativePath,
          );
      setPreviewSource(source);
      setSelectedFilePath(manifestPreview ?? fallback!.relativePath);
    } catch (error) {
      onNotice(`Video preview: ${errorMessage(error)}`);
    } finally {
      setPreviewLoading(false);
    }
  }, [onNotice]);

  useEffect(() => {
    void refreshProjects();
  }, [workspaces]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    setFiles([]);
    setProjectTreeCollapsed(false);
    setSelectedFilePath(null);
    setTextPreview(null);
    setPreviewSource(null);
    setEditState(null);
    setUndoStack([]);
    setRedoStack([]);
    if (!selectedProject) return;
    void (async () => {
      const projectFiles = await refreshFiles(selectedProject);
      await loadMainPreview(selectedProject, projectFiles);
    })();
  }, [selectedProject?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  const mediaPreviewUrl = useMemo(() => {
    if (!previewSource) return null;
    if (
      !previewSource.mimeType.startsWith("video/")
      && !previewSource.mimeType.startsWith("audio/")
    ) {
      return null;
    }
    return previewSource.playbackUrl ?? convertFileSrc(previewSource.videoPath);
  }, [
    previewSource?.createdAt,
    previewSource?.mimeType,
    previewSource?.playbackUrl,
    previewSource?.videoPath,
  ]);

  function startEditing(sourcePath: string) {
    const next: EditState = {
      segments: [{
        id: `segment-${Date.now()}-0`,
        sourcePath,
        startSeconds: 0,
        endSeconds: null,
      }],
      audioMode: "keep",
      replacementAudioPath: "",
    };
    setEditState(next);
    setUndoStack([]);
    setRedoStack([]);
  }

  function commitEdit(next: EditState) {
    if (editState) {
      setUndoStack((current) => [...current.slice(-49), cloneEditState(editState)]);
    }
    setEditState(cloneEditState(next));
    setRedoStack([]);
  }

  function undoEdit() {
    if (!editState || undoStack.length === 0) return;
    const previous = undoStack[undoStack.length - 1];
    setUndoStack((current) => current.slice(0, -1));
    setRedoStack((current) => [cloneEditState(editState), ...current.slice(0, 49)]);
    setEditState(cloneEditState(previous));
  }

  function redoEdit() {
    if (!editState || redoStack.length === 0) return;
    const next = redoStack[0];
    setRedoStack((current) => current.slice(1));
    setUndoStack((current) => [...current.slice(-49), cloneEditState(editState)]);
    setEditState(cloneEditState(next));
  }

  async function createProject() {
    if (!selectedWorkspace || !newProjectName.trim()) return;
    setCreating(true);
    try {
      const created = await createVideoProject(
        selectedWorkspace.id,
        newProjectName.trim(),
        aspectRatio,
      );
      setNewProjectName("");
      await refreshProjects(created.id);
      setSelectedProjectId(created.id);
      onNotice(`Video Project "${created.name}" created.`);
    } catch (error) {
      onNotice(`Create Video Project: ${errorMessage(error)}`);
    } finally {
      setCreating(false);
    }
  }

  async function importFolder() {
    if (!selectedWorkspace || importing) return;
    const folder = await open({ directory: true, multiple: false, title: "Add Video Project Folder" });
    if (!folder || Array.isArray(folder)) return;
    setImporting(true);
    try {
      const imported = await importVideoProjectFolder(selectedWorkspace.id, folder);
      await refreshProjects(imported.id);
      setSelectedProjectId(imported.id);
      onNotice(`Video Project "${imported.name}" added from folder.`);
    } catch (error) {
      onNotice(`Add Video Project: ${errorMessage(error)}`);
    } finally {
      setImporting(false);
    }
  }

  async function togglePinned(project: VideoProductionProject) {
    if (pinningProjectId) return;
    setPinningProjectId(project.id);
    try {
      await setVideoProjectPinned(project.workspaceId, project.id, !project.pinned);
      await refreshProjects(project.id);
    } catch (error) {
      onNotice(`Video Project pin: ${errorMessage(error)}`);
    } finally {
      setPinningProjectId(null);
    }
  }

  async function deleteProject(project: VideoProductionProject) {
    if (deletingProjectId) return;
    setPendingDeleteProject(null);
    setDeletingProjectId(project.id);
    try {
      await deleteVideoProject(project.workspaceId, project.id);
      if (selectedProjectId === project.id) {
        setSelectedProjectId(null);
        setPreviewSource(null);
        setFiles([]);
      }
      await refreshProjects(null);
      onNotice(`Video Project "${project.name}" deleted.`);
    } catch (error) {
      onNotice(`Delete Video Project: ${errorMessage(error)}`);
    } finally {
      setDeletingProjectId(null);
    }
  }

  async function openProjectFile(file: VideoProjectFile) {
    if (!selectedProject) return;
    setSelectedFilePath(file.relativePath);
    if (["video", "audio", "image"].includes(file.kind)) {
      setTextPreview(null);
      setPreviewLoading(true);
      try {
        const source = await prepareVideoProjectFilePreview(
          selectedProject.workspaceId,
          selectedProject.id,
          file.relativePath,
        );
        setPreviewSource(source);
        if (file.kind === "video") startEditing(file.relativePath);
      } catch (error) {
        setPreviewSource(null);
        onNotice(`Video Project preview: ${errorMessage(error)}`);
      } finally {
        setPreviewLoading(false);
      }
      return;
    }

    if (file.kind === "text" || file.kind === "subtitle") {
      setPreviewSource(null);
      setPreviewLoading(true);
      try {
        const content = await readVideoProjectTextFile(
          selectedProject.workspaceId,
          selectedProject.id,
          file.relativePath,
        );
        setTextPreview({ path: file.relativePath, content });
      } catch (error) {
        setTextPreview(null);
        onNotice(`Open Video Project file: ${errorMessage(error)}`);
      } finally {
        setPreviewLoading(false);
      }
      return;
    }

    setPreviewSource(null);
    setTextPreview({ path: file.relativePath, content: "No built-in preview is available for this file type." });
  }

  function toggleFolder(path: string) {
    setExpandedFolders((current) => {
      const next = new Set(current);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }

  function renderTree(nodes: FileTreeNode[], depth = 0): React.ReactNode[] {
    return nodes.map((node) => {
      if (node.kind === "folder") {
        const expanded = expandedFolders.has(node.path);
        return (
          <div className="video-file-tree-branch" key={node.path}>
            <button
              type="button"
              className="video-file-tree-row folder"
              style={{ paddingLeft: `${8 + depth * 14}px` }}
              title={node.name}
              onClick={() => toggleFolder(node.path)}
            >
              <span className={`video-file-tree-chevron ${expanded ? "expanded" : ""}`}>▸</span>
              <span className="video-file-tree-name">{node.name}</span>
            </button>
            {expanded ? renderTree(node.children, depth + 1) : null}
          </div>
        );
      }

      const file = node.file;
      if (!file) return <div key={node.path} />;
      return (
        <button
          type="button"
          key={node.path}
          className={`video-file-tree-row file ${selectedFilePath === file.relativePath ? "selected" : ""}`}
          style={{ paddingLeft: `${22 + depth * 14}px` }}
          title={file.relativePath}
          onClick={() => void openProjectFile(file)}
        >
          <span className={`video-file-kind ${file.kind}`}>{fileIcon(file.kind)}</span>
          <span className="video-file-tree-name">{node.name}</span>
          <small>{formatBytes(file.sizeBytes)}</small>
        </button>
      );
    });
  }

  function updateSegment(id: string, field: "startSeconds" | "endSeconds", raw: string) {
    if (!editState) return;
    const value = raw.trim() === "" && field === "endSeconds" ? null : Number(raw);
    if (value !== null && (!Number.isFinite(value) || value < 0)) return;
    const next = cloneEditState(editState);
    next.segments = next.segments.map((segment) =>
      segment.id === id ? { ...segment, [field]: value } : segment,
    );
    commitEdit(next);
  }

  function cutSegment(id: string) {
    if (!editState || editState.segments.length <= 1) return;
    const next = cloneEditState(editState);
    next.segments = next.segments.filter((segment) => segment.id !== id);
    commitEdit(next);
  }

  function splitAtPlayhead() {
    if (!editState || !videoRef.current) return;
    const at = videoRef.current.currentTime;
    const index = editState.segments.findIndex((segment) => {
      const end = segment.endSeconds ?? Number.POSITIVE_INFINITY;
      return at > segment.startSeconds && at < end;
    });
    if (index < 0) {
      onNotice("Move the playhead inside a kept segment before splitting.");
      return;
    }
    const segment = editState.segments[index];
    const first: EditSegment = { ...segment, id: `${segment.id}-a-${Date.now()}`, endSeconds: at };
    const second: EditSegment = { ...segment, id: `${segment.id}-b-${Date.now()}`, startSeconds: at };
    const next = cloneEditState(editState);
    next.segments.splice(index, 1, first, second);
    commitEdit(next);
  }

  function setAudioMode(audioMode: AudioMode) {
    if (!editState) return;
    commitEdit({ ...cloneEditState(editState), audioMode });
  }

  async function renderManualEdit() {
    if (!selectedProject || !editState || editState.segments.length === 0 || rendering) return;
    if (editState.audioMode === "replace" && !editState.replacementAudioPath) {
      onNotice("Choose a replacement audio file first.");
      return;
    }
    for (const segment of editState.segments) {
      if (segment.endSeconds !== null && segment.endSeconds <= segment.startSeconds) {
        onNotice("Each trim end must be after its start.");
        return;
      }
    }

    setRendering(true);
    try {
      await renderVideoProjectTimeline(selectedProject.workspaceId, selectedProject.id, {
        version: 1,
        clips: editState.segments.map((segment) => ({
          sourcePath: segment.sourcePath,
          startSeconds: segment.startSeconds,
          endSeconds: segment.endSeconds,
        })),
        narrationPath: editState.audioMode === "replace"
          ? editState.replacementAudioPath
          : null,
        preserveSourceAudio: editState.audioMode === "keep",
        finalRender: false,
      });
      await refreshProjects(selectedProject.id);
      const refreshed = await listVideoProjects(selectedProject.workspaceId);
      const project = refreshed.find((item) => item.id === selectedProject.id);
      if (project) {
        await refreshFiles(project);
        await loadMainPreview(project);
      }
      onNotice("Manual edit rendered as a new draft.");
    } catch (error) {
      onNotice(`Manual edit: ${errorMessage(error)}`);
    } finally {
      setRendering(false);
    }
  }

  function handleEditorKeyDown(event: React.KeyboardEvent<HTMLElement>) {
    const target = event.target as HTMLElement;
    const inField = ["INPUT", "SELECT", "TEXTAREA"].includes(target.tagName);
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "z") {
      event.preventDefault();
      if (event.shiftKey) redoEdit();
      else undoEdit();
      return;
    }
    if (inField) return;
    if (event.key.toLowerCase() === "s") {
      event.preventDefault();
      splitAtPlayhead();
    } else if (event.code === "Space" && videoRef.current) {
      event.preventDefault();
      if (videoRef.current.paused) void videoRef.current.play();
      else videoRef.current.pause();
    }
  }

  return (
    <div className="video-production-shell">
      <aside className="video-production-rail">
        <div className="video-production-rail-head">
          <h3>Video Projects</h3>
          <span className="video-project-count">{projects.length}</span>
        </div>

        <div className="video-project-create">
          <input
            value={newProjectName}
            placeholder="New video project…"
            maxLength={120}
            disabled={!selectedWorkspace || creating}
            onChange={(event) => setNewProjectName(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") void createProject();
            }}
          />
          <div>
            <select
              aria-label="Video aspect ratio"
              value={aspectRatio}
              disabled={creating || importing}
              onChange={(event) => setAspectRatio(event.target.value as AspectRatio)}
            >
              <option value="16:9">16:9</option>
              <option value="9:16">9:16</option>
              <option value="1:1">1:1</option>
              <option value="4:5">4:5</option>
            </select>
            <button
              className="primary-button"
              type="button"
              disabled={!selectedWorkspace || !newProjectName.trim() || creating || importing}
              onClick={() => void createProject()}
            >
              {creating ? "Creating…" : "+ Create"}
            </button>
          </div>
          <button
            className="secondary-button video-project-add-folder"
            type="button"
            disabled={!selectedWorkspace || creating || importing}
            onClick={() => void importFolder()}
          >
            {importing ? "Adding folder…" : "+ Add folder"}
          </button>
        </div>

        <input
          className="video-project-search"
          value={search}
          placeholder="Search video projects…"
          onChange={(event) => setSearch(event.target.value)}
        />

        <div className="video-production-list">
          {loading && projects.length === 0 ? (
            <div className="video-empty">Loading Video Projects…</div>
          ) : filteredProjects.length === 0 ? (
            <div className="video-empty">
              {projects.length === 0
                ? "No Video Projects yet. Create one above."
                : "No Video Projects match this search."}
            </div>
          ) : (
            filteredProjects.map((project) => {
              const selected = project.id === selectedProjectId;
              return (
                <div key={project.id} className={`video-production-project-group ${selected ? "selected" : ""}`}>
                  <div className={`video-production-project-row ${selected ? "selected" : ""}`}>
                    <button
                      type="button"
                      className="video-production-project"
                      onClick={() => setSelectedProjectId(project.id)}
                      title={project.name}
                    >
                      <span className="video-production-project-icon" aria-hidden="true">▶</span>
                      <span>
                        <strong>{project.name}</strong>
                        <small>{project.aspectRatio}</small>
                      </span>
                    </button>
                    <div className="video-project-row-actions">
                      {selected ? (
                        <button
                          type="button"
                          className="video-project-tree-toggle"
                          aria-label={projectTreeCollapsed ? `Expand files for ${project.name}` : `Collapse files for ${project.name}`}
                          title={projectTreeCollapsed ? "Expand project files" : "Collapse project files"}
                          onClick={() => setProjectTreeCollapsed((current) => !current)}
                        >
                          {projectTreeCollapsed ? "⌄" : "⌃"}
                        </button>
                      ) : null}
                      <button
                        type="button"
                        className={project.pinned ? "active" : ""}
                        disabled={pinningProjectId === project.id}
                        aria-label={project.pinned ? `Unpin ${project.name}` : `Pin ${project.name}`}
                        title={project.pinned ? "Unpin project" : "Pin project"}
                        onClick={() => void togglePinned(project)}
                      >
                        {project.pinned ? "★" : "☆"}
                      </button>
                      <button
                        type="button"
                        className="remove"
                        disabled={deletingProjectId === project.id}
                        aria-label={`Delete ${project.name}`}
                        title="Delete project"
                        onClick={() => setPendingDeleteProject(project)}
                      >
                        {deletingProjectId === project.id ? "…" : "×"}
                      </button>
                    </div>
                  </div>

                  {selected && !projectTreeCollapsed ? (
                    <div className="video-project-sidebar-tree">
                      {filesLoading ? (
                        <div className="video-empty">Loading files…</div>
                      ) : fileTree.length === 0 ? (
                        <div className="video-empty">No project files yet.</div>
                      ) : (
                        renderTree(fileTree)
                      )}
                    </div>
                  ) : null}
                </div>
              );
            })
          )}
        </div>
      </aside>

      <section className="video-production-main">
        {!selectedProject ? (
          <div className="video-production-welcome">
            <span className="video-production-welcome-icon" aria-hidden="true">▶</span>
            <h2>Create or select a Video Project</h2>
          </div>
        ) : (
          <>
            <header className="video-production-project-head">
              <div>
                <h2>{selectedProject.name}</h2>
                <p>{selectedProject.relativePath}</p>
              </div>
              <div className="video-production-format">
                <strong>{selectedProject.width} × {selectedProject.height}</strong>
                <small>{selectedProject.aspectRatio} · {selectedProject.fps} FPS</small>
              </div>
            </header>

            <div className="video-project-content">
              <div className="video-preview-column">
                <div className="video-selected-file-bar">
                  <strong title={selectedFilePath ?? "Project preview"}>
                    {selectedFilePath?.split("/").pop() ?? "Project preview"}
                  </strong>
                  {selectedFilePath ? <small>{selectedFilePath}</small> : null}
                </div>

                <div
                  className="video-production-preview-screen"
                  style={{ aspectRatio: `${selectedProject.width} / ${selectedProject.height}` }}
                >
                  {previewLoading ? (
                    <div>
                      <span aria-hidden="true">▶</span>
                      <strong>Preparing preview…</strong>
                    </div>
                  ) : textPreview ? (
                    <div className="video-text-file-preview">
                      <pre>{textPreview.content}</pre>
                    </div>
                  ) : previewSource?.mimeType.startsWith("image/") ? (
                    <img
                      key={previewSource.createdAt}
                      src={convertFileSrc(previewSource.videoPath)}
                      alt={selectedFilePath?.split("/").pop() ?? "Video Project image"}
                    />
                  ) : previewSource?.mimeType.startsWith("audio/") && mediaPreviewUrl ? (
                    <audio
                      key={previewSource.createdAt}
                      controls
                      autoPlay
                      preload="auto"
                      src={mediaPreviewUrl}
                    >
                      Your system webview does not support HTML audio playback.
                    </audio>
                  ) : previewSource?.mimeType.startsWith("video/") && mediaPreviewUrl ? (
                    <video
                      ref={videoRef}
                      key={previewSource.createdAt}
                      controls
                      autoPlay
                      playsInline
                      preload="auto"
                      src={mediaPreviewUrl}
                      onCanPlay={(event) => {
                        event.currentTarget.play().catch(() => undefined);
                      }}
                      onError={(event) => {
                        const mediaError = event.currentTarget.error;
                        onNotice(
                          `Video preview playback failed${mediaError?.message ? `: ${mediaError.message}` : "."}`,
                        );
                      }}
                    >
                      {previewSource.subtitlePath ? (
                        <track
                          default
                          kind="subtitles"
                          label="Subtitles"
                          srcLang={previewSource.subtitleLanguage ?? "und"}
                          src={previewSource.subtitleUrl ?? convertFileSrc(previewSource.subtitlePath)}
                        />
                      ) : null}
                      Your system webview does not support HTML video playback.
                    </video>
                  ) : (
                    <div>
                      <span aria-hidden="true">▶</span>
                      <strong>No media selected</strong>
                      <small>Select a video, audio, image, subtitle, or text file from the project tree.</small>
                    </div>
                  )}
                </div>
              </div>
            </div>

            <section
              className="video-manual-editor"
              tabIndex={0}
              onKeyDown={handleEditorKeyDown}
              aria-label="Manual video editor"
            >
              <div className="video-manual-editor-head">
                <div>
                  <strong>Manual edit</strong>
                  <small>Trim, split, cut and control audio without changing the original media.</small>
                </div>
                <div className="video-manual-editor-actions">
                  <button className="secondary-button" type="button" disabled={undoStack.length === 0} onClick={undoEdit}>Undo</button>
                  <button className="secondary-button" type="button" disabled={redoStack.length === 0} onClick={redoEdit}>Redo</button>
                  <button className="primary-button" type="button" disabled={!editState || editState.segments.length === 0 || rendering} onClick={() => void renderManualEdit()}>
                    {rendering ? "Rendering…" : "Render draft"}
                  </button>
                </div>
              </div>

              <div className="video-manual-source-row">
                <label>
                  <span>Video</span>
                  <select
                    value={editState?.segments[0]?.sourcePath ?? ""}
                    disabled={videoFiles.length === 0 || rendering}
                    onChange={(event) => {
                      const value = event.target.value;
                      if (value) {
                        startEditing(value);
                        const file = files.find((item) => item.relativePath === value);
                        if (file) void openProjectFile(file);
                      }
                    }}
                  >
                    {videoFiles.length === 0 ? <option value="">No video files</option> : null}
                    {videoFiles.map((file) => <option key={file.relativePath} value={file.relativePath}>{file.name}</option>)}
                  </select>
                </label>
                <button className="secondary-button" type="button" disabled={!editState || !previewSource?.mimeType.startsWith("video/")} onClick={splitAtPlayhead}>
                  Split at playhead
                </button>
              </div>

              {editState ? (
                <>
                  <div className="video-edit-segments">
                    {editState.segments.map((segment, index) => (
                      <div className="video-edit-segment" key={segment.id}>
                        <strong>Clip {index + 1}</strong>
                        <label>
                          <span>Start</span>
                          <input
                            type="number"
                            min={0}
                            step="0.01"
                            value={segment.startSeconds}
                            onChange={(event) => updateSegment(segment.id, "startSeconds", event.target.value)}
                          />
                        </label>
                        <label>
                          <span>End</span>
                          <input
                            type="number"
                            min={0}
                            step="0.01"
                            placeholder="End"
                            value={segment.endSeconds ?? ""}
                            onChange={(event) => updateSegment(segment.id, "endSeconds", event.target.value)}
                          />
                        </label>
                        <button
                          type="button"
                          className="secondary-button"
                          disabled={editState.segments.length <= 1}
                          onClick={() => cutSegment(segment.id)}
                        >
                          Cut
                        </button>
                      </div>
                    ))}
                  </div>

                  <div className="video-audio-controls">
                    <label>
                      <span>Audio</span>
                      <select value={editState.audioMode} onChange={(event) => setAudioMode(event.target.value as AudioMode)}>
                        <option value="keep">Keep source audio</option>
                        <option value="mute">Mute</option>
                        <option value="replace">Replace audio</option>
                      </select>
                    </label>
                    {editState.audioMode === "replace" ? (
                      <label>
                        <span>Replacement</span>
                        <select
                          value={editState.replacementAudioPath}
                          onChange={(event) => commitEdit({ ...cloneEditState(editState), replacementAudioPath: event.target.value })}
                        >
                          <option value="">Choose audio…</option>
                          {audioFiles.map((file) => <option key={file.relativePath} value={file.relativePath}>{file.name}</option>)}
                        </select>
                      </label>
                    ) : null}
                  </div>
                </>
              ) : (
                <div className="video-empty">Select a video file to start manual editing.</div>
              )}

              <div className="video-editor-shortcuts">
                <span>Space: play/pause</span>
                <span>S: split at playhead</span>
                <span>Ctrl/Cmd+Z: undo</span>
                <span>Ctrl/Cmd+Shift+Z: redo</span>
              </div>
            </section>
          </>
        )}
      </section>

      {pendingDeleteProject ? (
        <div
          className="video-confirm-backdrop"
          role="presentation"
          onMouseDown={(event) => {
            if (event.target === event.currentTarget && !deletingProjectId) {
              setPendingDeleteProject(null);
            }
          }}
        >
          <div
            className="video-confirm-modal"
            role="dialog"
            aria-modal="true"
            aria-labelledby="video-delete-title"
          >
            <div className="video-confirm-icon" aria-hidden="true">!</div>
            <div className="video-confirm-copy">
              <strong id="video-delete-title">Delete Video Project?</strong>
              <p>
                Delete <span>"{pendingDeleteProject.name}"</span> and its RepoTunnel-managed project folder and media?
              </p>
              <small>A source folder previously added from elsewhere will not be deleted.</small>
            </div>
            <div className="video-confirm-actions">
              <button
                className="secondary-button"
                type="button"
                disabled={Boolean(deletingProjectId)}
                onClick={() => setPendingDeleteProject(null)}
              >
                Cancel
              </button>
              <button
                className="danger-button"
                type="button"
                disabled={Boolean(deletingProjectId)}
                onClick={() => void deleteProject(pendingDeleteProject)}
              >
                {deletingProjectId ? "Deleting…" : "Delete"}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}

export default VideoProductionPanel;
