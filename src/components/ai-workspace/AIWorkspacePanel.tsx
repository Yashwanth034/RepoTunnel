import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  aiWorkspaceAction,
  getAiWorkspaceFrame,
  getAiWorkspaceStatus,
  getVideoProjectRecording,
  listVideoProjects,
  startVideoProjectRecording,
  stopVideoProjectRecording,
  listAiWorkspaceRecoveryEvidence,
  queueAiWorkspaceRecoveryForReview,
  startAiWorkspace,
  stopAiWorkspace,
} from "../../lib/backend";
import { detectedProductivityFamilies, isProductivityApplication } from "../../lib/applicationFamilies";
import type { AiWorkspaceFrame, AiWorkspaceRecoveryRecord, AiWorkspaceStatus, LaunchApplication, VideoProductionProject, VideoRecordingStatus, Workspace } from "../../types";

type RecoveryEvidenceReviewProps = {
  workspaceId: string;
  count: number;
  onError: (message: string) => void;
};

function RecoveryEvidenceReview({ workspaceId, count, onError }: RecoveryEvidenceReviewProps) {
  const [records, setRecords] = useState<AiWorkspaceRecoveryRecord[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmedReceipt, setConfirmedReceipt] = useState("");
  const [queuedReview, setQueuedReview] = useState<Record<string, number>>({});

  async function queueForReview(receiptId: string) {
    if (confirmedReceipt !== receiptId || busy) return;
    setBusy(true);
    try {
      const ids = await queueAiWorkspaceRecoveryForReview(workspaceId, receiptId);
      setQueuedReview((current) => ({ ...current, [receiptId]: ids.length }));
      setConfirmedReceipt("");
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  if (!count) return null;

  async function review() {
    if (records) {
      setRecords(null);
      return;
    }
    setBusy(true);
    try {
      setRecords(await listAiWorkspaceRecoveryEvidence(workspaceId));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="ai-workspace-recovery-review">
      <p role="status">
        {count} AI Workspace task recovery evidence
        {count === 1 ? " record" : " records"} saved for review.
        Edits are never applied automatically. Evidence is retained after Change History approval for audit.
      </p>
      <button className="secondary-button" type="button" disabled={busy} onClick={() => void review()}>
        {busy ? "Inspecting saved edits…" : records ? "Hide recovery review" : "Review saved edits"}
      </button>
      {records ? (
        <div aria-label="AI Workspace read-only recovery review">
          {records.length === 0 ? (
            <p>No matching readable recovery records. Check the saved evidence manually.</p>
          ) : records.map((record) => (
            <div key={record.id} className="ai-workspace-recovery-record">
              <p><strong>Task {record.taskId}</strong> · App session {record.appSessionId}</p>
              <p>{record.reason}</p>
              <p>Saved {new Date(record.recordedAt).toLocaleString()}</p>
              {record.nativeGuiEvidenceStatus ? (
                <p role="status">
                  Native GUI project copy: {record.nativeGuiEvidenceStatus === "present-unverified"
                    ? "preserved on disk, but its contents are unverified. Manual recovery required."
                    : "missing, inaccessible, or unsafe to inspect. Automatic restoration remains disabled; manual review is required."}
                  {" "}No files are replayed and no private filesystem path is shown.
                </p>
              ) : null}
              {record.reviewError ? (
                <p role="alert">Cannot safely preview these edits: {record.reviewError}. The original evidence is preserved.</p>
              ) : (
                <>
                  <p>{record.changes.length} pending file change{record.changes.length === 1 ? "" : "s"} (read-only).</p>
                  <ul>
                    {record.changes.slice(0, 40).map((change, index) => (
                      <li key={record.id + ":" + index}>
                        {change.operation}: {change.paths.join(" → ")}
                      </li>
                    ))}
                  </ul>
                  {record.changes.length > 40 ? <p>Only the first 40 changes are shown here.</p> : null}
                  {queuedReview[record.id] !== undefined ? (
                    <p role="status">
                      {queuedReview[record.id]} Change History request{queuedReview[record.id] === 1 ? "" : "s"} queued.
                      Project files are unchanged. Open Change History to approve or reject each request.
                    </p>
                  ) : (
                    <>
                      <label>
                        <input
                          type="checkbox"
                          checked={confirmedReceipt === record.id}
                          onChange={(event) => setConfirmedReceipt(event.target.checked ? record.id : "")}
                          disabled={busy || record.changes.length === 0}
                        />
                        I have reviewed this receipt. Queue its edits for separate local approval; do not apply them now.
                      </label>
                      <button
                        className="secondary-button"
                        type="button"
                        disabled={busy || confirmedReceipt !== record.id || record.changes.length === 0}
                        onClick={() => void queueForReview(record.id)}
                      >
                        {busy ? "Queuing review requests…" : "Queue in Change History for approval"}
                      </button>
                    </>
                  )}
                </>
              )}
            </div>
          ))}
          <p>Review evidence only. No edit will be restored or approved automatically.</p>
        </div>
      ) : null}
    </div>
  );
}

// The screenshot is object-fit:contain inside a fixed-aspect-ratio image.
// Map clicks against the *rendered bitmap*, not its letterboxed element box.
// Blank bars are never AI input, and every returned ratio stays in [0,1).
function renderedFramePoint(
  rect: Pick<DOMRect, "left" | "top" | "width" | "height">,
  clientX: number,
  clientY: number,
  bitmapWidth: number,
  bitmapHeight: number,
): { xRatio: number; yRatio: number } | null {
  if (![rect.left, rect.top, rect.width, rect.height, clientX, clientY, bitmapWidth, bitmapHeight]
    .every(Number.isFinite)
    || rect.width <= 0 || rect.height <= 0 || bitmapWidth <= 0 || bitmapHeight <= 0) {
    return null;
  }
  const scale = Math.min(rect.width / bitmapWidth, rect.height / bitmapHeight);
  const shownWidth = bitmapWidth * scale;
  const shownHeight = bitmapHeight * scale;
  const shownLeft = rect.left + (rect.width - shownWidth) / 2;
  const shownTop = rect.top + (rect.height - shownHeight) / 2;
  const x = (clientX - shownLeft) / shownWidth;
  const y = (clientY - shownTop) / shownHeight;
  if (x < 0 || x >= 1 || y < 0 || y >= 1) return null;
  return { xRatio: x, yRatio: y };
}

function nativeElementTypeTextIsValid(value: string): boolean {
  return value.length > 0 && !value.includes("\0") && new TextEncoder().encode(value).length <= 32_768;
}

type AIWorkspacePanelProps = {
  workspace: Workspace;
  applications: LaunchApplication[];
  desktopEnabled: boolean;
  onError: (message: string) => void;
};

function AIWorkspacePanel({ workspace, applications, desktopEnabled, onError }: AIWorkspacePanelProps) {
  const [statusState, setStatus] = useState<AiWorkspaceStatus | null>(null);
  // Keep each captured frame bound to the exact workspace, desktop session,
  // and selected app that requested it. An old frame must never be interactive
  // after the user switches apps while a screenshot read is in flight.
  const [frameState, setFrameState] = useState<{ scope: string; revision: number; value: AiWorkspaceFrame } | null>(null);
  const [applicationId, setApplicationId] = useState("");
  const [selectedAppSessionId, setSelectedAppSessionId] = useState("");
  const [target, setTarget] = useState("");
  const [busy, setBusy] = useState(false);
  const [backendAvailable, setBackendAvailable] = useState(true);
  const [expanded, setExpanded] = useState(false);
  const [nativeTypeText, setNativeTypeText] = useState("");
  const [nativeTypeArmed, setNativeTypeArmed] = useState(false);
  const [nativeScrollArmed, setNativeScrollArmed] = useState(false);
  const [nativeScrollDirection, setNativeScrollDirection] = useState<"up" | "down" | "left" | "right">("down");
  const [videoProjects, setVideoProjects] = useState<VideoProductionProject[]>([]);
  const [recordingProjectId, setRecordingProjectId] = useState("");
  const [recording, setRecording] = useState<VideoRecordingStatus | null>(null);
  const [recordingBusy, setRecordingBusy] = useState(false);
  const recordingActionBusy = useRef(false);
  const frameBusy = useRef<{ scope: string; revision: number } | null>(null);
  const currentWorkspaceRef = useRef(workspace.id);
  const statusRevisionRef = useRef(0);
  const statusBusyRef = useRef<string | null>(null);
  if (currentWorkspaceRef.current !== workspace.id) {
    currentWorkspaceRef.current = workspace.id;
    statusRevisionRef.current++;
  }
  // Never render previous-project controls before the new status arrives.
  const status = statusState?.workspaceId === workspace.id ? statusState : null;
  const frameRevisionRef = useRef(0);
  const latestFrameScopeRef = useRef("");
  const frameScope = JSON.stringify([workspace.id, status?.sessionId ?? null, selectedAppSessionId, Boolean(status?.ready && status.running)]);
  if (latestFrameScopeRef.current !== frameScope) {
    latestFrameScopeRef.current = frameScope;
    frameRevisionRef.current++;
  }
  const frame = status?.running && status.ready && frameState?.scope === frameScope &&
    frameState.revision === frameRevisionRef.current ? frameState.value : null;
  const clearFrame = () => setFrameState(null);
  const nativeTypeBusy = useRef(false);
  const nativeScrollBusy = useRef(false);

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
  const selectedApplication = useMemo(
    () => status?.applications.find((application) => application.appSessionId === selectedAppSessionId) ?? null,
    [selectedAppSessionId, status?.applications],
  );
  const canStartAnotherApp =
    !status?.running || status.applicationCount < status.maxConcurrentApplications;

  useEffect(() => {
    setRecording(null);
    setRecordingProjectId("");
    setVideoProjects([]);
    clearFrame();
    setNativeScrollArmed(false);
    setNativeTypeArmed(false);
  }, [workspace.id]);

  // A one-shot typing target is specific to the selected session. Never
  // retain a drafted text action when the user switches native GUI apps.
  useEffect(() => {
    setNativeTypeArmed(false);
    setNativeScrollArmed(false);
    setNativeTypeText("");
  }, [selectedAppSessionId]);

  useEffect(() => {
    if (!desktopEnabled || !status?.running) {
      setNativeTypeArmed(false);
      setNativeScrollArmed(false);
    }
  }, [desktopEnabled, status?.running]);

  useEffect(() => {
    if (!applicationId || !selectableApps.some((application) => application.id === applicationId)) {
      const preferred = selectableApps.find((application) => application.id === "android-studio") ?? selectableApps[0];
      setApplicationId(preferred?.id ?? "");
    }
  }, [applicationId, selectableApps]);

  const refreshStatus = useCallback(async () => {
    const requestedWorkspace = workspace.id;
    if (statusBusyRef.current === requestedWorkspace) return null;
    const revision = ++statusRevisionRef.current;
    statusBusyRef.current = requestedWorkspace;
    const isCurrent = () =>
      currentWorkspaceRef.current === requestedWorkspace &&
      statusRevisionRef.current === revision;
    try {
      const next = await getAiWorkspaceStatus(requestedWorkspace);
      if (!isCurrent() || next.workspaceId !== requestedWorkspace) return null;
      if (typeof next.supported !== "boolean") {
        setBackendAvailable(false);
        setStatus(null);
        clearFrame();
        return null;
      }
      setBackendAvailable(true);
      setStatus(next);
      setSelectedAppSessionId((current) => {
        if (current && next.applications.some((application) => application.appSessionId === current)) {
          return current;
        }
        if (
          next.lastStartedAppSessionId &&
          next.applications.some(
            (application) => application.appSessionId === next.lastStartedAppSessionId,
          )
        ) {
          return next.lastStartedAppSessionId;
        }
        return next.applications[0]?.appSessionId ?? "";
      });
      if (!next.running) clearFrame();
      return next;
    } catch (error) {
      if (!isCurrent()) return null;
      const message = error instanceof Error ? error.message : String(error);
      if (message.includes("Command get_ai_workspace_status not found")) {
        setBackendAvailable(false);
        setStatus(null);
        clearFrame();
        return null;
      }
      onError(message);
      return null;
    } finally {
      if (statusBusyRef.current === requestedWorkspace) statusBusyRef.current = null;
    }
  }, [onError, workspace.id]);

  const refreshFrame = useCallback(async () => {
    if (!status?.ready || !status.sessionId || !selectedAppSessionId ||
        document.visibilityState !== "visible") return;
    const scope = frameScope;
    const revision = frameRevisionRef.current;
    if (frameBusy.current?.scope === scope && frameBusy.current.revision === revision) return;
    frameBusy.current = { scope, revision };
    try {
      const next = await getAiWorkspaceFrame(workspace.id, selectedAppSessionId, 1440);
      if (latestFrameScopeRef.current !== scope || frameRevisionRef.current !== revision ||
          next.sessionId !== status.sessionId) return;
      setFrameState({ scope, revision, value: next });
    } catch {
      // Status polling handles app exits/restarts without spamming the global error surface.
    } finally {
      if (frameBusy.current?.scope === scope && frameBusy.current.revision === revision) {
        frameBusy.current = null;
      }
    }
  }, [frameScope, selectedAppSessionId, status?.ready, status?.sessionId, workspace.id]);

  useEffect(() => {
    if (!backendAvailable) return;
    void refreshStatus();
    if (!desktopEnabled) {
      clearFrame();
      return;
    }
    const timer = window.setInterval(() => void refreshStatus(), 2500);
    return () => window.clearInterval(timer);
  }, [backendAvailable, desktopEnabled, refreshStatus]);

  useEffect(() => {
    if (!status?.running || !status.ready || !selectedAppSessionId) return;
    clearFrame();
    void refreshFrame();
    const timer = window.setInterval(() => void refreshFrame(), 700);
    return () => window.clearInterval(timer);
  }, [refreshFrame, selectedAppSessionId, status?.ready, status?.running]);

  // Video Projects are the only approved recording destinations. Loading
  // them here makes native app-only recording testable from AI Workspace
  // without silently writing video into the source repository or host home.
  useEffect(() => {
    if (!desktopEnabled || !status?.running) return;
    let cancelled = false;
    void listVideoProjects(workspace.id)
      .then((projects) => {
        if (cancelled) return;
        setVideoProjects(projects);
        setRecordingProjectId((current) =>
          current && projects.some((project) => project.id === current)
            ? current
            : projects[0]?.id ?? "",
        );
      })
      .catch((error) => {
        if (!cancelled) onError(error instanceof Error ? error.message : String(error));
      });
    return () => { cancelled = true; };
  }, [desktopEnabled, onError, status?.running, workspace.id]);

  useEffect(() => {
    if (!desktopEnabled || !status?.running) return;
    let cancelled = false;
    const refresh = async () => {
      try {
        const current = await getVideoProjectRecording(workspace.id);
        if (!cancelled) {
          setRecording(current);
          if (current) setRecordingProjectId(current.projectId);
        }
      } catch {
        // A transient status failure must not start or stop any recording.
      }
    };
    void refresh();
    const timer = window.setInterval(() => void refresh(), 3000);
    return () => { cancelled = true; window.clearInterval(timer); };
  }, [desktopEnabled, status?.running, workspace.id]);

  useEffect(() => {
    if (!expanded) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setExpanded(false);
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [expanded]);

  async function start() {
    if (!applicationId || busy || status?.supported !== true) return;
    setBusy(true);
    try {
      const next = await startAiWorkspace(workspace.id, applicationId, target);
      setStatus(next);
      const nextSessionId =
        next.lastStartedAppSessionId ?? next.applications.at(-1)?.appSessionId ?? "";
      setSelectedAppSessionId(nextSessionId);
      clearFrame();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function startRecording() {
    if (
      recordingActionBusy.current || !status?.ready || !selectedAppSessionId
      || !recordingProjectId || recording?.status === "recording"
    ) return;
    recordingActionBusy.current = true;
    setRecordingBusy(true);
    try {
      const next = await startVideoProjectRecording(
        workspace.id, recordingProjectId, selectedAppSessionId,
      );
      if (next.appSessionId !== selectedAppSessionId || next.projectId !== recordingProjectId) {
        throw new Error("The recording backend returned a different app session or Video Project.");
      }
      setRecording(next);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      recordingActionBusy.current = false;
      setRecordingBusy(false);
    }
  }

  async function stopRecording() {
    if (recordingActionBusy.current || recording?.status !== "recording") return;
    recordingActionBusy.current = true;
    setRecordingBusy(true);
    try {
      setRecording(await stopVideoProjectRecording(workspace.id, recording.projectId));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      recordingActionBusy.current = false;
      setRecordingBusy(false);
    }
  }

  async function stop() {
    if (busy || !selectedAppSessionId || recordingBusy || recordingActionBusy.current) return;
    setBusy(true);
    try {
      if (recording?.status === "recording" && recording.appSessionId === selectedAppSessionId) {
        const stopped = await stopVideoProjectRecording(workspace.id, recording.projectId);
        setRecording(stopped);
      }
      const next = await stopAiWorkspace(workspace.id, selectedAppSessionId);
      setStatus(next);
      setSelectedAppSessionId(next.applications[0]?.appSessionId ?? "");
      clearFrame();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function clickFrame(event: React.MouseEvent<HTMLImageElement>) {
    if (!status?.running || !status.ready || !selectedAppSessionId) return;
    // Native UIA/AX exposes a verified single semantic activation, not a
    // desktop-wide double-click gesture. Ignore repeat click events so an
    // intentional double-click never dispatches an unsupported second action.
    if (status.display === "native" && event.detail > 1) return;
    // A screenshot and the input derived from it must refer to the exact
    // same owned window. Without a signed frame window ID, refuse clicks.
    if (!frame?.windowId || frame.sessionId !== status.sessionId) return;
    const rect = event.currentTarget.getBoundingClientRect();
    const point = renderedFramePoint(
      rect, event.clientX, event.clientY, frame?.width ?? 0, frame?.height ?? 0,
    );
    if (!point) return;
    const typing = status.display === "native" && nativeTypeArmed;
    const scrolling = status.display === "native" && nativeScrollArmed && !typing;
    if (scrolling) {
      if (nativeScrollBusy.current) return;
      nativeScrollBusy.current = true;
    }
    if (typing) {
      if (nativeTypeBusy.current) return;
      if (!nativeElementTypeTextIsValid(nativeTypeText)) {
        onError("Native semantic typing requires 1–32,768 UTF-8 bytes without NUL.");
        return;
      }
      nativeTypeBusy.current = true;
    }
    try {
      if (typing) {
        // Native backend re-inspects the signed window and permits only one
        // verified, enabled, nonsensitive editable element at the pointer.
        // No global host keyboard events or shortcuts are ever emitted.
        await aiWorkspaceAction(workspace.id, selectedAppSessionId, "type", {
          windowId: frame.windowId,
          xRatio: point.xRatio,
          yRatio: point.yRatio,
          text: nativeTypeText,
        });
        setNativeTypeArmed(false);
        setNativeTypeText("");
      } else if (scrolling) {
        const deltaX = nativeScrollDirection === "left" ? -120 : nativeScrollDirection === "right" ? 120 : 0;
        const deltaY = nativeScrollDirection === "up" ? -120 : nativeScrollDirection === "down" ? 120 : 0;
        await aiWorkspaceAction(workspace.id, selectedAppSessionId, "scroll", {
          windowId: frame.windowId,
          xRatio: point.xRatio,
          yRatio: point.yRatio,
          deltaX,
          deltaY,
        });
        setNativeScrollArmed(false);
      } else {
        await aiWorkspaceAction(workspace.id, selectedAppSessionId, "click", {
          windowId: frame.windowId,
          xRatio: point.xRatio,
          yRatio: point.yRatio,
          clickCount: status.display === "native" ? 1 : event.detail >= 2 ? 2 : 1,
        });
      }
      window.setTimeout(() => void refreshFrame(), 100);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      if (typing) nativeTypeBusy.current = false;
      if (scrolling) nativeScrollBusy.current = false;
    }
  }

  if (status?.supported === false) {
    return (
      <section className="ai-workspace-panel disabled">
        <div className="ai-workspace-heading">
          <div><strong>AI Workspace</strong><span>Isolated app screen</span></div>
          <small>Unavailable on this platform</small>
        </div>
        <p>{status.unsupportedReason ?? "AI Workspace is not supported on this platform."}</p>
      </section>
    );
  }

  if (!desktopEnabled) {
    return (
      <section className="ai-workspace-panel disabled">
        <div className="ai-workspace-heading">
          <div><strong>AI Workspace</strong><span>Isolated app screen</span></div>
          <small>Enable Desktop to use</small>
        </div>
        <p>Desktop permission is required before RepoTunnel can start an isolated GUI session.</p>
        <RecoveryEvidenceReview
          key={workspace.id}
          workspaceId={workspace.id}
          count={status?.workspaceId === workspace.id ? status.recoveryEvidenceCount : 0}
          onError={onError}
        />
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
          <span>
            {status?.running
              ? `${status.applicationCount} app${status.applicationCount === 1 ? "" : "s"} · ${status.display === "native" ? "experimental native app session" : "shared isolated display"}`
              : "App-scoped AI workspace for ChatGPT"}
          </span>
        </div>
        {status?.running && selectedAppSessionId ? (
          <button className="secondary-button" type="button" disabled={busy} onClick={() => void stop()}>
            {busy ? "Stopping…" : `Stop ${selectedApplication?.applicationName ?? "selected app"}`}
          </button>
        ) : null}
      </div>

      <RecoveryEvidenceReview
        key={workspace.id}
        workspaceId={workspace.id}
        count={status?.workspaceId === workspace.id ? status.recoveryEvidenceCount : 0}
        onError={onError}
      />

      {canStartAnotherApp ? (
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
          <button className="primary-button" type="button" disabled={!status || !applicationId || busy} onClick={() => void start()}>
            {busy ? "Starting…" : status?.running ? "Open application" : "Start AI Workspace"}
          </button>
        </div>
      ) : status?.running ? (
        <p>
          AI Workspace is at its fixed application-session limit
          ({status.applicationCount}/{status.maxConcurrentApplications}).
        </p>
      ) : null}

      {status?.running ? (
        <>
          <div className="ai-workspace-meta">
            <span className="ai-workspace-live-dot" aria-hidden="true" />
            <span>{status.display === "native"
              ? "Experimental native sandbox · not a separate desktop"
              : "AI control isolated from your desktop"}</span>
            <span className="ai-workspace-quality">
              {status.applicationCount}/{status.maxConcurrentApplications} app slots in use
            </span>
            {frame?.activeTitle ? <span>{frame.activeTitle}</span> : null}
          </div>

          {status.applications.length > 0 ? (
            <div className="ai-workspace-start-row">
              <select
                aria-label="AI Workspace active application"
                value={selectedAppSessionId}
                onChange={(event) => {
                  setSelectedAppSessionId(event.target.value);
                  clearFrame();
                }}
              >
                {status.applications.map((application) => (
                  <option key={application.appSessionId} value={application.appSessionId}>
                    {application.applicationName}
                    {application.primary ? " · primary" : ""}
                  </option>
                ))}
              </select>
              <span>
                {selectedApplication
                  ? `Session ${selectedApplication.appSessionId}`
                  : "Select an application"}
              </span>
            </div>
          ) : null}
          {status.display === "native" ? (
            <div className="ai-workspace-start-row" role="group" aria-label="Native signed text input">
              <input
                type="text"
                aria-label="Native text for one editable control"
                autoComplete="off"
                spellCheck={false}
                maxLength={32_768}
                value={nativeTypeText}
                onChange={(event) => setNativeTypeText(event.target.value)}
                placeholder="Text for a signed editable control"
              />
              <button
                className="secondary-button"
                type="button"
                aria-pressed={nativeTypeArmed}
                disabled={busy || !selectedAppSessionId || (!nativeTypeArmed && !nativeElementTypeTextIsValid(nativeTypeText))}
                onClick={() => {
                  setNativeScrollArmed(false);
                  setNativeTypeArmed((armed) => !armed);
                }}
              >
                {nativeTypeArmed ? "Cancel typing" : "Type at next click"}
              </button>
              <span role="status">
                {nativeTypeArmed
                  ? "Click the exact editable control in the captured app window. Password fields are blocked."
                  : "Text stays local until you arm typing and click an owned editable control."}
              </span>
            </div>
          ) : null}
          {status.display === "native" ? (
            <div className="ai-workspace-start-row" role="group" aria-label="Native signed control scrolling">
              <select
                aria-label="Native scroll direction"
                value={nativeScrollDirection}
                disabled={busy || nativeScrollBusy.current}
                onChange={(event) =>
                  setNativeScrollDirection(event.target.value as "up" | "down" | "left" | "right")
                }
              >
                <option value="down">Scroll down</option>
                <option value="up">Scroll up</option>
                <option value="right">Scroll right</option>
                <option value="left">Scroll left</option>
              </select>
              <button
                className="secondary-button"
                type="button"
                aria-pressed={nativeScrollArmed}
                disabled={busy || !selectedAppSessionId || !status.ready}
                onClick={() => {
                  setNativeTypeArmed(false);
                  setNativeScrollArmed((armed) => !armed);
                }}
              >
                {nativeScrollArmed ? "Cancel scrolling" : "Scroll at next click"}
              </button>
              <span role="status">
                {nativeScrollArmed
                  ? "Click an owned, signed app scrollbar to scroll one notch. No desktop-wide wheel events."
                  : "Scroll is limited to a current, enabled app-owned semantic scrollbar."}
              </span>
            </div>
          ) : null}
          <div className="ai-workspace-start-row" role="group" aria-label="AI Workspace app-only recording">
            <select
              aria-label="Recording Video Project"
              value={recordingProjectId}
              disabled={recordingBusy || recording?.status === "recording" || videoProjects.length === 0}
              onChange={(event) => setRecordingProjectId(event.target.value)}
            >
              {videoProjects.length === 0 ? (
                <option value="">Create a Video Project first</option>
              ) : videoProjects.map((project) => (
                <option key={project.id} value={project.id}>{project.name}</option>
              ))}
            </select>
            {recording?.status === "recording" ? (
              <button
                className="secondary-button"
                type="button"
                disabled={recordingBusy}
                onClick={() => void stopRecording()}
              >
                {recordingBusy ? "Stopping recording…" : "Stop app recording"}
              </button>
            ) : (
              <button
                className="secondary-button"
                type="button"
                disabled={recordingBusy || !status.ready || !recordingProjectId || !selectedAppSessionId}
                onClick={() => void startRecording()}
              >
                {recordingBusy ? "Starting recording…" : "Record selected app"}
              </button>
            )}
            <span role="status">
              {recording?.status === "recording"
                ? `Recording app session ${recording.appSessionId} only; output is in its Video Project.`
                : recording
                  ? `${recording.status}: ${recording.message}`
                  : "Records only the selected app session into a Video Project. No camera or microphone."}
            </span>
          </div>
          <div className={`ai-workspace-screen-shell ${expanded ? "expanded" : ""}`}>
            {frame?.dataBase64 ? (
              <img
                className="ai-workspace-screen"
                src={`data:${frame.mimeType};base64,${frame.dataBase64}`}
                alt={`Live ${selectedApplication?.applicationName ?? "AI Workspace"} screen`}
                draggable={false}
                onClick={(event) => void clickFrame(event)}
              />
            ) : (
              <div className="ai-workspace-screen-placeholder">
                <span>
                  {selectedAppSessionId
                    ? "Loading selected application…"
                    : "Select an application session"}
                </span>
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
          <p className="ai-workspace-footnote">
            {status.display === "native"
              ? "Experimental native sandbox: this is not a separate virtual desktop. Screenshot clicks, typing and one-notch scrolling are scoped to signed app-owned controls."
              : "Multiple bounded app sessions share this virtual desktop. Preview and input are scoped to the selected app session."}
          </p>
        </>
      ) : null}
    </section>
  );
}

export default AIWorkspacePanel;
