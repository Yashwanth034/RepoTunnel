import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import ApplicationLauncher from "./ApplicationLauncher";
import BrowserAutomation from "./BrowserAutomation";
import MonitoringPanel from "./MonitoringPanel";
import {
  approveCommand,
  approveManagedProcess,
  approveTerminalCommand,
  getExecutionStatus,
  listCommandHistory,
  listCommandPresets,
  listManagedProcesses,
  listTerminalHistory,
  readManagedProcessOutput,
  rejectCommand,
  rejectManagedProcess,
  rejectTerminalCommand,
  restartManagedProcess,
  runTerminalCommand,
  runWorkspaceCommand,
  startManagedProcess,
  stopManagedProcess,
} from "../lib/backend";
import type {
  CommandPreset,
  CommandRecord,
  CommandStatus,
  ExecutionStatus,
  ManagedProcessRecord,
  ManagedProcessStatus,
  TerminalCommandRecord,
  TerminalCommandStatus,
  Workspace,
} from "../types";

type ExecutionPanelProps = {
  workspaces: Workspace[];
  gatewayRunning: boolean;
  onError: (message: string) => void;
};

type ProcessLogView = {
  processId: string;
  stdout: string;
  stderr: string;
  stdoutOffset: number;
  stderrOffset: number;
  outputTruncated: boolean;
};

const sandboxStatusLabels: Record<CommandStatus, string> = {
  pending: "Pending approval",
  running: "Running",
  completed: "Completed",
  failed: "Failed",
  rejected: "Rejected",
  timedOut: "Timed out",
};

const terminalStatusLabels: Record<TerminalCommandStatus, string> = {
  pending: "Pending approval",
  running: "Running",
  completed: "Completed",
  failed: "Failed",
  rejected: "Rejected",
  timedOut: "Timed out",
};

const processStatusLabels: Record<ManagedProcessStatus, string> = {
  pending: "Pending approval",
  running: "Running",
  exited: "Exited",
  stopped: "Stopped",
  failed: "Failed",
  rejected: "Rejected",
};

function formatDuration(durationMs: number | null): string | null {
  if (durationMs === null) return null;
  if (durationMs < 1000) return `${durationMs} ms`;
  return `${(durationMs / 1000).toFixed(durationMs < 10_000 ? 1 : 0)} s`;
}

function policyCopy(workspace: Workspace | null): string {
  if (!workspace) return "Select an approved project";
  if (workspace.changePolicy === "automatic") return "AI Auto · live actions run immediately";
  if (workspace.commandPolicy === "disabled") return "AI Review · commands disabled";
  if (workspace.commandPolicy === "automatic") return "AI Review · commands set to auto run";
  return "AI Review · live actions require approval";
}

function mergeLogChunk(current: ProcessLogView | null, chunk: Awaited<ReturnType<typeof readManagedProcessOutput>>): ProcessLogView {
  const sameProcess = current?.processId === chunk.processId;
  return {
    processId: chunk.processId,
    stdout: `${sameProcess ? current.stdout : ""}${chunk.stdout}`,
    stderr: `${sameProcess ? current.stderr : ""}${chunk.stderr}`,
    stdoutOffset: chunk.nextStdoutOffset,
    stderrOffset: chunk.nextStderrOffset,
    outputTruncated: Boolean((sameProcess && current.outputTruncated) || chunk.outputTruncated),
  };
}

function ExecutionPanel({ workspaces, gatewayRunning, onError }: ExecutionPanelProps) {
  const [selectedWorkspaceId, setSelectedWorkspaceId] = useState<string>("");
  const [status, setStatus] = useState<ExecutionStatus | null>(null);
  const [presets, setPresets] = useState<CommandPreset[]>([]);
  const [sandboxHistory, setSandboxHistory] = useState<CommandRecord[]>([]);
  const [terminalHistory, setTerminalHistory] = useState<TerminalCommandRecord[]>([]);
  const [processes, setProcesses] = useState<ManagedProcessRecord[]>([]);
  const [loading, setLoading] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [terminalCommand, setTerminalCommand] = useState("");
  const [terminalCwd, setTerminalCwd] = useState("");
  const [processCommand, setProcessCommand] = useState("");
  const [processCwd, setProcessCwd] = useState("");
  const [processLabel, setProcessLabel] = useState("");
  const [expandedProcessId, setExpandedProcessId] = useState<string | null>(null);
  const [processLog, setProcessLog] = useState<ProcessLogView | null>(null);
  const processLogRef = useRef<ProcessLogView | null>(null);
  const expandedProcessIdRef = useRef<string | null>(null);
  const processOutputPollingRef = useRef(false);

  const selectedWorkspace = useMemo(
    () => workspaces.find((workspace) => workspace.id === selectedWorkspaceId) ?? null,
    [selectedWorkspaceId, workspaces],
  );

  useEffect(() => {
    if (!selectedWorkspaceId || !workspaces.some((workspace) => workspace.id === selectedWorkspaceId)) {
      setSelectedWorkspaceId(workspaces[0]?.id ?? "");
    }
  }, [selectedWorkspaceId, workspaces]);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const [sandboxStatus, commandHistory, liveHistory, processHistory] = await Promise.all([
        getExecutionStatus(),
        listCommandHistory(undefined, 60),
        listTerminalHistory(undefined, 60),
        listManagedProcesses(undefined, 60),
      ]);
      setStatus(sandboxStatus);
      setSandboxHistory(commandHistory);
      setTerminalHistory(liveHistory);
      setProcesses(processHistory);
      if (selectedWorkspaceId) {
        setPresets(await listCommandPresets(selectedWorkspaceId));
      } else {
        setPresets([]);
      }
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setLoading(false);
    }
  }, [onError, selectedWorkspaceId]);

  useEffect(() => {
    refresh().catch(() => undefined);
  }, [refresh]);

  useEffect(() => {
    const timer = window.setInterval(() => {
      Promise.all([
        listCommandHistory(undefined, 60),
        listTerminalHistory(undefined, 60),
        listManagedProcesses(undefined, 60),
      ])
        .then(([commands, terminals, managed]) => {
          setSandboxHistory(commands);
          setTerminalHistory(terminals);
          setProcesses(managed);
        })
        .catch(() => undefined);
    }, gatewayRunning ? 2500 : 5000);
    return () => window.clearInterval(timer);
  }, [gatewayRunning]);

  useEffect(() => {
    processLogRef.current = processLog;
  }, [processLog]);

  useEffect(() => {
    expandedProcessIdRef.current = expandedProcessId;
    processOutputPollingRef.current = false;
  }, [expandedProcessId]);

  const expandedProcessStatus = useMemo(
    () => processes.find((item) => item.id === expandedProcessId)?.status ?? null,
    [expandedProcessId, processes],
  );

  useEffect(() => {
    if (!expandedProcessId) return;
    if (!expandedProcessStatus || expandedProcessStatus === "pending" || expandedProcessStatus === "rejected") return;

    const pollOutput = () => {
      if (processOutputPollingRef.current) return;
      processOutputPollingRef.current = true;
      const current = processLogRef.current;
      const stdoutOffset = current?.processId === expandedProcessId ? current.stdoutOffset : 0;
      const stderrOffset = current?.processId === expandedProcessId ? current.stderrOffset : 0;
      readManagedProcessOutput(expandedProcessId, stdoutOffset, stderrOffset)
        .then((chunk) => {
          if (expandedProcessIdRef.current === chunk.processId) {
            setProcessLog((latest) => mergeLogChunk(latest, chunk));
          }
        })
        .catch(() => undefined)
        .finally(() => {
          processOutputPollingRef.current = false;
        });
    };

    const timer = window.setInterval(pollOutput, expandedProcessStatus === "running" ? 1500 : 4000);
    return () => window.clearInterval(timer);
  }, [expandedProcessId, expandedProcessStatus]);

  async function refreshLiveActivity() {
    const [liveHistory, managed] = await Promise.all([
      listTerminalHistory(undefined, 60),
      listManagedProcesses(undefined, 60),
    ]);
    setTerminalHistory(liveHistory);
    setProcesses(managed);
  }

  async function requestPreset(preset: CommandPreset) {
    if (!selectedWorkspace) return;
    setBusyId(preset.id);
    try {
      await runWorkspaceCommand(selectedWorkspace.id, preset.id);
      setSandboxHistory(await listCommandHistory(undefined, 60));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function actOnSandboxCommand(command: CommandRecord, action: "approve" | "reject") {
    setBusyId(command.id);
    try {
      if (action === "approve") {
        await approveCommand(command.id);
      } else {
        await rejectCommand(command.id);
      }
      setSandboxHistory(await listCommandHistory(undefined, 60));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
      setSandboxHistory(await listCommandHistory(undefined, 60).catch(() => sandboxHistory));
    } finally {
      setBusyId(null);
    }
  }

  async function submitTerminalCommand() {
    if (!selectedWorkspace || !terminalCommand.trim()) return;
    setBusyId("live-terminal");
    try {
      await runTerminalCommand(
        selectedWorkspace.id,
        terminalCommand,
        terminalCwd.trim() || undefined,
      );
      setTerminalCommand("");
      await refreshLiveActivity();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function actOnTerminalCommand(command: TerminalCommandRecord, action: "approve" | "reject") {
    setBusyId(command.id);
    try {
      if (action === "approve") {
        await approveTerminalCommand(command.id);
      } else {
        await rejectTerminalCommand(command.id);
      }
      setTerminalHistory(await listTerminalHistory(undefined, 60));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function submitProcess() {
    if (!selectedWorkspace || !processCommand.trim()) return;
    setBusyId("managed-process");
    try {
      await startManagedProcess(
        selectedWorkspace.id,
        processCommand,
        processCwd.trim() || undefined,
        processLabel.trim() || undefined,
      );
      setProcessCommand("");
      setProcessLabel("");
      await refreshLiveActivity();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function actOnProcess(process: ManagedProcessRecord, action: "approve" | "reject" | "stop" | "restart") {
    setBusyId(process.id);
    try {
      if (action === "approve") await approveManagedProcess(process.id);
      if (action === "reject") await rejectManagedProcess(process.id);
      if (action === "stop") await stopManagedProcess(process.id);
      if (action === "restart") await restartManagedProcess(process.id);
      await refreshLiveActivity();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function toggleProcessOutput(process: ManagedProcessRecord) {
    if (expandedProcessId === process.id) {
      expandedProcessIdRef.current = null;
      setExpandedProcessId(null);
      setProcessLog(null);
      return;
    }
    expandedProcessIdRef.current = process.id;
    setExpandedProcessId(process.id);
    setProcessLog(null);
    try {
      const chunk = await readManagedProcessOutput(process.id, 0, 0);
      if (expandedProcessIdRef.current === process.id) {
        setProcessLog(mergeLogChunk(null, chunk));
      }
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    }
  }

  const pendingCount = sandboxHistory.filter((command) => command.status === "pending").length
    + terminalHistory.filter((command) => command.status === "pending").length
    + processes.filter((process) => process.status === "pending").length;
  const runningProcesses = processes.filter((process) => process.status === "running").length;
  const selectedProcesses = processes.filter((process) => !selectedWorkspace || process.workspaceId === selectedWorkspace.id);
  const selectedTerminalHistory = terminalHistory.filter((command) => !selectedWorkspace || command.workspaceId === selectedWorkspace.id);
  const selectedPendingTerminal = selectedTerminalHistory.filter((command) => command.status === "pending");

  return (
    <section className="execution-section" aria-labelledby="execution-title">
      <div className="section-heading execution-heading">
        <div>
          <span className="section-kicker">Commands</span>
          <h2 id="execution-title">Terminal, processes, apps & browser</h2>
          <p>
            Run one-shot commands and persistent development processes inside the approved project boundary.
            Launch localhost, project paths and supported applications, then use browser automation and monitoring when the project needs deeper verification.
          </p>
        </div>
        <div className="execution-heading-actions">
          {runningProcesses > 0 ? <span className="running-count">{runningProcesses} running</span> : null}
          {pendingCount > 0 ? <span className="pending-count">{pendingCount} pending</span> : null}
          <button className="secondary-button" type="button" onClick={() => refresh()} disabled={loading}>
            {loading ? "Refreshing…" : "Refresh"}
          </button>
        </div>
      </div>

      {workspaces.length > 0 ? (
        <div className="command-picker">
          <select
            aria-label="Project for terminal commands"
            value={selectedWorkspaceId}
            onChange={(event) => setSelectedWorkspaceId(event.target.value)}
          >
            {workspaces.map((workspace) => (
              <option key={workspace.id} value={workspace.id}>{workspace.name}</option>
            ))}
          </select>
          <span><strong>{policyCopy(selectedWorkspace)}</strong></span>
        </div>
      ) : null}

      <div className="live-terminal-grid">
        <div className="live-terminal-card">
          <div className="live-terminal-card-heading">
            <div>
              <strong>Live terminal</strong>
              <span>One-shot command · real workspace</span>
            </div>
            <span className="live-side-effect-badge">Project-scoped</span>
          </div>
          <textarea
            aria-label="Live terminal command"
            value={terminalCommand}
            onChange={(event) => setTerminalCommand(event.target.value)}
            placeholder="npm install && npm run build"
            rows={3}
            disabled={!selectedWorkspace || selectedWorkspace.commandPolicy === "disabled" && selectedWorkspace.changePolicy !== "automatic"}
          />
          <div className="live-terminal-form-footer">
            <input
              aria-label="Terminal working directory"
              value={terminalCwd}
              onChange={(event) => setTerminalCwd(event.target.value)}
              placeholder="Working directory (project root)"
              disabled={!selectedWorkspace}
            />
            <button
              className="primary-button"
              type="button"
              disabled={!selectedWorkspace || !terminalCommand.trim() || busyId !== null || selectedWorkspace.accessMode === "readOnly" || selectedWorkspace.commandPolicy === "disabled" && selectedWorkspace.changePolicy !== "automatic"}
              onClick={submitTerminalCommand}
            >
              {busyId === "live-terminal" ? "Running…" : selectedWorkspace?.changePolicy === "automatic" || selectedWorkspace?.commandPolicy === "automatic" ? "Run command" : "Request command"}
            </button>
          </div>
        </div>

        <div className="live-terminal-card">
          <div className="live-terminal-card-heading">
            <div>
              <strong>Persistent process</strong>
              <span>Dev server, watcher or worker</span>
            </div>
            <span className="process-count-badge">{selectedProcesses.filter((item) => item.status === "running").length} active</span>
          </div>
          <input
            aria-label="Process label"
            value={processLabel}
            onChange={(event) => setProcessLabel(event.target.value)}
            placeholder="Optional label (Frontend dev server)"
            disabled={!selectedWorkspace}
          />
          <textarea
            aria-label="Persistent process command"
            value={processCommand}
            onChange={(event) => setProcessCommand(event.target.value)}
            placeholder="npm run dev -- --host 127.0.0.1"
            rows={2}
            disabled={!selectedWorkspace || selectedWorkspace.commandPolicy === "disabled" && selectedWorkspace.changePolicy !== "automatic"}
          />
          <div className="live-terminal-form-footer">
            <input
              aria-label="Process working directory"
              value={processCwd}
              onChange={(event) => setProcessCwd(event.target.value)}
              placeholder="Working directory (project root)"
              disabled={!selectedWorkspace}
            />
            <button
              className="primary-button"
              type="button"
              disabled={!selectedWorkspace || !processCommand.trim() || busyId !== null || selectedWorkspace.accessMode === "readOnly" || selectedWorkspace.commandPolicy === "disabled" && selectedWorkspace.changePolicy !== "automatic"}
              onClick={submitProcess}
            >
              {busyId === "managed-process" ? "Starting…" : selectedWorkspace?.changePolicy === "automatic" || selectedWorkspace?.commandPolicy === "automatic" ? "Start process" : "Request start"}
            </button>
          </div>
        </div>
      </div>

      <ApplicationLauncher
        workspace={selectedWorkspace}
        gatewayRunning={gatewayRunning}
        onError={onError}
      />

      <BrowserAutomation
        workspace={selectedWorkspace}
        gatewayRunning={gatewayRunning}
        onError={onError}
      />

      <MonitoringPanel
        workspace={selectedWorkspace}
        gatewayRunning={gatewayRunning}
        onError={onError}
      />

      <div className="managed-process-section">
        <div className="command-history-title">
          <strong>Managed processes</strong>
          <span>{selectedProcesses.length} records</span>
        </div>
        {selectedProcesses.length === 0 ? (
          <div className="command-empty"><p>No persistent processes have been started for this project.</p></div>
        ) : (
          <div className="managed-process-list">
            {selectedProcesses.slice(0, 20).map((process) => (
              <article className={`managed-process-record ${process.status}`} key={process.id}>
                <div className="managed-process-main">
                  <div className="managed-process-copy">
                    <div className="command-record-meta">
                      <span className={`change-status ${process.status}`}>{processStatusLabels[process.status]}</span>
                      {process.pid !== null ? <span>PID {process.pid}</span> : null}
                      <span>{process.cwd}</span>
                      {process.restartCount > 0 ? <span>{process.restartCount} restart{process.restartCount === 1 ? "" : "s"}</span> : null}
                      {process.exitCode !== null ? <span>exit {process.exitCode}</span> : null}
                    </div>
                    <h3>{process.label}</h3>
                    <code>{process.command}</code>
                    {process.error ? <p className="change-error">{process.error}</p> : null}
                  </div>
                  <div className="managed-process-actions">
                    {process.status === "pending" ? (
                      <>
                        <button className="secondary-button reject-button" type="button" disabled={busyId !== null} onClick={() => actOnProcess(process, "reject")}>Reject</button>
                        <button className="primary-button" type="button" disabled={busyId !== null} onClick={() => actOnProcess(process, "approve")}>{busyId === process.id ? "Starting…" : "Accept & start"}</button>
                      </>
                    ) : null}
                    {process.status === "running" ? (
                      <button className="secondary-button reject-button" type="button" disabled={busyId !== null} onClick={() => actOnProcess(process, "stop")}>{busyId === process.id ? "Stopping…" : "Stop"}</button>
                    ) : null}
                    {!["pending", "rejected"].includes(process.status) ? (
                      <button className="secondary-button" type="button" disabled={busyId !== null} onClick={() => actOnProcess(process, "restart")}>{busyId === process.id ? "Restarting…" : "Restart"}</button>
                    ) : null}
                    {!["pending", "rejected"].includes(process.status) ? (
                      <button className="secondary-button" type="button" onClick={() => toggleProcessOutput(process)}>{expandedProcessId === process.id ? "Hide output" : "Output"}</button>
                    ) : null}
                  </div>
                </div>
                {expandedProcessId === process.id ? (
                  <div className="managed-process-output">
                    {processLog?.processId === process.id && processLog.stdout ? <pre className="command-output">{processLog.stdout}</pre> : null}
                    {processLog?.processId === process.id && processLog.stderr ? <pre className="command-output error-output">{processLog.stderr}</pre> : null}
                    {processLog?.processId === process.id && !processLog.stdout && !processLog.stderr ? <p>No output captured yet.</p> : null}
                    {processLog?.outputTruncated ? <p className="change-note">Process output reached RepoTunnel&apos;s per-stream safety cap.</p> : null}
                  </div>
                ) : null}
              </article>
            ))}
          </div>
        )}
      </div>

      {selectedPendingTerminal.length > 0 ? (
        <div className="command-history live-command-history pending-only">
          <div className="command-history-title">
            <strong>Pending terminal approvals</strong>
            <span>{selectedPendingTerminal.length} waiting</span>
          </div>
          {selectedPendingTerminal.map((command) => (
            <article className={`command-record ${command.status}`} key={command.id}>
              <div className="command-record-header">
                <div>
                  <div className="command-record-meta">
                    <span className={`change-status ${command.status}`}>{terminalStatusLabels[command.status]}</span>
                    <span>{command.cwd}</span>
                  </div>
                  <h3>Terminal command</h3>
                  <code>{command.command}</code>
                </div>
                <div className="change-actions">
                  <button className="secondary-button reject-button" type="button" disabled={busyId !== null} onClick={() => actOnTerminalCommand(command, "reject")}>Reject</button>
                  <button className="primary-button" type="button" disabled={busyId !== null} onClick={() => actOnTerminalCommand(command, "approve")}>{busyId === command.id ? "Running…" : "Accept & run"}</button>
                </div>
              </div>
            </article>
          ))}
        </div>
      ) : null}

      <div className="sandbox-verification-section">
        <div className="command-history-title sandbox-section-heading">
          <div>
            <strong>Disposable verification sandbox</strong>
            <span>Existing offline Bubblewrap presets remain available for side-effect-free checks.</span>
          </div>
        </div>
        <div className={`sandbox-status ${status?.sandboxAvailable ? "available" : "unavailable"}`}>
          <span className="sandbox-indicator" aria-hidden="true" />
          <div>
            <strong>{status?.sandboxAvailable ? "Bubblewrap sandbox ready" : "Command sandbox unavailable"}</strong>
            <p>{status?.message ?? "Checking Linux sandbox support…"}</p>
          </div>
          {status?.sandboxVersion ? <code>{status.sandboxVersion}</code> : null}
        </div>

        {selectedWorkspace && status?.sandboxAvailable ? (
          presets.length > 0 ? (
            <div className="command-preset-list">
              {presets.map((preset) => (
                <div className="command-preset" key={preset.id}>
                  <div>
                    <strong>{preset.label}</strong>
                    <code>{preset.command}</code>
                  </div>
                  <button
                    className="secondary-button"
                    type="button"
                    disabled={busyId !== null || selectedWorkspace.commandPolicy === "disabled"}
                    onClick={() => requestPreset(preset)}
                  >
                    {busyId === preset.id ? "Requesting…" : selectedWorkspace.commandPolicy === "review" ? "Request run" : "Run"}
                  </button>
                </div>
              ))}
            </div>
          ) : (
            <div className="command-empty">
              <strong>No supported sandbox presets detected.</strong>
              <p>Live terminal commands remain available; the sandbox only discovers common verification scripts.</p>
            </div>
          )
        ) : null}

        <div className="command-history sandbox-command-history">
          <div className="command-history-title">
            <strong>Recent sandbox activity</strong>
            <span>{sandboxHistory.length} records</span>
          </div>
          {sandboxHistory.length === 0 ? (
            <div className="command-empty"><p>No sandbox commands have been requested yet.</p></div>
          ) : (
            sandboxHistory.slice(0, 12).map((command) => {
              const duration = formatDuration(command.durationMs);
              return (
                <article className={`command-record ${command.status}`} key={command.id}>
                  <div className="command-record-header">
                    <div>
                      <div className="command-record-meta">
                        <span className={`change-status ${command.status}`}>{sandboxStatusLabels[command.status]}</span>
                        <span>{command.workspaceName}</span>
                        {duration ? <span>{duration}</span> : null}
                        {command.exitCode !== null ? <span>exit {command.exitCode}</span> : null}
                      </div>
                      <h3>{command.label}</h3>
                      <code>{command.command}</code>
                    </div>
                    {command.status === "pending" ? (
                      <div className="change-actions">
                        <button className="secondary-button reject-button" type="button" disabled={busyId !== null} onClick={() => actOnSandboxCommand(command, "reject")}>Reject</button>
                        <button className="primary-button" type="button" disabled={busyId !== null} onClick={() => actOnSandboxCommand(command, "approve")}>{busyId === command.id ? "Running…" : "Approve & run"}</button>
                      </div>
                    ) : null}
                  </div>
                  {command.stdout ? <pre className="command-output">{command.stdout}</pre> : null}
                  {command.stderr ? <pre className="command-output error-output">{command.stderr}</pre> : null}
                  {command.error ? <p className="change-error">{command.error}</p> : null}
                  {command.outputTruncated ? <p className="change-note">Output was truncated to protect the app from excessive logs.</p> : null}
                </article>
              );
            })
          )}
        </div>
      </div>
    </section>
  );
}

export default ExecutionPanel;
