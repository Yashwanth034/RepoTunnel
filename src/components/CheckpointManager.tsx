import { useCallback, useEffect, useMemo, useState } from "react";
import {
  clearCheckpoints,
  compareCheckpoint,
  deleteCheckpoint,
  listCheckpoints,
  renameCheckpoint,
  restoreCheckpoint,
  setCheckpointPinned,
} from "../lib/backend";
import type {
  CheckpointComparison,
  CheckpointSummary,
  Workspace,
} from "../types";
import ConfirmationDialog from "./ConfirmationDialog";
import { NavIcon } from "./AppSidebar";

const PAGE_SIZE = 20;

type CheckpointGroup = {
  key: string;
  label: string;
  checkpoints: CheckpointSummary[];
};

type CheckpointManagerProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  onError: (message: string) => void;
  onNotice: (message: string) => void;
  onRestored: () => void;
};

function formatTime(timestamp: number): string {
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

function dateGroupLabel(timestamp: number): string {
  const value = new Date(timestamp);
  const today = new Date();
  const todayStart = new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime();
  const valueStart = new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime();
  const dayDifference = Math.round((todayStart - valueStart) / 86_400_000);
  if (dayDifference === 0) return "Today";
  if (dayDifference === 1) return "Yesterday";
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    year: "numeric",
  }).format(value);
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function CheckpointManager({
  workspaces,
  selectedWorkspaceId,
  onError,
  onNotice,
  onRestored,
}: CheckpointManagerProps) {
  const [checkpoints, setCheckpoints] = useState<CheckpointSummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [comparison, setComparison] = useState<CheckpointComparison | null>(null);
  const [restoreTarget, setRestoreTarget] = useState<CheckpointSummary | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<CheckpointSummary | null>(null);
  const [showAllProjects, setShowAllProjects] = useState(false);
  const [search, setSearch] = useState("");
  const [visibleCount, setVisibleCount] = useState(PAGE_SIZE);
  const [namingId, setNamingId] = useState<string | null>(null);
  const [nameDraft, setNameDraft] = useState("");
  const [clearOpen, setClearOpen] = useState(false);
  const [clearBusy, setClearBusy] = useState(false);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setCheckpoints(await listCheckpoints());
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setLoading(false);
    }
  }, [onError]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    setVisibleCount(PAGE_SIZE);
    setNamingId(null);
  }, [selectedWorkspaceId, showAllProjects, search]);

  const scoped = useMemo(() => {
    if (showAllProjects || !selectedWorkspaceId) return checkpoints;
    return checkpoints.filter((checkpoint) => checkpoint.workspaceId === selectedWorkspaceId);
  }, [checkpoints, selectedWorkspaceId, showAllProjects]);

  const filtered = useMemo(() => {
    const query = search.trim().toLowerCase();
    return scoped
      .filter((checkpoint) => {
        if (!query) return true;
        return [checkpoint.name ?? "", checkpoint.workspaceName, checkpoint.id]
          .join(" ")
          .toLowerCase()
          .includes(query);
      })
      .sort((left, right) => {
        if (left.pinned !== right.pinned) return left.pinned ? -1 : 1;
        return right.createdAt - left.createdAt;
      });
  }, [scoped, search]);

  const visible = filtered.slice(0, visibleCount);
  const groups = useMemo(() => {
    const grouped = new Map<string, CheckpointGroup>();
    for (const checkpoint of visible) {
      const key = checkpoint.pinned ? "pinned" : dateGroupLabel(checkpoint.createdAt);
      const label = checkpoint.pinned ? "Pinned" : key;
      const group = grouped.get(key) ?? { key, label, checkpoints: [] };
      group.checkpoints.push(checkpoint);
      grouped.set(key, group);
    }
    return Array.from(grouped.values());
  }, [visible]);

  const workspaceById = useMemo(
    () => new Map(workspaces.map((workspace) => [workspace.id, workspace])),
    [workspaces],
  );

  async function inspect(checkpoint: CheckpointSummary) {
    setBusyId(checkpoint.id);
    try {
      setComparison(await compareCheckpoint(checkpoint.workspaceId, checkpoint.id));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function confirmRestore() {
    if (!restoreTarget) return;
    const checkpoint = restoreTarget;
    setBusyId(checkpoint.id);
    try {
      const result = await restoreCheckpoint(checkpoint.workspaceId, checkpoint.id);
      setRestoreTarget(null);
      setComparison(null);
      await refresh();
      onRestored();
      onNotice(
        `Restored ${result.restoredFiles} files from checkpoint. RepoTunnel created a pre-restore checkpoint first for recovery.`,
      );
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function confirmDelete() {
    if (!deleteTarget) return;
    const checkpoint = deleteTarget;
    setBusyId(checkpoint.id);
    try {
      await deleteCheckpoint(checkpoint.workspaceId, checkpoint.id);
      setDeleteTarget(null);
      if (comparison?.checkpoint.id === checkpoint.id) setComparison(null);
      await refresh();
      onNotice("Checkpoint deleted.");
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function saveName(checkpoint: CheckpointSummary) {
    setBusyId(checkpoint.id);
    try {
      const updated = await renameCheckpoint(
        checkpoint.workspaceId,
        checkpoint.id,
        nameDraft.trim() || null,
      );
      setCheckpoints((current) =>
        current.map((item) =>
          item.workspaceId === updated.workspaceId && item.id === updated.id ? updated : item,
        ),
      );
      if (comparison?.checkpoint.id === updated.id) {
        setComparison((current) => current ? { ...current, checkpoint: updated } : null);
      }
      setNamingId(null);
      onNotice(updated.name ? "Checkpoint renamed." : "Checkpoint name cleared.");
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function togglePinned(checkpoint: CheckpointSummary) {
    setBusyId(checkpoint.id);
    try {
      const updated = await setCheckpointPinned(
        checkpoint.workspaceId,
        checkpoint.id,
        !checkpoint.pinned,
      );
      setCheckpoints((current) =>
        current.map((item) =>
          item.workspaceId === updated.workspaceId && item.id === updated.id ? updated : item,
        ),
      );
      onNotice(updated.pinned ? "Checkpoint pinned." : "Checkpoint unpinned.");
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusyId(null);
    }
  }

  async function confirmClear() {
    setClearBusy(true);
    try {
      const workspaceId = showAllProjects || !selectedWorkspaceId ? undefined : selectedWorkspaceId;
      const result = await clearCheckpoints(workspaceId);
      setComparison(null);
      setClearOpen(false);
      setNamingId(null);
      await refresh();
      onNotice(
        `Cleared ${result.removedCheckpoints} ${result.removedCheckpoints === 1 ? "checkpoint" : "checkpoints"}. Current project files were not changed.`,
      );
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setClearBusy(false);
    }
  }

  const clearScope = showAllProjects || !selectedWorkspaceId
    ? "all approved projects"
    : workspaceById.get(selectedWorkspaceId)?.name ?? "the active project";

  return (
    <section className="checkpoint-manager" aria-labelledby="checkpoint-manager-title">
      <div className="section-heading checkpoint-manager-heading">
        <div>
          <span className="section-kicker">Recovery</span>
          <h2 id="checkpoint-manager-title">Checkpoints</h2>
          <p>Restore an earlier AI-accessible project state without touching protected secrets or ignored build folders.</p>
        </div>
        <div className="checkpoint-toolbar">
          {workspaces.length > 1 ? (
            <button
              type="button"
              className="secondary-button"
              onClick={() => setShowAllProjects((current) => !current)}
              disabled={clearBusy}
            >
              {showAllProjects ? "Active project only" : "All projects"}
            </button>
          ) : null}
          <button type="button" className="secondary-button" onClick={() => void refresh()} disabled={loading || clearBusy}>
            {loading ? "Refreshing…" : "Refresh"}
          </button>
        </div>
      </div>

      {scoped.length > 0 ? (
        <div className="checkpoint-management-toolbar">
          <label className="checkpoint-search">
            <span className="sr-only">Search checkpoints</span>
            <input
              type="search"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder="Search checkpoints"
            />
          </label>
          <button
            type="button"
            className="secondary-button checkpoint-clear-all"
            onClick={() => setClearOpen(true)}
            disabled={clearBusy}
          >
            Clear All Checkpoints
          </button>
        </div>
      ) : null}

      {scoped.length === 0 ? (
        <div className="checkpoint-empty">
          <NavIcon name="checkpoint" size={22} />
          <div>
            <strong>No checkpoints for this project yet</strong>
            <p>Create one from Home before a large AI change.</p>
          </div>
        </div>
      ) : filtered.length === 0 ? (
        <div className="checkpoint-empty">
          <NavIcon name="checkpoint" size={22} />
          <div>
            <strong>No matching checkpoints</strong>
            <p>Try another checkpoint name or project search.</p>
          </div>
        </div>
      ) : (
        <div className="checkpoint-list">
          {groups.map((group) => (
            <div className="checkpoint-date-group" key={group.key}>
              <div className="checkpoint-date-heading"><span>{group.label}</span></div>
              {group.checkpoints.map((checkpoint) => {
                const workspace = workspaceById.get(checkpoint.workspaceId);
                const busy = busyId === checkpoint.id;
                const naming = namingId === checkpoint.id;
                return (
                  <article className={`checkpoint-row ${checkpoint.pinned ? "pinned" : ""}`} key={`${checkpoint.workspaceId}:${checkpoint.id}`}>
                    <div className="checkpoint-row-icon"><NavIcon name="checkpoint" size={18} /></div>
                    <div className="checkpoint-row-copy">
                      {naming ? (
                        <input
                          className="checkpoint-name-input"
                          value={nameDraft}
                          maxLength={80}
                          autoFocus
                          onChange={(event) => setNameDraft(event.target.value)}
                          onKeyDown={(event) => {
                            if (event.key === "Enter") void saveName(checkpoint);
                            if (event.key === "Escape" && !busy) setNamingId(null);
                          }}
                          aria-label="Checkpoint name"
                        />
                      ) : (
                        <strong>{checkpoint.name || checkpoint.workspaceName}</strong>
                      )}
                      <span>
                        {checkpoint.name ? `${checkpoint.workspaceName} · ` : ""}{formatTime(checkpoint.createdAt)}
                        {checkpoint.pinned ? <span className="checkpoint-pinned-label">Pinned</span> : null}
                      </span>
                      <small>{checkpoint.fileCount} files · {formatBytes(checkpoint.totalBytes)}</small>
                    </div>
                    {workspace?.accessMode === "readOnly" ? (
                      <div className="checkpoint-row-state">
                        <span className="checkpoint-readonly">Read only</span>
                      </div>
                    ) : null}
                    <div className={`checkpoint-row-actions ${naming ? "naming" : ""}`}>
                      {naming ? (
                        <>
                          <button type="button" className="secondary-button" disabled={busy} onClick={() => void saveName(checkpoint)}>
                            {busy ? "Saving…" : "Save"}
                          </button>
                          <button type="button" className="secondary-button" disabled={busy} onClick={() => setNamingId(null)}>
                            Cancel
                          </button>
                        </>
                      ) : (
                        <>
                          <button
                            type="button"
                            className={`icon-button checkpoint-pin ${checkpoint.pinned ? "active" : ""}`}
                            disabled={busy}
                            onClick={() => void togglePinned(checkpoint)}
                            aria-label={checkpoint.pinned ? "Unpin checkpoint" : "Pin checkpoint"}
                            title={checkpoint.pinned ? "Unpin checkpoint" : "Pin checkpoint"}
                          >
                            ★
                          </button>
                          <button
                            type="button"
                            className="icon-button checkpoint-rename"
                            disabled={busy}
                            onClick={() => {
                              setNamingId(checkpoint.id);
                              setNameDraft(checkpoint.name ?? "");
                            }}
                            aria-label="Rename checkpoint"
                            title="Rename checkpoint"
                          >
                            ✎
                          </button>
                          <button type="button" className="secondary-button" disabled={busy} onClick={() => void inspect(checkpoint)}>
                            {busy ? "Checking…" : "Compare"}
                          </button>
                          <button
                            type="button"
                            className="secondary-button"
                            disabled={busy || workspace?.accessMode === "readOnly"}
                            onClick={() => setRestoreTarget(checkpoint)}
                            title={workspace?.accessMode === "readOnly" ? "Switch this project to read + write before restoring" : "Restore checkpoint"}
                          >
                            Restore
                          </button>
                          <button type="button" className="icon-button checkpoint-delete" disabled={busy} onClick={() => setDeleteTarget(checkpoint)} aria-label="Delete checkpoint" title="Delete checkpoint">
                            ×
                          </button>
                        </>
                      )}
                    </div>
                  </article>
                );
              })}
            </div>
          ))}

          {visibleCount < filtered.length ? (
            <div className="checkpoint-load-more">
              <button type="button" className="secondary-button" onClick={() => setVisibleCount((count) => count + PAGE_SIZE)}>
                Load More
              </button>
              <span>{Math.min(visibleCount, filtered.length)} of {filtered.length}</span>
            </div>
          ) : null}
        </div>
      )}

      {comparison ? (
        <div className="checkpoint-comparison">
          <div className="checkpoint-comparison-heading">
            <div>
              <span className="section-kicker">Compared with current project</span>
              <strong>{comparison.checkpoint.name || formatTime(comparison.checkpoint.createdAt)}</strong>
            </div>
            <button type="button" className="icon-button" onClick={() => setComparison(null)} aria-label="Close comparison">×</button>
          </div>
          <div className="checkpoint-comparison-stats">
            <div><strong>{comparison.modifiedCount}</strong><span>modified</span></div>
            <div><strong>{comparison.addedCount}</strong><span>new now</span></div>
            <div><strong>{comparison.deletedCount}</strong><span>missing now</span></div>
          </div>
          {comparison.modified.length + comparison.added.length + comparison.deleted.length === 0 ? (
            <p className="checkpoint-match">Current project matches this checkpoint.</p>
          ) : (
            <div className="checkpoint-path-groups">
              {comparison.modified.length > 0 ? <PathGroup title="Modified since checkpoint" paths={comparison.modified} /> : null}
              {comparison.added.length > 0 ? <PathGroup title="Added since checkpoint" paths={comparison.added} /> : null}
              {comparison.deleted.length > 0 ? <PathGroup title="Deleted since checkpoint" paths={comparison.deleted} /> : null}
            </div>
          )}
        </div>
      ) : null}

      {restoreTarget ? (
        <ConfirmationDialog
          title="Restore checkpoint?"
          message={`RepoTunnel will restore “${restoreTarget.name || restoreTarget.workspaceName}” to this checkpoint for all AI-accessible files. New AI-accessible files created after the checkpoint will be removed. Protected/ignored files stay untouched. A fresh pre-restore checkpoint is created automatically first.`}
          confirmLabel="Restore checkpoint"
          busy={busyId === restoreTarget.id}
          onCancel={() => setRestoreTarget(null)}
          onConfirm={() => void confirmRestore()}
        />
      ) : null}

      {deleteTarget ? (
        <ConfirmationDialog
          title="Delete checkpoint?"
          message={`Delete ${deleteTarget.name ? `“${deleteTarget.name}”` : `this ${formatTime(deleteTarget.createdAt)} checkpoint`} for “${deleteTarget.workspaceName}”? This does not change the project itself.`}
          confirmLabel="Delete checkpoint"
          busy={busyId === deleteTarget.id}
          onCancel={() => setDeleteTarget(null)}
          onConfirm={() => void confirmDelete()}
        />
      ) : null}

      {clearOpen ? (
        <ConfirmationDialog
          title="Clear all checkpoints?"
          message={`RepoTunnel will delete all saved checkpoints for ${clearScope}, including pinned checkpoints. Current project files will not be changed.`}
          confirmLabel="Clear All Checkpoints"
          busy={clearBusy}
          busyLabel="Clearing…"
          onCancel={() => !clearBusy && setClearOpen(false)}
          onConfirm={() => void confirmClear()}
        />
      ) : null}
    </section>
  );
}

function PathGroup({ title, paths }: { title: string; paths: string[] }) {
  return (
    <details className="checkpoint-path-group">
      <summary>{title} <span>{paths.length}</span></summary>
      <div className="checkpoint-path-list">
        {paths.map((path) => <code key={path}>{path}</code>)}
      </div>
    </details>
  );
}

export default CheckpointManager;
