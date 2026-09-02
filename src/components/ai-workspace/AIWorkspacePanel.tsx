import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  aiWorkspaceAction,
  getAiWorkspaceFrame,
  getAiWorkspaceStatus,
  startAiWorkspace,
  stopAiWorkspace,
} from "../../lib/backend";
import { detectedProductivityFamilies, isProductivityApplication } from "../../lib/applicationFamilies";
import type { AiWorkspaceFrame, AiWorkspaceStatus, LaunchApplication, Workspace } from "../../types";

type AIWorkspacePanelProps = {
  workspace: Workspace;
  applications: LaunchApplication[];
  desktopEnabled: boolean;
  onError: (message: string) => void;
};

function AIWorkspacePanel({ workspace, applications, desktopEnabled, onError }: AIWorkspacePanelProps) {
  const [status, setStatus] = useState<AiWorkspaceStatus | null>(null);
  const [frame, setFrame] = useState<AiWorkspaceFrame | null>(null);
  const [applicationId, setApplicationId] = useState("");
  const [target, setTarget] = useState("");
  const [busy, setBusy] = useState(false);
  const [backendAvailable, setBackendAvailable] = useState(true);
  const [expanded, setExpanded] = useState(false);
  const frameBusy = useRef(false);

  const selectableApps = useMemo(
    () => applications.filter((application) => application.id !== "docker" && application.category !== "browser"),
    [applications],
  );
  const productivityFamilies = useMemo(
    () => detectedProductivityFamilies(selectableApps),
    [selectableApps],
  );
  const otherSelectableApps = useMemo(
    () => selectableApps.filter((application) => !isProductivityApplication(application)),
    [selectableApps],
  );

  useEffect(() => {
    if (!applicationId || !selectableApps.some((application) => application.id === applicationId)) {
      const preferred = selectableApps.find((application) => application.id === "android-studio") ?? selectableApps[0];
      setApplicationId(preferred?.id ?? "");
    }
  }, [applicationId, selectableApps]);

  const refreshStatus = useCallback(async () => {
    try {
      const next = await getAiWorkspaceStatus(workspace.id);
      setBackendAvailable(true);
      setStatus(next);
      if (!next.running) setFrame(null);
      return next;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (message.includes("Command get_ai_workspace_status not found")) {
        setBackendAvailable(false);
        setStatus(null);
        setFrame(null);
        return null;
      }
      onError(message);
      return null;
    }
  }, [onError, workspace.id]);

  const refreshFrame = useCallback(async () => {
    if (frameBusy.current || document.visibilityState !== "visible") return;
    frameBusy.current = true;
    try {
      const next = await getAiWorkspaceFrame(workspace.id, 1440);
      setFrame(next);
    } catch {
      // Status polling handles app exits/restarts without spamming the global error surface.
    } finally {
      frameBusy.current = false;
    }
  }, [workspace.id]);

  useEffect(() => {
    if (!desktopEnabled) {
      setStatus(null);
      setFrame(null);
      return;
    }
    if (!backendAvailable) return;
    void refreshStatus();
    const timer = window.setInterval(() => void refreshStatus(), 2500);
    return () => window.clearInterval(timer);
  }, [backendAvailable, desktopEnabled, refreshStatus]);

  useEffect(() => {
    if (!status?.running || !status.ready) return;
    void refreshFrame();
    const timer = window.setInterval(() => void refreshFrame(), 200);
    return () => window.clearInterval(timer);
  }, [refreshFrame, status?.ready, status?.running]);

  useEffect(() => {
    if (!expanded) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setExpanded(false);
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [expanded]);

  async function start() {
    if (!applicationId || busy) return;
    setBusy(true);
    try {
      const next = await startAiWorkspace(workspace.id, applicationId, target);
      setStatus(next);
      setFrame(null);
      window.setTimeout(() => void refreshFrame(), 700);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function stop() {
    if (busy) return;
    setBusy(true);
    try {
      setStatus(await stopAiWorkspace(workspace.id));
      setFrame(null);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function clickFrame(event: React.MouseEvent<HTMLImageElement>) {
    if (!status?.running) return;
    const rect = event.currentTarget.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    const xRatio = (event.clientX - rect.left) / rect.width;
    const yRatio = (event.clientY - rect.top) / rect.height;
    try {
      await aiWorkspaceAction(workspace.id, "click", { xRatio, yRatio, clickCount: event.detail >= 2 ? 2 : 1 });
      window.setTimeout(() => void refreshFrame(), 100);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    }
  }

  if (!desktopEnabled) {
    return (
      <section className="ai-workspace-panel disabled">
        <div className="ai-workspace-heading">
          <div><strong>AI Workspace</strong><span>Isolated app screen</span></div>
          <small>Enable Desktop to use</small>
        </div>
        <p>Desktop permission is required before RepoTunnel can start an isolated GUI session.</p>
      </section>
    );
  }

  if (!backendAvailable) {
    return (
      <section className="ai-workspace-panel disabled">
        <div className="ai-workspace-heading">
          <div><strong>AI Workspace</strong><span>Isolated app screen</span></div>
          <small>Restart required</small>
        </div>
        <p>This RepoTunnel window is using an older backend. Restart the matching source build to enable AI Workspace.</p>
      </section>
    );
  }

  return (
    <section className={`ai-workspace-panel ${status?.running ? "running" : ""}`}>
      <div className="ai-workspace-heading">
        <div>
          <strong>AI Workspace</strong>
          <span>{status?.running ? `${status.applicationName ?? "Application"} · isolated display` : "Private virtual desktop for ChatGPT"}</span>
        </div>
        {status?.running ? (
          <button className="secondary-button" type="button" disabled={busy} onClick={() => void stop()}>
            {busy ? "Stopping…" : "Stop"}
          </button>
        ) : null}
      </div>

      {!status?.running ? (
        <div className="ai-workspace-start-row">
          <select aria-label="AI Workspace application" value={applicationId} onChange={(event) => setApplicationId(event.target.value)}>
            {productivityFamilies.map((family) => (
              <optgroup key={family.id} label={family.name}>
                {family.detectedMembers.map((member) => (
                  <option key={member.id} value={member.id}>{member.label}</option>
                ))}
              </optgroup>
            ))}
            {otherSelectableApps.length > 0 ? (
              <optgroup label="Other applications">
                {otherSelectableApps.map((application) => (
                  <option key={application.id} value={application.id}>{application.name}</option>
                ))}
              </optgroup>
            ) : null}
          </select>
          <input
            aria-label="AI Workspace project file or folder"
            value={target}
            onChange={(event) => setTarget(event.target.value)}
            placeholder="Project file or folder (optional)"
          />
          <button className="primary-button" type="button" disabled={!applicationId || busy} onClick={() => void start()}>
            {busy ? "Starting…" : "Start AI Workspace"}
          </button>
        </div>
      ) : (
        <>
          <div className="ai-workspace-meta">
            <span className="ai-workspace-live-dot" aria-hidden="true" />
            <span>AI control isolated from your desktop</span>
            <span className="ai-workspace-quality">Sharp · 1440×900 PNG</span>
            {frame?.activeTitle ? <span>{frame.activeTitle}</span> : null}
          </div>
          <div className={`ai-workspace-screen-shell ${expanded ? "expanded" : ""}`}>
            {frame?.dataBase64 ? (
              <img
                className="ai-workspace-screen"
                src={`data:${frame.mimeType};base64,${frame.dataBase64}`}
                alt={`Live ${status.applicationName ?? "AI Workspace"} screen`}
                draggable={false}
                onClick={(event) => void clickFrame(event)}
              />
            ) : (
              <div className="ai-workspace-screen-placeholder">
                <span>Starting isolated display…</span>
              </div>
            )}
            <button
              className="ai-workspace-expand-button"
              type="button"
              onClick={(event) => {
                event.stopPropagation();
                setExpanded((value) => !value);
              }}
            >
              {expanded ? "Exit full view" : "Expand"}
            </button>
          </div>
          <p className="ai-workspace-footnote">Your normal mouse, keyboard and active applications are not used by this session.</p>
        </>
      )}
    </section>
  );
}

export default AIWorkspacePanel;
