import { useCallback, useEffect, useMemo, useState, type ChangeEvent, type MouseEvent } from "react";
import {
  approveBrowserAction,
  browserActivateTab,
  browserClick,
  browserCloseTab,
  browserInspectPage,
  browserNavigate,
  browserOpenTab,
  browserPickElement,
  browserReload,
  browserScroll,
  browserTakeScreenshot,
  browserType,
  getBrowserAutomationStatus,
  getBrowserDiagnostics,
  getBrowserVisualSelection,
  listAutomationBrowsers,
  listBrowserHistory,
  listBrowserTabs,
  rejectBrowserAction,
  startBrowserAutomation,
  stopBrowserAutomation,
} from "../lib/backend";
import type {
  BrowserActionKind,
  BrowserActionRecord,
  BrowserActionStatus,
  BrowserApplication,
  BrowserAutomationStatus,
  BrowserDiagnostics,
  BrowserPageInspection,
  BrowserScreenshot,
  BrowserTab,
  BrowserVisualSelection,
  Workspace,
} from "../types";

type BrowserAutomationProps = {
  workspace: Workspace | null;
  gatewayRunning: boolean;
  onError: (message: string) => void;
};

const statusLabels: Record<BrowserActionStatus, string> = {
  pending: "Pending approval",
  applied: "Applied",
  failed: "Failed",
  rejected: "Rejected",
};

const kindLabels: Record<BrowserActionKind, string> = {
  start: "Start browser",
  stop: "Stop browser",
  openTab: "Open tab",
  activateTab: "Activate tab",
  closeTab: "Close tab",
  navigate: "Navigate",
  click: "Click",
  type: "Type",
  scroll: "Scroll",
  reload: "Reload",
};

function formatTime(timestamp: number): string {
  if (!timestamp) return "";
  return new Date(timestamp).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function BrowserAutomation({ workspace, gatewayRunning, onError }: BrowserAutomationProps) {
  const [applications, setApplications] = useState<BrowserApplication[]>([]);
  const [status, setStatus] = useState<BrowserAutomationStatus | null>(null);
  const [tabs, setTabs] = useState<BrowserTab[]>([]);
  const [history, setHistory] = useState<BrowserActionRecord[]>([]);
  const [selectedApplicationId, setSelectedApplicationId] = useState("");
  const [selectedTabId, setSelectedTabId] = useState("");
  const [url, setUrl] = useState("http://localhost:5173");
  const [selector, setSelector] = useState("");
  const [typeText, setTypeText] = useState("");
  const [clearFirst, setClearFirst] = useState(true);
  const [inspection, setInspection] = useState<BrowserPageInspection | null>(null);
  const [screenshot, setScreenshot] = useState<BrowserScreenshot | null>(null);
  const [livePreview, setLivePreview] = useState(false);
  const [visualSelection, setVisualSelection] = useState<BrowserVisualSelection | null>(null);
  const [previewRefreshing, setPreviewRefreshing] = useState(false);
  const [fullPage, setFullPage] = useState(false);
  const [diagnostics, setDiagnostics] = useState<BrowserDiagnostics>({ consoleEntries: [], networkFailures: [] });
  const [busyId, setBusyId] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const automatic = workspace?.changePolicy === "automatic";
  const selectedTab = useMemo(
    () => tabs.find((tab) => tab.id === selectedTabId) ?? null,
    [selectedTabId, tabs],
  );
  const pending = useMemo(
    () => history.filter((action) => action.status === "pending"),
    [history],
  );

  const refresh = useCallback(async (quiet = false) => {
    if (!workspace) {
      setApplications([]);
      setStatus(null);
      setTabs([]);
      setHistory([]);
      setSelectedTabId("");
      setInspection(null);
      setScreenshot(null);
      setLivePreview(false);
      setVisualSelection(null);
      setDiagnostics({ consoleEntries: [], networkFailures: [] });
      return;
    }
    if (!quiet) setLoading(true);
    try {
      const [available, currentStatus, actions] = await Promise.all([
        listAutomationBrowsers(workspace.id),
        getBrowserAutomationStatus(workspace.id),
        listBrowserHistory(workspace.id, 60),
      ]);
      setApplications(available);
      setStatus(currentStatus);
      setHistory(actions);
      setSelectedApplicationId((current) => {
        if (current && available.some((browser) => browser.id === current)) return current;
        return currentStatus.browserId ?? available[0]?.id ?? "";
      });

      if (currentStatus.running) {
        const currentTabs = await listBrowserTabs(workspace.id);
        setTabs(currentTabs);
        setSelectedTabId((current) => {
          if (current && currentTabs.some((tab) => tab.id === current)) return current;
          return currentStatus.activeTabId && currentTabs.some((tab) => tab.id === currentStatus.activeTabId)
            ? currentStatus.activeTabId
            : currentTabs[0]?.id ?? "";
        });
      } else {
        setTabs([]);
        setSelectedTabId("");
        setInspection(null);
        setScreenshot(null);
        setLivePreview(false);
        setVisualSelection(null);
        setDiagnostics({ consoleEntries: [], networkFailures: [] });
      }
    } catch (error) {
      if (!quiet) onError(error instanceof Error ? error.message : String(error));
    } finally {
      if (!quiet) setLoading(false);
    }
  }, [onError, workspace]);

  useEffect(() => {
    refresh().catch(() => undefined);
  }, [refresh]);

  useEffect(() => {
    if (!workspace) return;
    const timer = window.setInterval(() => {
      refresh(true).catch(() => undefined);
    }, gatewayRunning ? 2500 : 4500);
    return () => window.clearInterval(timer);
  }, [gatewayRunning, refresh, workspace]);

  useEffect(() => {
    if (!workspace || !status?.running || !selectedTabId) return;
    const load = () => {
      getBrowserDiagnostics(workspace.id, selectedTabId, 40)
        .then(setDiagnostics)
        .catch(() => undefined);
    };
    load();
    const timer = window.setInterval(load, 2500);
    return () => window.clearInterval(timer);
  }, [selectedTabId, status?.running, workspace]);

  useEffect(() => {
    if (!workspace) return;
    getBrowserVisualSelection(workspace.id)
      .then(setVisualSelection)
      .catch(() => undefined);
  }, [workspace]);

  useEffect(() => {
    if (!livePreview || !workspace || !status?.running || !selectedTabId) return;
    let cancelled = false;

    const capturePreview = async () => {
      if (cancelled) return;
      setPreviewRefreshing(true);
      try {
        const next = await browserTakeScreenshot(workspace.id, selectedTabId, false);
        if (!cancelled) setScreenshot(next);
      } catch {
        // Background preview refresh is best-effort; normal browser status surfaces failures.
      } finally {
        if (!cancelled) setPreviewRefreshing(false);
      }
    };

    capturePreview().catch(() => undefined);
    const timer = window.setInterval(() => {
      capturePreview().catch(() => undefined);
    }, 1800);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [livePreview, selectedTabId, status?.running, workspace]);

  async function selectPreviewElement(event: MouseEvent<HTMLImageElement>) {
    if (!workspace || !selectedTabId || !screenshot || screenshot.fullPage) return;
    const rect = event.currentTarget.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return;
    const xRatio = Math.min(1, Math.max(0, (event.clientX - rect.left) / rect.width));
    const yRatio = Math.min(1, Math.max(0, (event.clientY - rect.top) / rect.height));
    setBusyId("visual-select");
    try {
      const selected = await browserPickElement(workspace.id, selectedTabId, xRatio, yRatio);
      setVisualSelection(selected);
      setSelector(selected.selector);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function perform(id: string, task: () => Promise<unknown>, clear?: () => void) {
    if (!workspace) return;
    setBusyId(id);
    try {
      await task();
      clear?.();
      await refresh(true);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function inspect() {
    if (!workspace || !selectedTabId) return;
    setBusyId("inspect");
    try {
      setInspection(await browserInspectPage(workspace.id, selectedTabId, selector.trim() || undefined, 12000));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function capture() {
    if (!workspace || !selectedTabId) return;
    setBusyId("screenshot");
    try {
      setScreenshot(await browserTakeScreenshot(workspace.id, selectedTabId, fullPage));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function actOnPending(action: BrowserActionRecord, operation: "approve" | "reject") {
    await perform(action.id, async () => {
      if (operation === "approve") await approveBrowserAction(action.id);
      else await rejectBrowserAction(action.id);
    });
  }

  return (
    <div className="browser-automation-section">
      <div className="command-history-title browser-heading">
        <div>
          <strong>Browser automation</strong>
          <span>Isolated Chrome DevTools session for localhost testing, DOM inspection and diagnostics.</span>
        </div>
        <div className="browser-heading-actions">
          {status?.running ? <span className="running-count">{status.browserName} · PID {status.pid}</span> : null}
          {pending.length > 0 ? <span className="pending-count">{pending.length} pending</span> : null}
          <button className="secondary-button" type="button" disabled={!workspace || loading} onClick={() => refresh()}>
            {loading ? "Refreshing…" : "Refresh"}
          </button>
        </div>
      </div>

      {!workspace ? (
        <div className="command-empty"><p>Add or select a project to use browser automation.</p></div>
      ) : !status?.available && !status?.running ? (
        <div className="command-empty"><p>{status?.message ?? "No supported automation browser is available."}</p></div>
      ) : (
        <>
          <div className="browser-session-bar">
            <div>
              <span className={`browser-session-dot ${status?.running ? "running" : "stopped"}`} />
              <strong>{status?.running ? "Automation session running" : "Automation session stopped"}</strong>
              <span>{automatic ? "AI Auto · browser actions run immediately" : "AI Review · browser actions require approval"}</span>
            </div>
            <div className="browser-session-actions">
              {!status?.running ? (
                <>
                  <select
                    aria-label="Automation browser"
                    value={selectedApplicationId}
                    onChange={(event: ChangeEvent<HTMLSelectElement>) => setSelectedApplicationId(event.target.value)}
                    disabled={busyId !== null}
                  >
                    {applications.map((browser) => <option key={browser.id} value={browser.id}>{browser.name}</option>)}
                  </select>
                  <button
                    className="primary-button"
                    type="button"
                    disabled={!selectedApplicationId || busyId !== null}
                    onClick={() => perform("start-browser", () => startBrowserAutomation(workspace.id, selectedApplicationId))}
                  >
                    {busyId === "start-browser" ? "Starting…" : automatic ? "Start browser" : "Request start"}
                  </button>
                </>
              ) : (
                <button
                  className="secondary-button reject-button"
                  type="button"
                  disabled={busyId !== null}
                  onClick={() => perform("stop-browser", () => stopBrowserAutomation(workspace.id))}
                >
                  {busyId === "stop-browser" ? "Stopping…" : automatic ? "Stop browser" : "Request stop"}
                </button>
              )}
            </div>
          </div>

          {status?.running ? (
            <>
              <div className="browser-address-row">
                <input
                  aria-label="Browser address"
                  value={url}
                  onChange={(event: ChangeEvent<HTMLInputElement>) => setUrl(event.target.value)}
                  placeholder="http://localhost:5173"
                />
                <button
                  className="secondary-button"
                  type="button"
                  disabled={!selectedTabId || !url.trim() || busyId !== null}
                  onClick={() => perform("navigate", () => browserNavigate(workspace.id, selectedTabId, url.trim()))}
                >
                  Navigate
                </button>
                <button
                  className="primary-button"
                  type="button"
                  disabled={!url.trim() || busyId !== null}
                  onClick={() => perform("open-tab", () => browserOpenTab(workspace.id, url.trim()))}
                >
                  New tab
                </button>
                <button
                  className="secondary-button"
                  type="button"
                  disabled={!selectedTabId || busyId !== null}
                  onClick={() => perform("reload", () => browserReload(workspace.id, selectedTabId))}
                >
                  Reload
                </button>
              </div>

              <div className="browser-workspace-grid">
                <div className="browser-tabs-card">
                  <div className="browser-card-heading"><strong>Tabs</strong><span>{tabs.length}</span></div>
                  {tabs.length === 0 ? <p>No page tabs are open.</p> : (
                    <div className="browser-tab-list">
                      {tabs.map((tab) => (
                        <div key={tab.id} className={`browser-tab-item ${selectedTabId === tab.id ? "selected" : ""}`}>
                          <button type="button" className="browser-tab-select" onClick={() => setSelectedTabId(tab.id)}>
                            <span className="browser-tab-title">{tab.title || "Untitled"}</span>
                            <span className="browser-tab-url">{tab.url}</span>
                          </button>
                          <div className="browser-tab-actions">
                            <button className="secondary-button" type="button" disabled={busyId !== null} onClick={() => perform(`activate-${tab.id}`, () => browserActivateTab(workspace.id, tab.id))}>Focus</button>
                            <button className="secondary-button reject-button" type="button" disabled={busyId !== null} onClick={() => perform(`close-${tab.id}`, () => browserCloseTab(workspace.id, tab.id))}>Close</button>
                          </div>
                        </div>
                      ))}
                    </div>
                  )}
                </div>

                <div className="browser-actions-card">
                  <div className="browser-card-heading"><strong>Page actions</strong><span>{selectedTab ? selectedTab.title : "Select a tab"}</span></div>
                  <input
                    aria-label="CSS selector"
                    value={selector}
                    onChange={(event: ChangeEvent<HTMLInputElement>) => setSelector(event.target.value)}
                    placeholder="CSS selector, e.g. button[type=submit]"
                  />
                  <div className="browser-action-button-row">
                    <button
                      className="secondary-button"
                      type="button"
                      disabled={!selectedTabId || !selector.trim() || busyId !== null}
                      onClick={() => perform("click", () => browserClick(workspace.id, selectedTabId, selector.trim()))}
                    >Click</button>
                    <button className="secondary-button" type="button" disabled={!selectedTabId || busyId !== null} onClick={() => perform("scroll-up", () => browserScroll(workspace.id, selectedTabId, 0, -600))}>↑ 600</button>
                    <button className="secondary-button" type="button" disabled={!selectedTabId || busyId !== null} onClick={() => perform("scroll-down", () => browserScroll(workspace.id, selectedTabId, 0, 600))}>↓ 600</button>
                    <button className="secondary-button" type="button" disabled={!selectedTabId || busyId !== null} onClick={() => inspect()}>{busyId === "inspect" ? "Inspecting…" : "Inspect"}</button>
                  </div>
                  <textarea
                    aria-label="Text to type in browser"
                    rows={3}
                    value={typeText}
                    onChange={(event: ChangeEvent<HTMLTextAreaElement>) => setTypeText(event.target.value)}
                    placeholder="Text to type into the selected element"
                  />
                  <div className="browser-type-footer">
                    <label><input type="checkbox" checked={clearFirst} onChange={(event: ChangeEvent<HTMLInputElement>) => setClearFirst(event.target.checked)} /> Clear existing value first</label>
                    <button
                      className="primary-button"
                      type="button"
                      disabled={!selectedTabId || !selector.trim() || busyId !== null}
                      onClick={() => perform("type", () => browserType(workspace.id, selectedTabId, selector.trim(), typeText, clearFirst))}
                    >Type</button>
                  </div>
                </div>
              </div>

              <div className="browser-observation-grid">
                <div className="browser-observation-card">
                  <div className="browser-card-heading">
                    <strong>Page inspection</strong>
                    <span>{inspection ? inspection.url : "DOM + visible text"}</span>
                  </div>
                  {inspection ? (
                    <>
                      <div className="browser-inspection-meta">
                        <span>{inspection.found ? inspection.tag ?? "Document" : "Selector not found"}</span>
                        {inspection.selector ? <code>{inspection.selector}</code> : null}
                      </div>
                      <pre className="browser-inspection-output">{inspection.text || "No visible text returned."}</pre>
                      <details><summary>HTML</summary><pre className="browser-inspection-output html">{inspection.html || "No HTML returned."}</pre></details>
                    </>
                  ) : <p>Inspect the selected tab or selector to read page content and DOM markup.</p>}
                </div>

                <div className="browser-observation-card browser-live-preview-card">
                  <div className="browser-card-heading">
                    <strong>Live Preview</strong>
                    <span>{livePreview ? (previewRefreshing ? "Refreshing…" : "Live") : screenshot ? `${Math.max(1, Math.round(screenshot.sizeBytes / 1024))} KB` : "Viewport preview"}</span>
                  </div>
                  <div className="browser-screenshot-actions">
                    <button
                      className={livePreview ? "primary-button" : "secondary-button"}
                      type="button"
                      disabled={!selectedTabId}
                      onClick={() => {
                        setFullPage(false);
                        setLivePreview((current) => !current);
                      }}
                    >
                      {livePreview ? "Stop live preview" : "Start live preview"}
                    </button>
                    <label><input type="checkbox" checked={fullPage} disabled={livePreview} onChange={(event: ChangeEvent<HTMLInputElement>) => setFullPage(event.target.checked)} /> Full page</label>
                    <button className="secondary-button" type="button" disabled={!selectedTabId || busyId !== null || livePreview} onClick={() => capture()}>{busyId === "screenshot" ? "Capturing…" : "Capture"}</button>
                  </div>
                  {screenshot ? (
                    <>
                      <button
                        className="browser-preview-picker"
                        type="button"
                        disabled={screenshot.fullPage || busyId === "visual-select"}
                        title={screenshot.fullPage ? "Capture a viewport screenshot to select an element." : "Click an element to make it the current Change This target."}
                      >
                        <img
                          className={`browser-screenshot-preview ${screenshot.fullPage ? "" : "selectable"}`}
                          alt="RepoTunnel live browser preview"
                          src={`data:${screenshot.mimeType};base64,${screenshot.dataBase64}`}
                          onClick={(event) => selectPreviewElement(event)}
                        />
                      </button>
                      {!screenshot.fullPage ? <small className="browser-preview-help">Click an element in the preview, then tell the AI “change this…”</small> : null}
                    </>
                  ) : <p>Start Live Preview to watch the selected tab here.</p>}
                  {visualSelection ? (
                    <div className="browser-visual-selection">
                      <span>Selected for “Change This”</span>
                      <code>{visualSelection.selector}</code>
                      <small>{visualSelection.text || `<${visualSelection.tag}>`}</small>
                    </div>
                  ) : null}
                </div>
              </div>

              <div className="browser-diagnostics-grid">
                <div className="browser-diagnostic-card">
                  <div className="browser-card-heading"><strong>Console errors</strong><span>{diagnostics.consoleEntries.length}</span></div>
                  {diagnostics.consoleEntries.length === 0 ? <p>No captured console errors or warnings.</p> : (
                    <div className="browser-diagnostic-list">
                      {diagnostics.consoleEntries.slice(-20).reverse().map((entry, index) => (
                        <div className={`browser-diagnostic-entry ${entry.level}`} key={`${entry.timestamp}-${index}`}>
                          <span>{entry.level} · {formatTime(entry.timestamp)}</span>
                          <code>{entry.message}</code>
                          {entry.url ? <small>{entry.url}</small> : null}
                        </div>
                      ))}
                    </div>
                  )}
                </div>
                <div className="browser-diagnostic-card">
                  <div className="browser-card-heading"><strong>Network failures</strong><span>{diagnostics.networkFailures.length}</span></div>
                  {diagnostics.networkFailures.length === 0 ? <p>No failed requests or HTTP 4xx/5xx responses captured.</p> : (
                    <div className="browser-diagnostic-list">
                      {diagnostics.networkFailures.slice(-20).reverse().map((entry, index) => (
                        <div className="browser-diagnostic-entry error" key={`${entry.timestamp}-${index}`}>
                          <span>{entry.status ? `HTTP ${entry.status}` : "Failed"} · {formatTime(entry.timestamp)}</span>
                          <code>{entry.method ? `${entry.method} ` : ""}{entry.url ?? "Unknown URL"}</code>
                          <small>{entry.errorText}</small>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              </div>
            </>
          ) : null}

          {pending.length > 0 ? (
            <div className="browser-pending-section">
              <div className="browser-card-heading"><strong>Pending browser review</strong><span>{pending.length}</span></div>
              {pending.map((action) => (
                <div className="browser-pending-row" key={action.id}>
                  <div>
                    <span className={`change-status ${action.status}`}>{statusLabels[action.status]}</span>
                    <strong>{kindLabels[action.kind]}</strong>
                    <code>{action.target}</code>
                    {action.detail ? <small>{action.detail}</small> : null}
                  </div>
                  <div>
                    <button className="secondary-button reject-button" type="button" disabled={busyId !== null} onClick={() => actOnPending(action, "reject")}>Reject</button>
                    <button className="primary-button" type="button" disabled={busyId !== null} onClick={() => actOnPending(action, "approve")}>{busyId === action.id ? "Applying…" : "Accept"}</button>
                  </div>
                </div>
              ))}
            </div>
          ) : null}

          {history.length > 0 ? (
            <details className="browser-history-details">
              <summary>Recent browser activity · {history.length}</summary>
              <div className="browser-history-list">
                {history.slice(0, 20).map((action) => (
                  <div className="browser-history-row" key={action.id}>
                    <span className={`change-status ${action.status}`}>{statusLabels[action.status]}</span>
                    <strong>{kindLabels[action.kind]}</strong>
                    <code>{action.target}</code>
                    <small>{formatTime(action.updatedAt)}</small>
                    {action.error ? <p className="change-error">{action.error}</p> : null}
                  </div>
                ))}
              </div>
            </details>
          ) : null}
        </>
      )}
    </div>
  );
}

export default BrowserAutomation;
