import { useCallback, useEffect, useMemo, useState } from "react";
import {
  approveLaunchAction,
  cancelGithubConnection,
  connectGithub,
  disconnectGithub,
  getDesktopControlEnabled,
  getGithubConnectionStatus,
  launchApplication,
  listDeepIntegrations,
  listLaunchHistory,
  listLaunchableApplications,
  openUrl,
  openWorkspacePath,
  rejectLaunchAction,
  setDeepIntegrationEnabled,
  setDesktopControlEnabled,
} from "../lib/backend";
import { detectedProductivityFamilies, isProductivityApplication } from "../lib/applicationFamilies";
import type { DeepIntegration, GithubConnectionStatus, LaunchActionRecord, LaunchActionStatus, LaunchApplication, Workspace } from "../types";
import AIWorkspacePanel from "./ai-workspace/AIWorkspacePanel";
import ConfirmationDialog from "./ConfirmationDialog";

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
  const [integrations, setIntegrations] = useState<DeepIntegration[]>([]);
  const [desktopEnabled, setDesktopEnabled] = useState(false);
  const [github, setGithub] = useState<GithubConnectionStatus | null>(null);
  const [githubBusy, setGithubBusy] = useState(false);
  const [confirmGithubDisconnect, setConfirmGithubDisconnect] = useState(false);
  const [history, setHistory] = useState<LaunchActionRecord[]>([]);
  const [url, setUrl] = useState("");
  const [browserId, setBrowserId] = useState("");
  const [relativePath, setRelativePath] = useState("");
  const [pathApplicationId, setPathApplicationId] = useState("");
  const [busyId, setBusyId] = useState<string | null>(null);
  const [integrationBusyId, setIntegrationBusyId] = useState<string | null>(null);
  const [desktopBusyId, setDesktopBusyId] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const browsers = useMemo(
    () => applications.filter((application) => application.supportsUrls),
    [applications],
  );
  const pathApplications = useMemo(
    () => applications.filter((application) => application.supportsPaths),
    [applications],
  );
  const productivityFamilies = useMemo(
    () => detectedProductivityFamilies(applications),
    [applications],
  );
  const standaloneApplications = useMemo(
    () => applications.filter((application) => !isProductivityApplication(application)),
    [applications],
  );
  const workspaceHistory = useMemo(
    () => history.filter((action) => !workspace || action.workspaceId === workspace.id),
    [history, workspace],
  );

  const refresh = useCallback(async () => {
    if (!workspace) {
      setApplications([]);
      setIntegrations([]);
      setDesktopEnabled(false);
      setGithub(null);
      setHistory([]);
      return;
    }

    const reportError = (error: unknown) => {
      onError(error instanceof Error ? error.message : String(error));
    };

    setLoading(true);
    const applicationsTask = listLaunchableApplications(workspace.id)
      .then(setApplications)
      .catch(reportError);
    const integrationsTask = listDeepIntegrations(workspace.id)
      .then(setIntegrations)
      .catch(reportError);

    void getDesktopControlEnabled(workspace.id)
      .then(setDesktopEnabled)
      .catch(reportError);
    void getGithubConnectionStatus()
      .then(setGithub)
      .catch(reportError);
    void listLaunchHistory(workspace.id, 60)
      .then(setHistory)
      .catch(reportError);

    try {
      await Promise.all([applicationsTask, integrationsTask]);
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

  useEffect(() => {
    if (!github?.connecting) return;
    const timer = window.setInterval(() => {
      getGithubConnectionStatus()
        .then((next) => {
          setGithub(next);
          if (!next.connecting && !next.connected && next.message) onError(next.message);
          if (next.connected && next.message) onError(next.message);
        })
        .catch((error) => onError(error instanceof Error ? error.message : String(error)));
    }, 900);
    return () => window.clearInterval(timer);
  }, [github?.connecting, onError]);

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

  async function handleGithubClick() {
    if (githubBusy || github?.connecting) return;
    if (github?.connected) {
      setConfirmGithubDisconnect(true);
      return;
    }

    setGithubBusy(true);
    try {
      const next = await connectGithub();
      setGithub(next);
      if (!next.connecting && !next.connected && next.message) onError(next.message);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
      setGithub(await getGithubConnectionStatus().catch(() => github));
    } finally {
      setGithubBusy(false);
    }
  }

  async function cancelGithubConnectionAction() {
    if (githubBusy) return;
    setGithubBusy(true);
    try {
      setGithub(await cancelGithubConnection());
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setGithubBusy(false);
    }
  }

  async function copyGithubDeviceCode() {
    const code = github?.deviceCode;
    if (!code) return;
    try {
      await navigator.clipboard.writeText(code);
    } catch {
      const input = document.createElement("textarea");
      input.value = code;
      input.setAttribute("readonly", "");
      input.style.position = "fixed";
      input.style.opacity = "0";
      document.body.appendChild(input);
      input.select();
      const copied = document.execCommand("copy");
      input.remove();
      if (!copied) onError("Could not copy the GitHub code automatically. Select the code and copy it manually.");
    }
  }

  async function confirmGithubDisconnectAction() {
    if (githubBusy) return;
    setGithubBusy(true);
    try {
      setGithub(await disconnectGithub());
      setConfirmGithubDisconnect(false);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setGithubBusy(false);
    }
  }

  async function toggleIntegration(integration: DeepIntegration) {
    if (!workspace || !integration.available || integrationBusyId) return;
    setIntegrationBusyId(integration.id);
    try {
      setIntegrations(await setDeepIntegrationEnabled(workspace.id, integration.id, !integration.enabled));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setIntegrationBusyId(null);
    }
  }

  async function toggleDesktopControl() {
    if (!workspace || desktopBusyId) return;
    setDesktopBusyId("desktop");
    try {
      setDesktopEnabled(await setDesktopControlEnabled(workspace.id, !desktopEnabled));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setDesktopBusyId(null);
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

          <div className="launcher-integration-row" aria-label="AI application integrations">
            {integrations.map((integration) => (
              <button
                key={integration.id}
                type="button"
                className={`launcher-integration-name ${integration.enabled ? "enabled" : ""} ${!integration.available ? "unavailable" : ""}`}
                disabled={!integration.available || integrationBusyId !== null}
                onClick={() => void toggleIntegration(integration)}
                title={integration.message ?? integration.name}
                aria-pressed={integration.enabled}
              >
                <i aria-hidden="true" />
                <span>{integration.name}</span>
              </button>
            ))}
            <button
              type="button"
              className={`launcher-integration-name ${desktopEnabled ? "enabled" : ""}`}
              disabled={desktopBusyId !== null}
              onClick={() => void toggleDesktopControl()}
              title={desktopEnabled
                ? "ChatGPT desktop control enabled for all approved projects"
                : "Allow ChatGPT to inspect and control desktop applications for all approved projects"}
              aria-pressed={desktopEnabled}
            >
              <i aria-hidden="true" />
              <span>Desktop</span>
            </button>
          </div>

          <AIWorkspacePanel
            workspace={workspace}
            applications={applications}
            desktopEnabled={desktopEnabled}
            onError={onError}
          />

          <div className="launcher-apps">
            <div className="launcher-subheading">
              <strong>Allowed applications</strong>
              <span>{applications.length + (github?.available ? 1 : 0)} detected</span>
            </div>
            {applications.length === 0 && github?.available === false ? (
              <div className="command-empty"><p>No supported desktop applications were detected on PATH.</p></div>
            ) : (
              <div className="launcher-app-grid">
                {productivityFamilies.map((family) => (
                  <section className="launcher-app-family" key={family.id} aria-label={family.name}>
                    <div className="launcher-app-family-heading">
                      <strong>{family.name}</strong>
                      <small>{family.description}</small>
                    </div>
                    <div className="launcher-app-family-actions">
                      {family.detectedMembers.map((member) => (
                        <button
                          key={member.id}
                          type="button"
                          disabled={busyId !== null}
                          onClick={() => submitApplication(member.application)}
                          title={member.application.executable}
                        >
                          {busyId === member.id ? "Opening…" : member.label}
                        </button>
                      ))}
                    </div>
                  </section>
                ))}
                {standaloneApplications.map((application) => (
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
                <button
                  className={`launcher-app-button github-connection-button ${github?.connected ? "connected" : ""}`}
                  type="button"
                  disabled={githubBusy || github?.connecting}
                  onClick={() => void handleGithubClick()}
                  title={github?.connected ? "Disconnect GitHub" : (github?.message ?? "Connect GitHub")}
                  aria-pressed={github?.connected ?? false}
                >
                  <span>GitHub</span>
                  <small>
                    {githubBusy || github?.connecting
                      ? "Connecting…"
                      : github?.connected
                        ? github.username ? `@${github.username} · Connected` : "Connected"
                        : github?.available === false ? "GitHub CLI required" : "Connect"}
                  </small>
                </button>
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

      {github?.connecting ? (
        <div className="dialog-backdrop" role="presentation">
          <section
            className="confirmation-dialog github-auth-dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="github-auth-title"
          >
            <div className="dialog-icon github-dialog-icon" aria-hidden="true">GH</div>
            <div className="dialog-copy">
              <h2 id="github-auth-title">Connect GitHub</h2>
              <p>
                GitHub is opening in your browser. Enter the one-time code below to connect this computer.
              </p>
              <button
                className="github-device-code"
                type="button"
                disabled={!github.deviceCode}
                onClick={() => void copyGithubDeviceCode()}
                title={github.deviceCode ? "Copy one-time GitHub code" : "Waiting for GitHub code"}
              >
                {github.deviceCode ?? "Preparing code…"}
                {github.deviceCode ? <small>Click to copy</small> : null}
              </button>
              <p className="github-auth-note">
                The code is only for this sign-in. RepoTunnel never gives your GitHub credential to the AI.
              </p>
            </div>
            <div className="dialog-actions">
              <button
                className="secondary-button"
                type="button"
                disabled={githubBusy}
                onClick={() => void cancelGithubConnectionAction()}
              >
                {githubBusy ? "Cancelling…" : "Cancel"}
              </button>
            </div>
          </section>
        </div>
      ) : null}

      {confirmGithubDisconnect ? (
        <ConfirmationDialog
          title="Disconnect GitHub?"
          message="RepoTunnel and connected AIs will stop using this GitHub connection until you connect it again."
          confirmLabel="Disconnect"
          busy={githubBusy}
          busyLabel="Disconnecting…"
          onCancel={() => !githubBusy && setConfirmGithubDisconnect(false)}
          onConfirm={() => void confirmGithubDisconnectAction()}
        />
      ) : null}
    </div>
  );
}

export default ApplicationLauncher;
