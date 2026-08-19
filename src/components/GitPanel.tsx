import { useCallback, useEffect, useMemo, useState } from "react";
import ConfirmationDialog from "./ConfirmationDialog";
import {
  approveGitAction,
  getGitDiff,
  getGitLog,
  getGitStatus,
  listGitActions,
  rejectGitAction,
  requestGitCommit,
  requestGitRestoreFile,
  requestGitStage,
} from "../lib/backend";
import type {
  GitActionRecord,
  GitActionStatus,
  GitCommitSummary,
  GitDiff,
  GitFileChange,
  GitRepositoryStatus,
  Workspace,
} from "../types";

type GitPanelProps = {
  workspaces: Workspace[];
  gatewayRunning: boolean;
  onError: (message: string) => void;
  onChangeQueued: () => void;
};

const actionLabels: Record<GitActionStatus, string> = {
  pending: "Pending approval",
  applied: "Applied",
  rejected: "Rejected",
  failed: "Failed",
};

function formatDate(timestamp: number): string {
  if (!timestamp) return "Unknown time";
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

function changeLabel(change: GitFileChange): string {
  if (change.conflicted) return "Conflict";
  if (change.untracked) return "Untracked";
  if (change.staged && change.unstaged) return "Staged + modified";
  if (change.staged) return "Staged";
  if (change.unstaged) return "Modified";
  return `${change.indexStatus}${change.worktreeStatus}`.trim() || "Changed";
}

function GitPanel({ workspaces, gatewayRunning, onError, onChangeQueued }: GitPanelProps) {
  const [selectedWorkspaceId, setSelectedWorkspaceId] = useState("");
  const [status, setStatus] = useState<GitRepositoryStatus | null>(null);
  const [diff, setDiff] = useState<GitDiff | null>(null);
  const [showStagedDiff, setShowStagedDiff] = useState(false);
  const [commits, setCommits] = useState<GitCommitSummary[]>([]);
  const [actions, setActions] = useState<GitActionRecord[]>([]);
  const [commitMessage, setCommitMessage] = useState("");
  const [loading, setLoading] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [restoreTarget, setRestoreTarget] = useState<GitFileChange | null>(null);

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
    if (!selectedWorkspaceId) {
      setStatus(null);
      setDiff(null);
      setCommits([]);
      setActions([]);
      return;
    }
    setLoading(true);
    try {
      const [repository, activity] = await Promise.all([
        getGitStatus(selectedWorkspaceId),
        listGitActions(selectedWorkspaceId, 30),
      ]);
      setStatus(repository);
      setActions(activity);
      if (repository.available) {
        const [nextDiff, nextCommits] = await Promise.all([
          getGitDiff(selectedWorkspaceId, showStagedDiff),
          getGitLog(selectedWorkspaceId, 10),
        ]);
        setDiff(nextDiff);
        setCommits(nextCommits);
      } else {
        setDiff(null);
        setCommits([]);
      }
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setLoading(false);
    }
  }, [onError, selectedWorkspaceId, showStagedDiff]);

  useEffect(() => {
    refresh().catch(() => undefined);
  }, [refresh]);

  useEffect(() => {
    if (!gatewayRunning || !selectedWorkspaceId) return;
    const timer = window.setInterval(() => {
      Promise.all([
        getGitStatus(selectedWorkspaceId),
        listGitActions(selectedWorkspaceId, 30),
      ])
        .then(async ([repository, activity]) => {
          setStatus(repository);
          setActions(activity);
          if (repository.available) {
            const nextDiff = await getGitDiff(selectedWorkspaceId, showStagedDiff);
            setDiff(nextDiff);
          } else {
            setDiff(null);
          }
        })
        .catch(() => undefined);
    }, 3500);
    return () => window.clearInterval(timer);
  }, [gatewayRunning, selectedWorkspaceId, showStagedDiff]);

  async function requestCommit() {
    if (!selectedWorkspace || !commitMessage.trim()) return;
    setBusyId("commit");
    try {
      await requestGitCommit(selectedWorkspace.id, commitMessage);
      setCommitMessage("");
      await refresh();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function actOnAction(action: GitActionRecord, operation: "approve" | "reject") {
    setBusyId(action.id);
    try {
      if (operation === "approve") {
        await approveGitAction(action.id);
      } else {
        await rejectGitAction(action.id);
      }
      await refresh();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
      await refresh().catch(() => undefined);
    } finally {
      setBusyId(null);
    }
  }

  async function stageFile(change: GitFileChange) {
    if (!selectedWorkspace) return;
    setBusyId(`stage:${change.path}`);
    try {
      await requestGitStage(selectedWorkspace.id, [change.path]);
      await refresh();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function restoreFile(change: GitFileChange) {
    if (!selectedWorkspace) return;
    setBusyId(`restore:${change.path}`);
    try {
      await requestGitRestoreFile(selectedWorkspace.id, change.path);
      onChangeQueued();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  const pendingCount = actions.filter((action) => action.status === "pending").length;
  const restorableChanges = status?.changes.filter(
    (change) => change.unstaged && !change.staged && !change.untracked && !change.conflicted && !change.path.includes(" → "),
  ) ?? [];

  return (
    <section className="git-section" aria-labelledby="git-title">
      <div className="section-heading git-heading">
        <div>
          <span className="section-kicker">Git</span>
          <h2 id="git-title">Repository control</h2>
          <p>
            Inspect repository state, review live diffs, stage and commit safely, and restore tracked
            text files without leaving RepoTunnel.
          </p>
        </div>
        <div className="git-heading-actions">
          {pendingCount > 0 ? <span className="pending-count">{pendingCount} pending</span> : null}
          <button className="secondary-button" type="button" onClick={() => refresh()} disabled={loading}>
            {loading ? "Refreshing…" : "Refresh"}
          </button>
        </div>
      </div>

      {workspaces.length > 0 ? (
        <div className="git-workspace-picker">
          <select
            aria-label="Project for Git integration"
            value={selectedWorkspaceId}
            onChange={(event) => setSelectedWorkspaceId(event.target.value)}
          >
            {workspaces.map((workspace) => (
              <option key={workspace.id} value={workspace.id}>{workspace.name}</option>
            ))}
          </select>
          <span>
            {selectedWorkspace?.changePolicy === "automatic"
              ? "AI Auto · stage/commit automatic · push only on user request"
              : "AI Review · Git writes require local approval"}
          </span>
        </div>
      ) : null}

      {!selectedWorkspace ? (
        <div className="git-empty"><p>Add a project to inspect its Git repository.</p></div>
      ) : status && !status.available ? (
        <div className="git-unavailable">
          <strong>Git integration unavailable for this project</strong>
          <p>{status.message}</p>
        </div>
      ) : status?.available ? (
        <>
          <div className="git-status-grid">
            <div><span>Branch</span><strong>{status.detached ? "Detached HEAD" : status.branch ?? "No commits yet"}</strong></div>
            <div><span>Staged</span><strong>{status.stagedCount}</strong></div>
            <div><span>Modified</span><strong>{status.unstagedCount}</strong></div>
            <div><span>Untracked</span><strong>{status.untrackedCount}</strong></div>
            <div><span>Ahead / behind</span><strong>{status.ahead} / {status.behind}</strong></div>
            <div><span>Conflicts</span><strong>{status.conflictedCount}</strong></div>
          </div>

          <div className="git-columns">
            <div className="git-card">
              <div className="git-card-heading">
                <div>
                  <strong>Working tree</strong>
                  <span>{status.changes.length} changed paths</span>
                </div>
              </div>
              {status.changes.length === 0 ? (
                <div className="git-clean-state">
                  <span aria-hidden="true">✓</span>
                  <div><strong>Working tree clean</strong><p>No local file changes. New edits will appear here automatically.</p></div>
                </div>
              ) : (
                <div className="git-change-list">
                  {status.changes.slice(0, 60).map((change, index) => {
                    const canRestore = restorableChanges.includes(change);
                    const canStage =
                      (change.unstaged || change.untracked) &&
                      !change.conflicted &&
                      !change.path.includes(" → ");
                    return (
                      <div className="git-change-row" key={`${change.path}:${index}`}>
                        <div>
                          <span className={`git-change-status ${change.conflicted ? "conflicted" : change.staged ? "staged" : ""}`}>
                            {changeLabel(change)}
                          </span>
                          <code>{change.path}</code>
                        </div>
                        {canStage || canRestore ? (
                          <div className="git-row-actions">
                            {canRestore ? (
                              <button
                                className="secondary-button"
                                type="button"
                                disabled={busyId !== null || selectedWorkspace.accessMode === "readOnly"}
                                onClick={() => setRestoreTarget(change)}
                              >
                                {busyId === `restore:${change.path}` ? "Requesting…" : "Restore to HEAD"}
                              </button>
                            ) : null}
                            {canStage ? (
                              <button
                                className="secondary-button"
                                type="button"
                                disabled={busyId !== null || selectedWorkspace.accessMode === "readOnly"}
                                onClick={() => stageFile(change)}
                              >
                                {busyId === `stage:${change.path}`
                                  ? "Staging…"
                                  : selectedWorkspace.changePolicy === "automatic" ? "Stage" : "Request stage"}
                              </button>
                            ) : null}
                          </div>
                        ) : null}
                      </div>
                    );
                  })}
                </div>
              )}
            </div>

            <div className="git-card">
              <div className="git-card-heading diff-heading">
                <div>
                  <strong>{showStagedDiff ? "Staged diff" : "Unstaged diff"}</strong>
                  <span>External diff and textconv are disabled</span>
                </div>
                <div className="git-diff-tabs">
                  <button type="button" className={!showStagedDiff ? "active" : ""} onClick={() => setShowStagedDiff(false)}>Unstaged</button>
                  <button type="button" className={showStagedDiff ? "active" : ""} onClick={() => setShowStagedDiff(true)}>Staged</button>
                </div>
              </div>
              {diff?.content ? (
                <pre className="git-diff">{diff.content}{diff.truncated ? "\n… diff truncated …" : ""}</pre>
              ) : (
                <div className="git-clean-state diff-clean">
                  <span aria-hidden="true">✓</span>
                  <div>
                    <strong>No {showStagedDiff ? "staged" : "unstaged"} diff</strong>
                    <p>{status.changes.length === 0 ? "This branch currently matches the working tree." : "Switch diff type or select a changed file to inspect its state."}</p>
                  </div>
                </div>
              )}
            </div>
          </div>

          <div className="git-commit-card">
            <div>
              <strong>Commit staged changes</strong>
              <p>
                The staged diff is frozen before commit. In AI Auto the commit applies immediately; in AI Review
                the same frozen commit waits for your approval. Remote push remains a separate explicit user intent.
              </p>
            </div>
            <textarea
              value={commitMessage}
              onChange={(event) => setCommitMessage(event.target.value)}
              placeholder="Commit message"
              maxLength={5000}
              rows={3}
              disabled={selectedWorkspace.accessMode === "readOnly"}
            />
            <button
              className="primary-button"
              type="button"
              disabled={
                busyId !== null ||
                selectedWorkspace.accessMode === "readOnly" ||
                status.stagedCount === 0 ||
                status.conflictedCount > 0 ||
                !commitMessage.trim()
              }
              onClick={requestCommit}
            >
              {busyId === "commit" ? "Preparing…" : selectedWorkspace.changePolicy === "automatic" ? "Commit changes" : "Review commit"}
            </button>
          </div>

          <div className="git-columns git-bottom-columns">
            <div className="git-card">
              <div className="git-card-heading"><div><strong>Recent commits</strong><span>{commits.length} shown</span></div></div>
              {commits.length === 0 ? (
                <div className="git-empty"><p>No commits yet.</p></div>
              ) : (
                <div className="git-log-list">
                  {commits.map((commit) => (
                    <div className="git-log-row" key={commit.hash}>
                      <code>{commit.shortHash}</code>
                      <div><strong>{commit.subject}</strong><span>{commit.author} · {formatDate(commit.timestamp)}</span></div>
                    </div>
                  ))}
                </div>
              )}
            </div>

            <div className="git-card">
              <div className="git-card-heading"><div><strong>Git activity</strong><span>{actions.length} records</span></div></div>
              {actions.length === 0 ? (
                <div className="git-empty"><p>No Git actions requested yet.</p></div>
              ) : (
                <div className="git-action-list">
                  {actions.slice(0, 15).map((action) => (
                    <article className={`git-action ${action.status}`} key={action.id}>
                      <div className="git-action-header">
                        <div>
                          <span className={`change-status ${action.status}`}>{actionLabels[action.status]}</span>
                          <strong>{action.summary}</strong>
                          {action.commitHash ? <code>{action.commitHash.slice(0, 12)}</code> : null}
                        </div>
                        {action.status === "pending" ? (
                          <div className="change-actions">
                            <button className="secondary-button reject-button" type="button" disabled={busyId !== null} onClick={() => actOnAction(action, "reject")}>Reject</button>
                            <button className="primary-button" type="button" disabled={busyId !== null} onClick={() => actOnAction(action, "approve")}>{busyId === action.id ? "Applying…" : action.kind === "stage" ? "Approve stage" : "Approve commit"}</button>
                          </div>
                        ) : null}
                      </div>
                      {action.detail ? <pre className="git-action-diff">{action.detail}</pre> : null}
                      {action.error ? <p className="change-error">{action.error}</p> : null}
                    </article>
                  ))}
                </div>
              )}
            </div>
          </div>
        </>
      ) : (
        <div className="git-empty"><p>Checking repository status…</p></div>
      )}

      {restoreTarget ? (
        <ConfirmationDialog
          title="Restore file to HEAD?"
          message={`RepoTunnel will replace “${restoreTarget.path}” with the committed HEAD version. The change is still captured in RepoTunnel version history so you can move back afterward.`}
          confirmLabel="Restore to HEAD"
          busyLabel="Restoring…"
          busy={busyId === `restore:${restoreTarget.path}`}
          onCancel={() => !busyId && setRestoreTarget(null)}
          onConfirm={() => {
            const target = restoreTarget;
            void restoreFile(target).finally(() => setRestoreTarget(null));
          }}
        />
      ) : null}
    </section>
  );
}

export default GitPanel;
