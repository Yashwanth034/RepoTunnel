import type { CheckpointSummary, SafetyScanResult } from "../types";
import { NavIcon } from "./AppSidebar";

type HomeFeatureDialogProps = {
  checkpoint: CheckpointSummary | null;
  safetyScan: SafetyScanResult | null;
  onClose: () => void;
};

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function HomeFeatureDialog({ checkpoint, safetyScan, onClose }: HomeFeatureDialogProps) {
  if (!checkpoint && !safetyScan) return null;

  return (
    <div className="feature-dialog-backdrop" role="presentation" onMouseDown={onClose}>
      <section
        className="feature-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="feature-dialog-title"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="feature-dialog-header">
          <div>
            <span className="feature-dialog-kicker">{checkpoint ? "Checkpoint" : "Safety scan"}</span>
            <h3 id="feature-dialog-title">
              {checkpoint ? "Project state saved" : `${safetyScan?.workspaceName} protection report`}
            </h3>
          </div>
          <button type="button" onClick={onClose} aria-label="Close">×</button>
        </div>

        {checkpoint ? (
          <div className="checkpoint-result">
            <div className="feature-result-icon pass"><NavIcon name="checkpoint" size={25} /></div>
            <p>RepoTunnel saved an isolated local checkpoint without changing the project.</p>
            <div className="feature-result-stats">
              <div><strong>{checkpoint.fileCount}</strong><span>files saved</span></div>
              <div><strong>{formatBytes(checkpoint.totalBytes)}</strong><span>checkpoint size</span></div>
              <div><strong>{checkpoint.workspaceName}</strong><span>project</span></div>
            </div>
            <small>Protected secrets, ignored build/dependency folders, and unsafe symlink targets are not copied into checkpoints.</small>
          </div>
        ) : safetyScan ? (
          <div className="safety-result">
            <div className={`safety-summary ${safetyScan.level}`}>
              <div className="feature-result-icon"><NavIcon name="shield" size={25} /></div>
              <div>
                <strong>{safetyScan.level === "protected" ? "Protected" : "Review recommended"}</strong>
                <span>{safetyScan.fileCount} accessible files · {safetyScan.ignoredEntryCount} ignored entries · automatic version history enabled</span>
              </div>
            </div>
            <div className="safety-check-list">
              {safetyScan.checks.map((check) => (
                <details className="safety-check-row safety-check-details" key={check.key}>
                  <summary>
                    <span className={`safety-check-indicator ${check.status}`}>
                      {check.status === "pass" ? "✓" : "!"}
                    </span>
                    <div><strong>{check.title}</strong><p>{check.detail}</p></div>
                    <span className="safety-expand">Details</span>
                  </summary>
                  {check.items.length > 0 ? (
                    <ul className="safety-detail-items">
                      {check.items.map((item) => <li key={item}>{item}</li>)}
                    </ul>
                  ) : null}
                </details>
              ))}


              <details className="safety-check-row safety-check-details">
                <summary>
                  <span className="safety-check-indicator pass">✓</span>
                  <div><strong>Local approval gates</strong><p>Applied AI file edits are version-backed; review-mode proposals and sensitive execution/Git actions keep local approval where configured.</p></div>
                  <span className="safety-expand">Details</span>
                </summary>
                <ul className="safety-detail-items">
                  <li>Queued file approvals: {safetyScan.pendingReviews}.</li>
                  <li>Sandbox commands still follow the project command policy.</li>
                  <li>Git staging and commits still require explicit local approval.</li>
                  <li>Restore actions require local confirmation.</li>
                </ul>
              </details>
            </div>
          </div>
        ) : null}

        <div className="feature-dialog-actions">
          <button type="button" className="primary" onClick={onClose}>Done</button>
        </div>
      </section>
    </div>
  );
}

export default HomeFeatureDialog;
