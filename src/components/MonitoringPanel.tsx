import { useCallback, useEffect, useMemo, useState } from "react";
import {
  getMonitoringSnapshot,
  getMonitoringStatus,
  startWorkspaceMonitoring,
  stopWorkspaceMonitoring,
} from "../lib/backend";
import type {
  MonitoringFileChangeKind,
  MonitoringSnapshot,
  MonitoringStatus,
  Workspace,
} from "../types";

type MonitoringPanelProps = {
  workspace: Workspace | null;
  gatewayRunning: boolean;
  onError: (message: string) => void;
};

const fileKindLabel: Record<MonitoringFileChangeKind, string> = {
  created: "Created",
  modified: "Modified",
  deleted: "Deleted",
};

function formatTime(value: number | null): string {
  if (!value) return "Not yet";
  return new Intl.DateTimeFormat(undefined, {
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
  }).format(new Date(value));
}

function MonitoringPanel({ workspace, gatewayRunning, onError }: MonitoringPanelProps) {
  const [status, setStatus] = useState<MonitoringStatus | null>(null);
  const [snapshot, setSnapshot] = useState<MonitoringSnapshot | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async (quiet = false) => {
    if (!workspace) {
      setStatus(null);
      setSnapshot(null);
      return;
    }
    if (!quiet) setLoading(true);
    try {
      const [nextStatus, nextSnapshot] = await Promise.all([
        getMonitoringStatus(workspace.id),
        getMonitoringSnapshot(workspace.id),
      ]);
      setStatus(nextStatus);
      setSnapshot(nextSnapshot);
    } catch (error) {
      if (!quiet) onError(error instanceof Error ? error.message : String(error));
    } finally {
      if (!quiet) setLoading(false);
    }
  }, [onError, workspace]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (!workspace) return;
    const timer = window.setInterval(
      () => void refresh(true),
      status?.running ? 2500 : gatewayRunning ? 5000 : 7000,
    );
    return () => window.clearInterval(timer);
  }, [gatewayRunning, refresh, status?.running, workspace]);

  async function toggleMonitoring() {
    if (!workspace) return;
    setBusy(true);
    try {
      const next = status?.running
        ? await stopWorkspaceMonitoring(workspace.id)
        : await startWorkspaceMonitoring(workspace.id);
      setStatus(next);
      setSnapshot(await getMonitoringSnapshot(workspace.id));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  const managedPorts = useMemo(
    () => snapshot?.ports.filter((listener) => listener.managedProcessId !== null) ?? [],
    [snapshot?.ports],
  );
  const consoleIssues = snapshot?.browser.consoleEntries.length ?? 0;
  const networkIssues = snapshot?.browser.networkFailures.length ?? 0;
  const fileEvents = snapshot?.fileEvents ?? [];
  const visiblePorts = snapshot?.ports.slice(0, 18) ?? [];

  return (
    <section className="monitoring-panel" aria-labelledby="monitoring-title">
      <div className="monitoring-heading">
        <div>
          <span className="section-kicker">Monitoring</span>
          <h3 id="monitoring-title">Live development observability</h3>
          <p>
            Watch managed processes, terminal output, dev-server ports, browser diagnostics and project file changes from one place.
            Monitoring is read-only and never needs an action approval.
          </p>
        </div>
        <div className="monitoring-heading-actions">
          <span className={`monitoring-state ${status?.running ? "running" : "stopped"}`}>
            {status?.running ? "Monitoring active" : "File monitor off"}
          </span>
          <button className="secondary-button" type="button" disabled={!workspace || loading || busy} onClick={() => void refresh()}>
            {loading ? "Refreshing…" : "Refresh"}
          </button>
          <button className={status?.running ? "secondary-button reject-button" : "primary-button"} type="button" disabled={!workspace || busy} onClick={toggleMonitoring}>
            {busy ? "Updating…" : status?.running ? "Stop monitoring" : "Start monitoring"}
          </button>
        </div>
      </div>

      {!workspace ? (
        <div className="command-empty"><p>Add or select an approved project to monitor it.</p></div>
      ) : (
        <>
          <div className="monitoring-summary-grid">
            <div className="monitoring-summary-card"><span>Processes</span><strong>{snapshot?.processes.length ?? 0}</strong><small>managed & running</small></div>
            <div className="monitoring-summary-card"><span>Dev ports</span><strong>{managedPorts.length}</strong><small>{snapshot?.ports.length ?? 0} listeners visible</small></div>
            <div className="monitoring-summary-card"><span>File changes</span><strong>{fileEvents.length}</strong><small>recent tracked events</small></div>
            <div className="monitoring-summary-card"><span>Browser tabs</span><strong>{snapshot?.browser.tabs.length ?? 0}</strong><small>{snapshot?.browser.status.running ? "automation running" : "browser stopped"}</small></div>
            <div className="monitoring-summary-card"><span>Console</span><strong>{consoleIssues}</strong><small>recent diagnostics</small></div>
            <div className="monitoring-summary-card"><span>Network</span><strong>{networkIssues}</strong><small>failures / HTTP errors</small></div>
          </div>

          <div className="monitoring-meta-row">
            <span>Last file scan <strong>{formatTime(status?.lastScanAt ?? null)}</strong></span>
            <span><strong>{status?.scannedFileCount ?? 0}</strong> project files watched</span>
            {status?.fileScanTruncated ? <span className="monitoring-warning">Project scan capped at the safe monitoring limit</span> : null}
            {status?.message ? <span className="monitoring-warning">{status.message}</span> : null}
          </div>

          <div className="monitoring-columns">
            <div className="monitoring-card">
              <div className="monitoring-card-heading"><strong>Processes & dev servers</strong><span>{snapshot?.processes.length ?? 0} running</span></div>
              {snapshot?.processes.length ? (
                <div className="monitoring-process-list">
                  {snapshot.processes.map((process) => (
                    <article className="monitoring-process" key={process.processId}>
                      <div className="monitoring-process-title">
                        <div><strong>{process.label}</strong><span>{process.pid ? `PID ${process.pid}` : "PID unavailable"}</span></div>
                        <div className="monitoring-port-chips">
                          {process.ports.length ? process.ports.map((port) => <span key={port}>:{port}</span>) : <span className="muted-chip">no listener</span>}
                        </div>
                      </div>
                      <code>{process.command}</code>
                      {process.stdoutTail ? <pre className="monitoring-output">{process.stdoutTail}</pre> : null}
                      {process.stderrTail ? <pre className="monitoring-output error-output">{process.stderrTail}</pre> : null}
                      {!process.stdoutTail && !process.stderrTail ? <p className="monitoring-empty-copy">No recent output.</p> : null}
                      {process.outputTruncated ? <small>Showing the latest bounded output tail.</small> : null}
                    </article>
                  ))}
                </div>
              ) : <p className="monitoring-empty-copy">No RepoTunnel-managed processes are running.</p>}
            </div>

            <div className="monitoring-card">
              <div className="monitoring-card-heading"><strong>Listening ports</strong><span>{snapshot?.ports.length ?? 0} detected</span></div>
              {visiblePorts.length ? (
                <div className="monitoring-port-list">
                  {visiblePorts.map((listener, index) => (
                    <div className={`monitoring-port-row ${listener.managedProcessId ? "managed" : ""}`} key={`${listener.protocol}-${listener.address}-${listener.port}-${listener.pid ?? index}`}>
                      <code>{listener.address}:{listener.port}</code>
                      <span>{listener.processName ?? "owner unavailable"}{listener.pid ? ` · PID ${listener.pid}` : ""}</span>
                      {listener.managedProcessId ? <em>RepoTunnel process</em> : null}
                    </div>
                  ))}
                </div>
              ) : <p className="monitoring-empty-copy">No TCP listeners detected.</p>}
            </div>
          </div>

          <div className="monitoring-columns">
            <div className="monitoring-card">
              <div className="monitoring-card-heading"><strong>Project file activity</strong><span>{status?.running ? "live" : "saved events"}</span></div>
              {fileEvents.length ? (
                <div className="monitoring-event-list">
                  {fileEvents.slice(0, 18).map((event) => (
                    <div className="monitoring-file-event" key={event.id}>
                      <span className={`monitoring-file-kind ${event.kind}`}>{fileKindLabel[event.kind]}</span>
                      <code>{event.path}</code>
                      <time>{formatTime(event.detectedAt)}</time>
                    </div>
                  ))}
                </div>
              ) : <p className="monitoring-empty-copy">No project file changes have been observed yet.</p>}
            </div>

            <div className="monitoring-card">
              <div className="monitoring-card-heading"><strong>Recent terminal results</strong><span>{snapshot?.terminal.length ?? 0} commands</span></div>
              {snapshot?.terminal.length ? (
                <div className="monitoring-terminal-list">
                  {snapshot.terminal.slice(0, 10).map((command) => (
                    <article className="monitoring-terminal-row" key={command.commandId}>
                      <div><span className={`change-status ${command.status}`}>{command.status}</span>{command.exitCode !== null ? <span>exit {command.exitCode}</span> : null}</div>
                      <code>{command.command}</code>
                      {command.stdoutTail ? <pre className="monitoring-output compact">{command.stdoutTail}</pre> : null}
                      {command.stderrTail ? <pre className="monitoring-output compact error-output">{command.stderrTail}</pre> : null}
                    </article>
                  ))}
                </div>
              ) : <p className="monitoring-empty-copy">No recent live terminal commands.</p>}
            </div>
          </div>

          <div className="monitoring-card browser-monitoring-card">
            <div className="monitoring-card-heading">
              <strong>Browser diagnostics</strong>
              <span>{snapshot?.browser.status.running ? `${snapshot.browser.tabs.length} tabs connected` : "managed browser stopped"}</span>
            </div>
            <div className="browser-monitoring-grid">
              <div>
                <span className="monitoring-subtitle">Console</span>
                {snapshot?.browser.consoleEntries.length ? snapshot.browser.consoleEntries.slice(0, 12).map((entry, index) => (
                  <div className="monitoring-diagnostic" key={`${entry.tabId}-${entry.timestamp}-${index}`}>
                    <span>{entry.level}</span><p>{entry.message}</p>
                  </div>
                )) : <p className="monitoring-empty-copy">No captured console diagnostics.</p>}
              </div>
              <div>
                <span className="monitoring-subtitle">Network</span>
                {snapshot?.browser.networkFailures.length ? snapshot.browser.networkFailures.slice(0, 12).map((failure, index) => (
                  <div className="monitoring-diagnostic" key={`${failure.tabId}-${failure.timestamp}-${index}`}>
                    <span>{failure.status ?? "ERR"}</span><p>{failure.method ? `${failure.method} ` : ""}{failure.url ?? failure.errorText}</p>
                  </div>
                )) : <p className="monitoring-empty-copy">No captured network failures.</p>}
              </div>
            </div>
          </div>
        </>
      )}
    </section>
  );
}

export default MonitoringPanel;
