import { useEffect, useMemo, useState } from "react";
import { getWorkflowReadiness } from "../lib/backend";
import type { WorkflowReadiness, Workspace } from "../types";

type WorkflowPanelProps = {
  workspaces: Workspace[];
  chatConnected: boolean;
  gatewayRunning: boolean;
};

const flow = [
  ["Inspect", "Understand the filtered project before changing code."],
  ["Edit", "Use project-level AI auto or review mode with local version protection."],
  ["Verify", "Run a discovered build/test/check/lint preset in the sandbox."],
  ["Git", "Review diff, stage explicit files, then commit with local approval."],
] as const;

function WorkflowPanel({ workspaces, chatConnected, gatewayRunning }: WorkflowPanelProps) {
  const [workspaceId, setWorkspaceId] = useState(workspaces[0]?.id ?? "");
  const [report, setReport] = useState<WorkflowReadiness | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!workspaces.some((workspace) => workspace.id === workspaceId)) {
      setWorkspaceId(workspaces[0]?.id ?? "");
      setReport(null);
    }
  }, [workspaces, workspaceId]);

  const selected = useMemo(
    () => workspaces.find((workspace) => workspace.id === workspaceId) ?? null,
    [workspaces, workspaceId],
  );

  async function runCheck() {
    if (!workspaceId) return;
    setChecking(true);
    setError(null);
    try {
      setReport(await getWorkflowReadiness(workspaceId));
    } catch (checkError) {
      setError(checkError instanceof Error ? checkError.message : String(checkError));
    } finally {
      setChecking(false);
    }
  }

  const connectionReady = gatewayRunning && chatConnected;

  return (
    <section className="workflow-section" aria-labelledby="workflow-title">
      <div className="section-heading workflow-heading">
        <div>
          <span className="section-kicker">Development workflow</span>
          <h2 id="workflow-title">End-to-end readiness</h2>
          <p>
            Check whether this project is actually ready for the full ChatGPT workflow before an AI
            starts editing, testing, staging, and committing.
          </p>
        </div>
        <div className="workflow-actions">
          <select
            aria-label="Project for workflow check"
            value={workspaceId}
            onChange={(event) => {
              setWorkspaceId(event.target.value);
              setReport(null);
              setError(null);
            }}
            disabled={workspaces.length === 0 || checking}
          >
            {workspaces.length === 0 ? <option value="">No projects</option> : null}
            {workspaces.map((workspace) => (
              <option key={workspace.id} value={workspace.id}>
                {workspace.name}
              </option>
            ))}
          </select>
          <button
            className="secondary-button"
            type="button"
            onClick={runCheck}
            disabled={!selected || checking}
          >
            {checking ? "Checking…" : report ? "Run again" : "Check workflow"}
          </button>
        </div>
      </div>

      <div className={`workflow-connection ${connectionReady ? "ready" : "offline"}`}>
        <span className="status-dot" aria-hidden="true" />
        <div>
          <strong>{connectionReady ? "ChatGPT path connected" : "Local workflow check"}</strong>
          <p>
            {connectionReady
              ? "ChatGPT is connected through the gateway; project readiness below determines which development steps are available."
              : "You can check project readiness without connecting ChatGPT. Connect later to exercise the workflow from a normal chat."}
          </p>
        </div>
      </div>

      <div className="workflow-flow" aria-label="Recommended AI development sequence">
        {flow.map(([title, detail], index) => (
          <div className="workflow-step" key={title}>
            <span>{index + 1}</span>
            <div><strong>{title}</strong><p>{detail}</p></div>
          </div>
        ))}
      </div>

      {error ? <p className="workflow-error">{error}</p> : null}

      {!report ? (
        <div className="workflow-empty">
          <strong>{selected ? `Ready to check ${selected.name}` : "Add a project first"}</strong>
          <p>The check is read-only and does not create test files, stage changes, or run project code.</p>
        </div>
      ) : (
        <div className="workflow-report">
          <div className={`workflow-summary ${report.level}`}>
            <div>
              <span>Workflow status</span>
              <strong>{report.level === "ready" ? "Ready" : report.level === "limited" ? "Ready with limits" : "Blocked"}</strong>
            </div>
            <div className="workflow-summary-metrics">
              <span>{report.projectFileCount} indexed files</span>
              <span>{report.commandPresetCount} command presets</span>
              {report.gitBranch ? <span>Git: {report.gitBranch}</span> : null}
            </div>
          </div>

          <div className="workflow-checks">
            {report.checks.map((item) => (
              <div className={`workflow-check ${item.status}`} key={item.key}>
                <span className="workflow-check-icon" aria-hidden="true">
                  {item.status === "pass" ? "✓" : item.status === "warning" ? "!" : "×"}
                </span>
                <div><strong>{item.title}</strong><p>{item.detail}</p></div>
              </div>
            ))}
          </div>

          <div className="workflow-next">
            <span>Recommended next step</span>
            <p>{report.nextStep}</p>
          </div>
        </div>
      )}
    </section>
  );
}

export default WorkflowPanel;
