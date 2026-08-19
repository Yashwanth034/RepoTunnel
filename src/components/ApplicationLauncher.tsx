import { useCallback, useEffect, useMemo, useState } from "react";
import {
  approveLaunchAction,
  launchApplication,
  listLaunchHistory,
  listLaunchableApplications,
  openUrl,
  openWorkspacePath,
  rejectLaunchAction,
} from "../lib/backend";
import type { LaunchActionRecord, LaunchActionStatus, LaunchApplication, Workspace } from "../types";

type ApplicationLauncherProps = {
  workspace: Workspace | null;
  gatewayRunning: boolean;
  onError: (message: string) => void;
};

const launchStatusLabels: Record<LaunchActionStatus, string> = {
  pending: "Pending approval",
  launched: "Launched",
  failed: "Failed",
  rejected: "Rejected",
};

function actionLabel(action: LaunchActionRecord): string {
  if (action.kind === "url") return "Open URL";
  if (action.kind === "workspacePath") return "Open path";
  return "Launch app";
}

function launchEnabled(workspace: Workspace | null): boolean {
  if (!workspace) return false;
  return workspace.changePolicy === "automatic" || workspace.commandPolicy !== "disabled";
}

function ApplicationLauncher({ workspace, gatewayRunning, onError }: ApplicationLauncherProps) {
  const [applications, setApplications] = useState<LaunchApplication[]>([]);
  const [history, setHistory] = useState<LaunchActionRecord[]>([]);
  const [url, setUrl] = useState("");
  const [browserId, setBrowserId] = useState("");
  const [relativePath, setRelativePath] = useState("");
  const [pathApplicationId, setPathApplicationId] = useState("");
  const [busyId, setBusyId] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const browsers = useMemo(
    () => applications.filter((application) => application.supportsUrls),
    [applications],
  );
  const pathApplications = useMemo(
    () => applications.filter((application) => application.supportsPaths),
    [applications],
  );
  const workspaceHistory = useMemo(
    () => history.filter((action) => !workspace || action.workspaceId === workspace.id),
    [history, workspace],
  );

  const refresh = useCallback(async () => {
    if (!workspace) {
      setApplications([]);
      setHistory([]);
      return;
    }
    setLoading(true);
    try {
      const [availableApplications, actions] = await Promise.all([
        listLaunchableApplications(workspace.id),
        listLaunchHistory(workspace.id, 60),
      ]);
      setApplications(availableApplications);
      setHistory(actions);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setLoading(false);
    }
  }, [onError, workspace]);

  useEffect(() => {
    refresh().catch(() => undefined);
  }, [refresh]);

  useEffect(() => {
    if (!workspace) return;
    const timer = window.setInterval(() => {
      listLaunchHistory(workspace.id, 60)
        .then(setHistory)
        .catch(() => undefined);
    }, gatewayRunning ? 3000 : 6000);
    return () => window.clearInterval(timer);
  }, [gatewayRunning, workspace]);

  async function submitUrl() {
    if (!workspace || !url.trim()) return;
    setBusyId("open-url");
    try {
      await openUrl(workspace.id, url.trim(), browserId || undefined);
      setUrl("");
      setHistory(await listLaunchHistory(workspace.id, 60));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function submitPath() {
    if (!workspace) return;
    setBusyId("open-path");
    try {
      await openWorkspacePath(workspace.id, relativePath.trim(), pathApplicationId || undefined);
      setHistory(await listLaunchHistory(workspace.id, 60));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function submitApplication(application: LaunchApplication) {
    if (!workspace) return;
    setBusyId(application.id);
    try {
      await launchApplication(workspace.id, application.id);
      setHistory(await listLaunchHistory(workspace.id, 60));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function actOnPending(action: LaunchActionRecord, operation: "approve" | "reject") {
    setBusyId(action.id);
    try {
      if (operation === "approve") {
        await approveLaunchAction(action.id);
      } else {
        await rejectLaunchAction(action.id);
      }
      if (workspace) setHistory(await listLaunchHistory(workspace.id, 60));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  const enabled = launchEnabled(workspace);
  const automatic = workspace?.changePolicy === "automatic" || workspace?.commandPolicy === "automatic";

  return (
    <div className="application-launcher-section">
      <div className="command-history-title launcher-heading">
        <div>
          <strong>Applications & links</strong>
          <span>Open localhost, project files and approved desktop applications without leaving RepoTunnel.</span>
        </div>
        <button className="secondary-button" type="button" disabled={!workspace || loading} onClick={() => refresh()}>
          {loading ? "Refreshing…" : "Refresh apps"}
        </button>
      </div>

      {!workspace ? (
        <div className="command-empty"><p>Add or select a project to use application launching.</p></div>
      ) : !enabled ? (
        <div className="command-empty"><p>Application launching is disabled while this project&apos;s command policy is Disabled.</p></div>
      ) : (
        <>
          <div className="launcher-action-grid">
            <div className="launcher-action-card">
              <div className="launcher-card-heading">
                <strong>Open URL</strong>
                <span>{automatic ? "Opens immediately" : "Requires review"}</span>
              </div>
              <input
                aria-label="URL to open"
                value={url}
                onChange={(event) => setUrl(event.target.value)}
                placeholder="http://localhost:5173"
              />
              <div className="launcher-form-footer">
                <select aria-label="Browser for URL" value={browserId} onChange={(event) => setBrowserId(event.target.value)}>
                  <option value="">Default browser</option>
                  {browsers.map((application) => (
                    <option key={application.id} value={application.id}>{application.name}</option>
                  ))}
                </select>
                <button className="primary-button" type="button" disabled={!url.trim() || busyId !== null} onClick={submitUrl}>
                  {busyId === "open-url" ? "Opening…" : automatic ? "Open" : "Request open"}
                </button>
              </div>
            </div>

            <div className="launcher-action-card">
              <div className="launcher-card-heading">
                <strong>Open project path</strong>
                <span>Blank path opens the project folder</span>
              </div>
              <input
                aria-label="Workspace path to open"
                value={relativePath}
                onChange={(event) => setRelativePath(event.target.value)}
                placeholder="dist/report.html or leave blank for project root"
              />
              <div className="launcher-form-footer">
                <select aria-label="Application for workspace path" value={pathApplicationId} onChange={(event) => setPathApplicationId(event.target.value)}>
                  <option value="">Default application</option>
                  {pathApplications.map((application) => (
                    <option key={application.id} value={application.id}>{application.name}</option>
                  ))}
                </select>
                <button className="primary-button" type="button" disabled={busyId !== null} onClick={submitPath}>
                  {busyId === "open-path" ? "Opening…" : automatic ? "Open path" : "Request open"}
                </button>
              </div>
            </div>
          </div>

          <div className="launcher-apps">
            <div className="launcher-subheading">
              <strong>Allowed applications</strong>
              <span>{applications.length} detected</span>
            </div>
            {applications.length === 0 ? (
              <div className="command-empty"><p>No supported desktop applications were detected on PATH.</p></div>
            ) : (
              <div className="launcher-app-grid">
                {applications.map((application) => (
                  <button
                    key={application.id}
                    className="launcher-app-button"
                    type="button"
                    disabled={busyId !== null}
                    onClick={() => submitApplication(application)}
                    title={application.executable}
                  >
                    <span>{application.name}</span>
                    <small>{application.category}</small>
                  </button>
                ))}
              </div>
            )}
          </div>

          <div className="launcher-history">
            <div className="launcher-subheading">
              <strong>Recent launch activity</strong>
              <span>{workspaceHistory.length} records</span>
            </div>
            {workspaceHistory.length === 0 ? (
              <div className="command-empty"><p>No applications or links have been opened for this project yet.</p></div>
            ) : (
              <div className="launcher-history-list">
                {workspaceHistory.slice(0, 12).map((action) => (
                  <article className={`launcher-history-record ${action.status}`} key={action.id}>
                    <div className="launcher-history-copy">
                      <div className="command-record-meta">
                        <span className={`change-status ${action.status}`}>{launchStatusLabels[action.status]}</span>
                        <span>{actionLabel(action)}</span>
                        {action.applicationName ? <span>{action.applicationName}</span> : <span>System default</span>}
                        {action.pid !== null ? <span>PID {action.pid}</span> : null}
                      </div>
                      <strong>{action.target}</strong>
                      {action.error ? <p className="change-error">{action.error}</p> : null}
                    </div>
                    {action.status === "pending" ? (
                      <div className="managed-process-actions">
                        <button className="secondary-button reject-button" type="button" disabled={busyId !== null} onClick={() => actOnPending(action, "reject")}>Reject</button>
                        <button className="primary-button" type="button" disabled={busyId !== null} onClick={() => actOnPending(action, "approve")}>{busyId === action.id ? "Opening…" : "Accept & open"}</button>
                      </div>
                    ) : null}
                  </article>
                ))}
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}

export default ApplicationLauncher;
