import { useCallback, useEffect, useState } from "react";
import {
  approveTaskIntegrityExpansion,
  cancelTaskIntegrity,
  getTaskIntegrityStatus,
  rejectTaskIntegrityExpansion,
} from "../lib/backend";
import type { TaskIntegrityLocalStatus } from "../types";

type TaskIntegrityPanelProps = {
  workspaceId: string;
  onError: (message: string) => void;
  onNotice: (message: string) => void;
};

const statusLabels: Record<string, string> = {
  active: "Active",
  awaitingScopeApproval: "Awaiting scope approval",
  frozenConflict: "Frozen conflict",
  frozenIntegrityViolation: "Integrity violation",
  verifying: "Verifying",
  awaitingReview: "Awaiting review",
  ambiguous: "Ambiguous",
  failed: "Failed",
  succeeded: "Succeeded",
  cancelled: "Cancelled",
};

const verificationLabels: Record<string, string> = {
  changeSpecific: "Change specific",
  subsystemRegression: "Subsystem regression",
  staticTooling: "Static tooling",
  integration: "Integration",
  fullGate: "Full gate",
};

function TaskIntegrityPanel({ workspaceId, onError, onNotice }: TaskIntegrityPanelProps) {
  const [status, setStatus] = useState<TaskIntegrityLocalStatus | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setStatus(await getTaskIntegrityStatus(workspaceId));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    }
  }, [onError, workspaceId]);

  useEffect(() => {
    refresh().catch(() => undefined);
  }, [refresh]);

  async function actOnExpansion(expansionId: string, action: "approve" | "reject") {
    setBusyId(expansionId);
    try {
      if (action === "approve") {
        await approveTaskIntegrityExpansion(workspaceId, expansionId);
        onNotice("Task scope expansion approved.");
      } else {
        await rejectTaskIntegrityExpansion(workspaceId, expansionId);
        onNotice("Task scope expansion rejected.");
      }
      await refresh();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function cancelTask() {
    const task = status?.task;
    if (!task) return;
    setBusyId(task.taskId);
    try {
      await cancelTaskIntegrity(workspaceId, task.taskId);
      onNotice("Task Integrity task cancelled.");
      await refresh();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  const task = status?.task ?? null;
  const activeTasks = status?.activeTasks ?? (task ? [task] : []);
  const pendingExpansions = status?.pendingExpansions ?? [];

  return (
    <section className="task-integrity-panel" aria-labelledby="task-integrity-title">
      <div className="task-integrity-heading">
        <div>
          <strong id="task-integrity-title">Task Integrity</strong>
          {!task ? <span>No active task</span> : null}
        </div>
        {task && !["succeeded", "failed", "cancelled"].includes(task.status) ? (
          <button
            className="secondary-button reject-button"
            type="button"
            disabled={busyId !== null}
            onClick={cancelTask}
          >
            {busyId === task.taskId ? "Cancelling…" : "Cancel task"}
          </button>
        ) : null}
      </div>

      {activeTasks.length > 1 ? (
        <div>
          <strong>Active tasks</strong>
          <ul>
            {activeTasks.map((activeTask) => (
              <li key={activeTask.taskId}>
                <span>{statusLabels[activeTask.status] ?? activeTask.status}</span>
                <span>{activeTask.intentSummary}</span>
                <code>{activeTask.taskId}</code>
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {task ? (
        <div className={`task-integrity-body ${task.status}`}>
          <div className="task-integrity-summary">
            <span className={`change-status ${task.status}`}>{statusLabels[task.status] ?? task.status}</span>
            <span>Scope revision {task.scopeRevision}</span>
            <span>{task.actualChangedCount} changed</span>
            <span>{task.protectedDirtyCount} protected dirty</span>
          </div>

          <p>{task.intentSummary}</p>

          {task.integrityViolation ? (
            <div className="task-integrity-alert" role="alert">
              <strong>Integrity violation</strong>
              <span>Task mutations are frozen until the violation is resolved.</span>
            </div>
          ) : null}

          {task.conflict ? (
            <div className="task-integrity-alert" role="alert">
              <strong>Conflict detected</strong>
              <span>Task mutations are frozen until current project state is reconciled.</span>
            </div>
          ) : null}

          <div className="task-integrity-scope">
            <strong>Approved scope</strong>
            {task.scopeSummary.length > 0 ? (
              <ul>
                {task.scopeSummary.map((item) => <li key={item}>{item}</li>)}
              </ul>
            ) : (
              <span>No writable source scope.</span>
            )}
          </div>

          <div className="task-integrity-budget">
            <strong>Remaining budget</strong>
            <span>{task.remainingBudget.existingFiles} existing files</span>
            <span>{task.remainingBudget.newFiles} new files</span>
            <span>{task.remainingBudget.changedLines} changed lines</span>
          </div>

          <div className="task-integrity-verification">
            <strong>{verificationLabels[task.verification.requiredLevel] ?? task.verification.requiredLevel}</strong>
            <span>
              {task.verification.evidenceCount} evidence{task.verification.evidenceCount === 1 ? "" : " records"}
            </span>
            {task.verification.complete ? <span>Verification complete</span> : null}
            {task.pendingReview ? <span>Review pending</span> : null}
          </div>

          {pendingExpansions.length > 0 ? (
            <div className="task-integrity-expansions">
              <strong>Pending scope expansion</strong>
              {pendingExpansions.map((expansion) => (
                <article key={expansion.expansionId}>
                  <div>
                    <code>{expansion.relativePath}</code>
                    <span>
                      {activeTasks.find((activeTask) => activeTask.taskId === expansion.taskId)?.intentSummary
                        ?? expansion.taskId}
                    </span>
                    <span>{expansion.reason}</span>
                  </div>
                  <div className="change-actions">
                    <button
                      className="secondary-button reject-button"
                      type="button"
                      disabled={busyId !== null}
                      onClick={() => actOnExpansion(expansion.expansionId, "reject")}
                    >
                      Reject
                    </button>
                    <button
                      className="primary-button"
                      type="button"
                      disabled={busyId !== null}
                      onClick={() => actOnExpansion(expansion.expansionId, "approve")}
                    >
                      Approve
                    </button>
                  </div>
                </article>
              ))}
            </div>
          ) : null}

          {task.completionReceipt ? (
            <div className="task-integrity-receipt">
              <strong>Integrity Receipt</strong>
              <span>{task.completionReceipt.changedPathCount} changed paths</span>
              <span>{task.completionReceipt.acceptedScopeExpansions} approved scope expansion{task.completionReceipt.acceptedScopeExpansions === 1 ? "" : "s"}</span>
            </div>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}

export default TaskIntegrityPanel;
