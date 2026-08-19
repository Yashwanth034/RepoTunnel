import type { ChangeRecord } from "../types";

type PendingChangeReviewProps = {
  changes: ChangeRecord[];
  busyId: string | null;
  onApprove: (change: ChangeRecord) => void;
  onReject: (change: ChangeRecord) => void;
};

function formatTime(timestamp: number): string {
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

function basename(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/\/$/, "");
  return normalized.split("/").filter(Boolean).pop() ?? path;
}

export default function PendingChangeReview({
  changes,
  busyId,
  onApprove,
  onReject,
}: PendingChangeReviewProps) {
  const pending = changes.filter((change) => change.status === "pending");
  if (pending.length === 0) return null;

  return (
    <section className="change-section" aria-labelledby="pending-change-title">
      <div className="section-heading change-heading">
        <div>
          <span className="section-kicker">Review mode</span>
          <h2 id="pending-change-title">Pending AI changes</h2>
          <p>
            This project is using review mode. Apply a proposal here, or switch the project to AI auto
            mode to let compatible MCP edits apply immediately with version protection.
          </p>
        </div>
        <span className="pending-count">{pending.length} pending</span>
      </div>

      <div className="change-list">
        {pending.map((change) => {
          const busy = busyId === change.id;
          return (
            <article className="change-card pending" key={change.id}>
              <div className="change-card-header">
                <div className="change-copy">
                  <div className="change-meta">
                    <span className="change-status pending">Needs review</span>
                    <span>{change.workspaceName}</span>
                    <span aria-hidden="true">•</span>
                    <span>{formatTime(change.createdAt)}</span>
                  </div>
                  <div className="change-file-row">
                    <h3 title={change.primaryPath}>{basename(change.primaryPath)}</h3>
                    <span className="change-operation">{change.summary}</span>
                  </div>
                </div>

                <div className="change-actions">
                  <button
                    className="secondary-button reject-button"
                    type="button"
                    disabled={busy}
                    onClick={() => onReject(change)}
                  >
                    Reject
                  </button>
                  <button
                    className="primary-button"
                    type="button"
                    disabled={busy}
                    onClick={() => onApprove(change)}
                  >
                    {busy ? "Applying…" : "Apply"}
                  </button>
                </div>
              </div>

              <details className="change-details">
                <summary>View proposal</summary>
                <div className="change-details-body">
                  <div className="change-detail-row">
                    <span>Path</span>
                    <code>
                      {change.secondaryPath
                        ? `${change.primaryPath} → ${change.secondaryPath}`
                        : change.primaryPath}
                    </code>
                  </div>
                  {change.diff ? <pre className="change-diff">{change.diff}</pre> : null}
                  {change.error ? <p className="change-error">{change.error}</p> : null}
                </div>
              </details>
            </article>
          );
        })}
      </div>
    </section>
  );
}
