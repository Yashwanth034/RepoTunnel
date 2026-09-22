import { useCallback, useEffect, useMemo, useState } from "react";
import VideoProductionPanel from "./VideoProductionPanel";
import {
  cancelVideoAnalysis,
  clearVideoCache,
  getVideoAnalysisJob,
  getVideoAnalysisResult,
  getVideoToolsStatus,
  installVideoTools,
  listVideoAnalysisJobs,
  listVideoProjects,
  startVideoAnalysis,
} from "../lib/backend";
import type {
  VideoAnalysisJob,
  VideoAnalysisMode,
  VideoAnalysisResult,
  VideoProductionProject,
  VideoToolsStatus,
  Workspace,
} from "../types";

type VideoPanelProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  onNotice: (message: string) => void;
};

const modes: Array<{ id: VideoAnalysisMode; label: string; detail: string }> = [
  { id: "full", label: "Full", detail: "Use all relevant speech, audio and visual context" },
  { id: "transcript", label: "Transcript focus", detail: "Prioritize spoken and subtitle context" },
  { id: "visual", label: "Visual focus", detail: "Prioritize scenes, design and on-screen changes" },
  { id: "instruction", label: "Tutorial focus", detail: "Prioritize instructions and important frames" },
];

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function formatBytes(bytes: number): string {
  if (bytes < 1024 ** 2) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
}

function formatDuration(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds)) return "Unknown duration";
  const total = Math.max(0, Math.round(seconds));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  return hours > 0
    ? `${hours}h ${minutes}m ${secs}s`
    : `${minutes}m ${secs}s`;
}

function formatTimestamp(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  return [hours, minutes, secs].map((value) => String(value).padStart(2, "0")).join(":");
}

function parseOptionalSeconds(value: string): number | undefined {
  const trimmed = value.trim();
  if (!trimmed) return undefined;
  const parsed = Number(trimmed);
  return Number.isFinite(parsed) ? parsed : Number.NaN;
}

function ToolPill({ label, available, source, version }: {
  label: string;
  available: boolean;
  source: string;
  version: string | null;
}) {
  return (
    <div className={`video-tool-pill ${available ? "ready" : "missing"}`}>
      <span className="video-tool-dot" aria-hidden="true" />
      <div>
        <strong>{label}</strong>
        <small>{available ? `${source === "managed" ? "RepoTunnel managed" : "System"} · ${version ?? "detected"}` : "Will prepare automatically"}</small>
      </div>
    </div>
  );
}

function VideoPanel({
  workspaces,
  selectedWorkspaceId,
  onNotice,
}: VideoPanelProps) {
  const [videoProjects, setVideoProjects] = useState<VideoProductionProject[]>([]);
  const [selectedVideoProjectId, setSelectedVideoProjectId] = useState("");
  const [tools, setTools] = useState<VideoToolsStatus | null>(null);
  const [source, setSource] = useState("");
  const [mode, setMode] = useState<VideoAnalysisMode>("full");
  const [startSeconds, setStartSeconds] = useState("");
  const [endSeconds, setEndSeconds] = useState("");
  const [maxFrames, setMaxFrames] = useState(10);
  const [activeJob, setActiveJob] = useState<VideoAnalysisJob | null>(null);
  const [result, setResult] = useState<VideoAnalysisResult | null>(null);
  const [recentJobs, setRecentJobs] = useState<VideoAnalysisJob[]>([]);
  const [loadingTools, setLoadingTools] = useState(true);
  const [preparingTools, setPreparingTools] = useState(false);
  const [starting, setStarting] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [section, setSection] = useState<"produce" | "understand">("produce");

  const selectedVideoProject = useMemo(
    () => videoProjects.find((project) => project.id === selectedVideoProjectId) ?? null,
    [selectedVideoProjectId, videoProjects],
  );
  const workspaceId = selectedVideoProject?.workspaceId ?? "";

  const refreshVideoProjects = useCallback(async () => {
    if (workspaces.length === 0) {
      setVideoProjects([]);
      setSelectedVideoProjectId("");
      return;
    }
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
    setVideoProjects(next);
    setSelectedVideoProjectId((current) =>
      current && next.some((project) => project.id === current)
        ? current
        : next[0]?.id ?? "",
    );
  }, [workspaces]);

  useEffect(() => {
    if (section !== "understand") return;
    void refreshVideoProjects();
  }, [refreshVideoProjects, section]);

  useEffect(() => {
    setSource(selectedVideoProject?.currentPreview ?? "");
  }, [selectedVideoProject?.id, selectedVideoProject?.currentPreview]);

  const refreshTools = useCallback(async () => {
    setLoadingTools(true);
    try {
      setTools(await getVideoToolsStatus());
    } catch (error) {
      onNotice(`Video: ${errorMessage(error)}`);
    } finally {
      setLoadingTools(false);
    }
  }, [onNotice]);

  const refreshJobs = useCallback(async () => {
    if (!workspaceId) {
      setRecentJobs([]);
      return;
    }
    try {
      setRecentJobs(await listVideoAnalysisJobs(workspaceId, 10));
    } catch {
      setRecentJobs([]);
    }
  }, [workspaceId]);

  useEffect(() => {
    void refreshTools();
  }, [refreshTools]);

  useEffect(() => {
    setResult(null);
    setActiveJob(null);
    void refreshJobs();
  }, [refreshJobs]);

  useEffect(() => {
    if (!activeJob || !["queued", "running"].includes(activeJob.status)) return;
    let disposed = false;
    let polling = false;

    const poll = async () => {
      if (polling) return;
      polling = true;
      try {
        const next = await getVideoAnalysisJob(activeJob.id);
        if (disposed) return;
        setActiveJob(next);
        setRecentJobs((current) => [next, ...current.filter((item) => item.id !== next.id)].slice(0, 10));
        if (next.status === "completed") {
          const analysis = await getVideoAnalysisResult(next.id);
          if (!disposed) {
            setResult(analysis);
            void refreshTools();
          }
        }
      } catch (error) {
        if (!disposed) onNotice(`Video: ${errorMessage(error)}`);
      } finally {
        polling = false;
      }
    };

    const timer = window.setInterval(() => void poll(), 700);
    void poll();
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [activeJob?.id, activeJob?.status, onNotice, refreshTools]);

  async function prepareTools() {
    setPreparingTools(true);
    try {
      const next = await installVideoTools();
      setTools(next);
      onNotice("Video helpers are ready.");
    } catch (error) {
      onNotice(`Video helper setup failed: ${errorMessage(error)}`);
    } finally {
      setPreparingTools(false);
    }
  }

  async function clearCache() {
    setClearing(true);
    try {
      const next = await clearVideoCache();
      setTools(next);
      setResult(null);
      onNotice("Video cache cleared.");
    } catch (error) {
      onNotice(`Video: ${errorMessage(error)}`);
    } finally {
      setClearing(false);
    }
  }

  async function analyze() {
    if (!selectedVideoProject || !source.trim()) return;
    const start = parseOptionalSeconds(startSeconds);
    const end = parseOptionalSeconds(endSeconds);
    if (Number.isNaN(start) || Number.isNaN(end)) {
      onNotice("Video: start/end must be numbers of seconds.");
      return;
    }
    if (start !== undefined && start < 0) {
      onNotice("Video: start time cannot be negative.");
      return;
    }
    if (end !== undefined && end <= (start ?? 0)) {
      onNotice("Video: end time must be greater than start time.");
      return;
    }

    setStarting(true);
    setResult(null);
    try {
      const job = await startVideoAnalysis(
        selectedVideoProject.workspaceId,
        source.trim(),
        mode,
        start,
        end,
        maxFrames,
      );
      setActiveJob(job);
      setRecentJobs((current) => [job, ...current.filter((item) => item.id !== job.id)].slice(0, 10));
      onNotice("Video analysis started in the background.");
    } catch (error) {
      onNotice(`Video: ${errorMessage(error)}`);
    } finally {
      setStarting(false);
    }
  }

  async function cancel() {
    if (!activeJob) return;
    try {
      setActiveJob(await cancelVideoAnalysis(activeJob.id));
    } catch (error) {
      onNotice(`Video: ${errorMessage(error)}`);
    }
  }

  async function openRecent(job: VideoAnalysisJob) {
    setActiveJob(job);
    setResult(null);
    if (job.status === "completed") {
      try {
        setResult(await getVideoAnalysisResult(job.id));
      } catch (error) {
        onNotice(`Video: ${errorMessage(error)}`);
      }
    }
  }

  const active = activeJob && ["queued", "running"].includes(activeJob.status);
  const sectionTabs = (
    <nav className="video-section-tabs" aria-label="Video section">
      <button
        type="button"
        className={section === "produce" ? "active" : ""}
        onClick={() => setSection("produce")}
      >
        <span aria-hidden="true">▶</span>
        <strong>Projects</strong>
      </button>
      <button
        type="button"
        className={section === "understand" ? "active" : ""}
        onClick={() => setSection("understand")}
      >
        <span aria-hidden="true">⌕</span>
        <strong>Analyze</strong>
      </button>
    </nav>
  );

  if (section === "produce") {
    return (
      <div className="video-page video-page-production">
        {sectionTabs}
        <VideoProductionPanel
          workspaces={workspaces}
          selectedWorkspaceId={selectedWorkspaceId}
          onNotice={onNotice}
        />
      </div>
    );
  }

  return (
    <div className="video-page">
      {sectionTabs}

      <section className="video-card">
        <div className="video-card-heading">
          <h3>Media helpers</h3>
          <div className="video-card-actions">
            {tools && !tools.ready ? (
              <button className="secondary-button" type="button" disabled={preparingTools} onClick={() => void prepareTools()}>
                {preparingTools ? "Preparing…" : "Prepare tools"}
              </button>
            ) : null}
            <button className="secondary-button" type="button" disabled={clearing || !tools || tools.cacheItems === 0} onClick={() => void clearCache()}>
              {clearing ? "Clearing…" : "Clear cache"}
            </button>
          </div>
        </div>
        {loadingTools && !tools ? (
          <div className="video-empty">Checking media helpers…</div>
        ) : tools ? (
          <>
            <div className="video-tools">
              <ToolPill label="yt-dlp" {...tools.ytDlp} />
              <ToolPill label="FFmpeg" {...tools.ffmpeg} />
              <div className="video-cache-stat">
                <strong>{formatBytes(tools.cacheBytes)}</strong>
                <small>{tools.cacheItems} cached analysis{tools.cacheItems === 1 ? "" : "es"} · limit {formatBytes(tools.cacheLimitBytes)}</small>
              </div>
            </div>
            <p className="video-runtime-message">{tools.message}</p>
          </>
        ) : null}
      </section>

      <section className="video-card video-analyze-card">
        <div className="video-card-heading">
          <div>
            <h3>Analyze media</h3>
            <p>Choose a focus if useful. The analysis runtime can still use relevant audio, transcript and visual context from the selected media.</p>
          </div>
          <select
            aria-label="Video Project for analysis"
            value={selectedVideoProjectId}
            disabled={videoProjects.length === 0 || Boolean(active)}
            onChange={(event) => setSelectedVideoProjectId(event.target.value)}
          >
            {videoProjects.length === 0 ? <option value="">No Video Projects</option> : null}
            {videoProjects.map((project) => (
              <option key={project.id} value={project.id}>{project.name}</option>
            ))}
          </select>
        </div>

        <label className="video-source-field">
          <span>Video URL or project media path</span>
          <input
            value={source}
            onChange={(event) => setSource(event.target.value)}
            placeholder="https://…  or  video-projects/project/assets/video/clip.mp4"
            disabled={Boolean(active)}
          />
          <small>Project media stays inside the approved Video Project boundary. Public URLs use the existing resolver when the source permits anonymous access.</small>
        </label>

        <div className="video-mode-grid" role="radiogroup" aria-label="Analysis focus">
          {modes.map((item) => (
            <button
              key={item.id}
              className={`video-mode ${mode === item.id ? "active" : ""}`}
              type="button"
              role="radio"
              aria-checked={mode === item.id}
              disabled={Boolean(active)}
              onClick={() => setMode(item.id)}
            >
              <strong>{item.label}</strong>
              <small>{item.detail}</small>
            </button>
          ))}
        </div>

        <div className="video-range-row">
          <label><span>Start seconds</span><input inputMode="decimal" placeholder="0" value={startSeconds} disabled={Boolean(active)} onChange={(event) => setStartSeconds(event.target.value)} /></label>
          <label><span>End seconds</span><input inputMode="decimal" placeholder="End" value={endSeconds} disabled={Boolean(active)} onChange={(event) => setEndSeconds(event.target.value)} /></label>
          <label>
            <span>Max smart frames</span>
            <input type="number" min={1} max={18} value={maxFrames} disabled={Boolean(active)} onChange={(event) => setMaxFrames(Math.min(18, Math.max(1, Number(event.target.value) || 1)))} />
          </label>
          <button className="primary-button video-analyze-button" type="button" disabled={!selectedVideoProject || !source.trim() || Boolean(active) || starting} onClick={() => void analyze()}>
            {starting ? "Starting…" : "Analyze video"}
          </button>
        </div>
      </section>

      {activeJob ? (
        <section className="video-card">
          <div className="video-job-heading">
            <div>
              <span className={`video-job-status ${activeJob.status}`}>{activeJob.status}</span>
              <h3>{activeJob.title ?? "Preparing video"}</h3>
              <p>{activeJob.message}</p>
            </div>
            {active ? <button className="secondary-button" type="button" onClick={() => void cancel()}>Cancel</button> : null}
          </div>
          <div className="video-progress-track" aria-label={`Video analysis ${activeJob.progress}%`}>
            <span style={{ width: `${activeJob.progress}%` }} />
          </div>
          <div className="video-job-metrics">
            <span>{activeJob.progress}%</span>
            <span>{activeJob.phase}</span>
            {activeJob.durationSeconds !== null ? <span>{formatDuration(activeJob.durationSeconds)}</span> : null}
            {activeJob.cacheHit ? <span>Cache hit</span> : null}
            {activeJob.transcriptAvailable ? <span>Transcript ready</span> : null}
            {activeJob.frameCount > 0 ? <span>{activeJob.frameCount} smart frames</span> : null}
            {activeJob.audioChunkCount > 0 ? <span>{activeJob.audioChunkCount} audio chunk{activeJob.audioChunkCount === 1 ? "" : "s"}</span> : null}
          </div>
        </section>
      ) : null}

      {result ? (
        <section className="video-card video-result">
          <div className="video-card-heading">
            <div>
              <span className="section-kicker">AI-ready analysis</span>
              <h3>{result.title}</h3>
              <p>{formatDuration(result.durationSeconds)} · {result.mode} · {result.cacheHit ? "reused cache" : "new analysis"}</p>
            </div>
          </div>
          <div className="video-result-summary">
            <div><strong>{result.transcript ? "Yes" : "No"}</strong><small>Transcript</small></div>
            <div><strong>{result.frames.length}</strong><small>Smart frames</small></div>
            <div><strong>{result.audioChunkCount}</strong><small>Audio context</small></div>
            <div><strong>{formatTimestamp(result.analysisStartSeconds)}</strong><small>Start</small></div>
          </div>
          {result.transcript ? (
            <div className="video-transcript">
              <div><strong>Timestamped transcript</strong><span>{result.transcriptSource ?? "captions"}</span></div>
              <pre>{result.transcript}</pre>
            </div>
          ) : (
            <div className="video-empty">
              {result.audioChunkCount > 0
                ? "No captions were available. Audio and visual context are ready for the connected AI."
                : "Visual context is ready for the connected AI."}
            </div>
          )}
          {result.frames.length > 0 ? (
            <div className="video-frame-times">
              <strong>Prepared frame timestamps</strong>
              <div>{result.frames.map((frame) => <span key={frame.index}>{formatTimestamp(frame.timestampSeconds)}</span>)}</div>
            </div>
          ) : null}
        </section>
      ) : null}

      <section className="video-card">
        <div className="video-card-heading">
          <h3>Recent analyses</h3>
          <button className="secondary-button" type="button" onClick={() => void refreshJobs()} disabled={!workspaceId}>Refresh</button>
        </div>
        {recentJobs.length === 0 ? (
          <div className="video-empty">No video analyses for this project yet.</div>
        ) : (
          <div className="video-job-list">
            {recentJobs.map((job) => (
              <button type="button" key={job.id} onClick={() => void openRecent(job)}>
                <span className={`video-job-status ${job.status}`}>{job.status}</span>
                <div><strong>{job.title ?? job.source}</strong><small>{job.mode} · {job.message}</small></div>
                <span>{job.progress}%</span>
              </button>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}

export default VideoPanel;
