import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  approveLaunchAction,
  cancelGithubConnection,
  connectGithub,
  disconnectGithub,
  getDesktopControlEnabled,
  getGmailAccessEnabled,
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
  setGmailAccessEnabled,
} from "../lib/backend";
import { detectedProductivityFamilies, isProductivityApplication } from "../lib/applicationFamilies";
import { reconcilePolledRecords } from "../lib/commandPolling";
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

// GitHub authentication is global to RepoTunnel, not scoped to the Commands
// component. Persist only non-secret display state so navigation and app restarts
// never flash a fake disconnected state while the backend refreshes the real
// GitHub CLI authentication in the background.
const GITHUB_STATUS_CACHE_KEY = "repotunnel.github.status.v1";

function loadCachedGithubStatus(): GithubConnectionStatus | null {
  try {
    const raw = window.localStorage.getItem(GITHUB_STATUS_CACHE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<GithubConnectionStatus>;
    if (typeof parsed.available !== "boolean" || typeof parsed.connected !== "boolean") return null;
    return {
      available: parsed.available,
      connected: parsed.connected,
      connecting: false,
      username: typeof parsed.username === "string" ? parsed.username : null,
      deviceCode: null,
      verificationUrl: null,
      message: null,
    };
  } catch {
    return null;
  }
}

function persistGithubStatus(next: GithubConnectionStatus) {
  if (next.connecting) return;
  try {
    window.localStorage.setItem(
      GITHUB_STATUS_CACHE_KEY,
      JSON.stringify({
        available: next.available,
        connected: next.connected,
        username: next.username,
      }),
    );
  } catch {
    // A storage failure must never block real GitHub authentication.
  }
}

let cachedGithubStatus: GithubConnectionStatus | null = loadCachedGithubStatus();

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
  const [gmailEnabled, setGmailEnabled] = useState(false);
  const [github, setGithub] = useState<GithubConnectionStatus | null>(() => cachedGithubStatus);
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
  const [gmailBusy, setGmailBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const currentWorkspaceIdRef = useRef(workspace?.id ?? null);
  const workspaceGenerationRef = useRef(0);
  // A -> B -> A still invalidates a delayed response from the first A.
  if (currentWorkspaceIdRef.current !== (workspace?.id ?? null)) {
    currentWorkspaceIdRef.current = workspace?.id ?? null;
    workspaceGenerationRef.current++;
  }
  const refreshRevisionRef = useRef(0);
  const historyRevisionRef = useRef(0);
  const visibleWorkspaceIdRef = useRef<string | null>(null);

  const rememberGithub = useCallback((next: GithubConnectionStatus) => {
    cachedGithubStatus = next;
    persistGithubStatus(next);
    setGithub(next);
  }, []);

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

  // Every history fetch carries a workspace and a monotonic revision.
  // A manual action always supersedes any slower background history poll.
  const readHistory = useCallback(async () => {
    const id = workspace?.id;
    if (!id) return;
    const revision = ++historyRevisionRef.current;
    const next = await listLaunchHistory(id, 60);
    if (currentWorkspaceIdRef.current !== id || revision !== historyRevisionRef.current) return;
    setHistory((current) => reconcilePolledRecords(current, next));
  }, [workspace?.id]);

  const refresh = useCallback(async () => {
    const revision = ++refreshRevisionRef.current;
    const id = workspace?.id;
    if (!id) {
      historyRevisionRef.current++;
      visibleWorkspaceIdRef.current = null;
      setApplications([]);
      setIntegrations([]);
      setDesktopEnabled(false);
      setGmailEnabled(false);
      setHistory([]);
      setIntegrationBusyId(null);
      setDesktopBusyId(null);
      setGmailBusy(false);
      setBusyId(null);
      setLoading(false);
      return;
    }

    const current = () => currentWorkspaceIdRef.current === id
      && revision === refreshRevisionRef.current;
    if (visibleWorkspaceIdRef.current !== id) {
      visibleWorkspaceIdRef.current = id;
      setApplications([]);
      setIntegrations([]);
      setDesktopEnabled(false);
      setGmailEnabled(false);
      setHistory([]);
      // Pending operations belong to the old project, not this one.
      setIntegrationBusyId(null);
      setDesktopBusyId(null);
      setGmailBusy(false);
      setBusyId(null);
    }
    const reportError = (error: unknown) => {
      if (current()) onError(error instanceof Error ? error.message : String(error));
    };

    setLoading(true);
    const applicationsTask = listLaunchableApplications(id)
      .then((next) => { if (current()) setApplications(next); })
      .catch(reportError);
    const integrationsTask = listDeepIntegrations(id)
      .then((next) => { if (current()) setIntegrations(next); })
      .catch(reportError);

    void getDesktopControlEnabled(id)
      .then((next) => { if (current()) setDesktopEnabled(next); })
      .catch(reportError);
    void getGmailAccessEnabled(id)
      .then((next) => { if (current()) setGmailEnabled(next); })
      .catch(reportError);
    // GitHub authorization is global, not scoped to this workspace.
    void getGithubConnectionStatus()
      .then(rememberGithub)
      .catch(reportError);
    void readHistory().catch(reportError);

    try {
      await Promise.all([applicationsTask, integrationsTask]);
    } finally {
      if (current()) setLoading(false);
    }
  }, [onError, rememberGithub, readHistory, workspace?.id]);

  useEffect(() => {
    void refresh();
    const onAppsChanged = () => void refresh();
    window.addEventListener("repotunnel:applications-updated", onAppsChanged);
    return () => {
      window.removeEventListener("repotunnel:applications-updated", onAppsChanged);
      refreshRevisionRef.current++;
      historyRevisionRef.current++;
    };
  }, [refresh]);

  useEffect(() => {
    if (!workspace) return;
    let active = true;
    let inFlight = false;
    const poll = async () => {
      if (!active || inFlight) return;
      inFlight = true;
      try {
        await readHistory();
      } catch {
        // Background launch-history checks are best-effort.
      } finally {
        inFlight = false;
      }
    };
    const timer = window.setInterval(() => void poll(), gatewayRunning ? 3000 : 6000);
    return () => {
      active = false;
      historyRevisionRef.current++;
      window.clearInterval(timer);
    };
  }, [gatewayRunning, readHistory, workspace?.id]);

  useEffect(() => {
    if (!github?.connecting) return;
    const timer = window.setInterval(() => {
      getGithubConnectionStatus()
        .then((next) => {
          rememberGithub(next);
          if (!next.connecting && !next.connected && next.message) onError(next.message);
          if (next.connected && next.message) onError(next.message);
        })
        .catch((error) => onError(error instanceof Error ? error.message : String(error)));
    }, 900);
    return () => window.clearInterval(timer);
  }, [github?.connecting, onError, rememberGithub]);

  async function submitUrl() {
    if (!workspace || !url.trim()) return;
    setBusyId("open-url");
    try {
      await openUrl(workspace.id, url.trim(), browserId || undefined);
      setUrl("");
      await readHistory();
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
      await readHistory();
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
      await readHistory();
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
      rememberGithub(next);
      if (!next.connecting && !next.connected && next.message) onError(next.message);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
      const refreshed = await getGithubConnectionStatus().catch(() => github);
      if (refreshed) rememberGithub(refreshed);
    } finally {
      setGithubBusy(false);
    }
  }

  async function cancelGithubConnectionAction() {
    if (githubBusy) return;
    setGithubBusy(true);
    try {
      rememberGithub(await cancelGithubConnection());
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
      rememberGithub(await disconnectGithub());
      setConfirmGithubDisconnect(false);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setGithubBusy(false);
    }
  }

  async function toggleIntegration(integration: DeepIntegration) {
    if (!workspace || !integration.available || integrationBusyId) return;
    const id = workspace.id;
    const generation = workspaceGenerationRef.current;
    const stillCurrent = () => currentWorkspaceIdRef.current === id
      && workspaceGenerationRef.current === generation;
    setIntegrationBusyId(integration.id);
    try {
      const next = await setDeepIntegrationEnabled(id, integration.id, !integration.enabled);
      if (stillCurrent()) setIntegrations(next);
    } catch (error) {
      if (stillCurrent()) onError(error instanceof Error ? error.message : String(error));
    } finally {
      if (stillCurrent()) setIntegrationBusyId(null);
    }
  }

  async function toggleDesktopControl() {
    if (!workspace || desktopBusyId) return;
    const id = workspace.id;
    const generation = workspaceGenerationRef.current;
    const stillCurrent = () => currentWorkspaceIdRef.current === id
      && workspaceGenerationRef.current === generation;
    setDesktopBusyId("desktop");
    try {
      const next = await setDesktopControlEnabled(id, !desktopEnabled);
      if (stillCurrent()) setDesktopEnabled(next);
    } catch (error) {
      if (stillCurrent()) onError(error instanceof Error ? error.message : String(error));
    } finally {
      if (stillCurrent()) setDesktopBusyId(null);
    }
  }

  async function toggleGmailAccess() {
    if (!workspace || gmailBusy) return;
    const id = workspace.id;
    const generation = workspaceGenerationRef.current;
    const stillCurrent = () => currentWorkspaceIdRef.current === id
      && workspaceGenerationRef.current === generation;
    setGmailBusy(true);
    try {
      const next = await setGmailAccessEnabled(id, !gmailEnabled);
      if (stillCurrent()) setGmailEnabled(next);
    } catch (error) {
      if (stillCurrent()) onError(error instanceof Error ? error.message : String(error));
    } finally {
      if (stillCurrent()) setGmailBusy(false);
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
      if (workspace) await readHistory();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  const enabled = launchEnabled(workspace);
  const automatic = workspace?.changePolicy === "automatic" || workspace?.commandPolicy === "automatic";
  // Render permission-gated controls fail-closed even during the single render
  // before the selected project's refresh effect clears the previous state.
  const permissionsReady = !!workspace && visibleWorkspaceIdRef.current === workspace.id;

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
            {(permissionsReady ? integrations : []).map((integration) => (
              <button
                key={integration.id}
                type="button"
                className={`launcher-integration-name ${integration.enabled ? "enabled" : ""} ${!integration.available ? "unavailable" : ""}`}
                disabled={!permissionsReady || !integration.available || integrationBusyId !== null}
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
              className={`launcher-integration-name ${permissionsReady && gmailEnabled ? "enabled" : ""}`}
              disabled={!permissionsReady || gmailBusy}
              onClick={() => void toggleGmailAccess()}
              title={gmailEnabled
                ? "Gmail and Google Sign-In access enabled for all approved projects"
                : "Allow AI browser workflows to reuse the managed Google session and read Gmail for sign-in and verification flows"}
              aria-pressed={permissionsReady && gmailEnabled}
            >
              <i aria-hidden="true" />
              <span>Gmail</span>
            </button>
            <button
              type="button"
              className={`launcher-integration-name ${permissionsReady && desktopEnabled ? "enabled" : ""}`}
              disabled={!permissionsReady || desktopBusyId !== null}
              onClick={() => void toggleDesktopControl()}
              title={desktopEnabled
                ? "ChatGPT desktop control enabled for all approved projects"
                : "Allow ChatGPT to inspect and control desktop applications for all approved projects"}
              aria-pressed={permissionsReady && desktopEnabled}
            >
              <i aria-hidden="true" />
              <span>Desktop</span>
            </button>
          </div>

          <div id="commands-ai-workspace" role="region" aria-label="AI Workspace" tabIndex={-1}>
            <AIWorkspacePanel
              workspace={workspace}
              applications={applications}
              desktopEnabled={permissionsReady && desktopEnabled}
              onError={onError}
            />
          </div>

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
                  disabled={github === null || githubBusy || github?.connecting}
                  onClick={() => void handleGithubClick()}
                  title={
                    github === null
                      ? "Checking GitHub connection"
                      : github.connected
                        ? "Disconnect GitHub"
                        : (github.message ?? "Connect GitHub")
                  }
                  aria-pressed={github?.connected ?? false}
                >
                  <span>GitHub</span>
                  <small>
                    {github === null
                      ? "Checking…"
                      : githubBusy || github.connecting
                        ? "Connecting…"
                        : github.connected
                          ? github.username ? `@${github.username} · Connected` : "Connected"
                          : github.available === false ? "GitHub CLI required" : "Connect"}
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
