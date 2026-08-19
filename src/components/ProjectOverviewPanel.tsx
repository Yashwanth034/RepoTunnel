import { useEffect, useMemo, useState } from "react";
import { inspectProject } from "../lib/backend";
import type { ProjectSnapshot, Workspace } from "../types";

type ProjectOverviewPanelProps = {
  workspaces: Workspace[];
};

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

function ProjectOverviewPanel({ workspaces }: ProjectOverviewPanelProps) {
  const [workspaceId, setWorkspaceId] = useState(workspaces[0]?.id ?? "");
  const [snapshot, setSnapshot] = useState<ProjectSnapshot | null>(null);
  const [scanning, setScanning] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!workspaces.some((workspace) => workspace.id === workspaceId)) {
      setWorkspaceId(workspaces[0]?.id ?? "");
      setSnapshot(null);
    }
  }, [workspaces, workspaceId]);

  const selected = useMemo(
    () => workspaces.find((workspace) => workspace.id === workspaceId) ?? null,
    [workspaces, workspaceId],
  );

  async function scan() {
    if (!workspaceId) return;
    setScanning(true);
    setError(null);
    try {
      setSnapshot(await inspectProject(workspaceId));
    } catch (scanError) {
      setError(scanError instanceof Error ? scanError.message : String(scanError));
    } finally {
      setScanning(false);
    }
  }

  return (
    <section className="project-overview-section" aria-labelledby="project-overview-title">
      <div className="section-heading project-overview-heading">
        <div>
          <span className="section-kicker">Project intelligence</span>
          <h2 id="project-overview-title">Smart project index</h2>
          <p>
            Build a filtered code view that respects ignore rules and skips generated, binary, and
            oversized content before AI exploration.
          </p>
        </div>
        <div className="project-scan-actions">
          <select
            aria-label="Project to inspect"
            value={workspaceId}
            onChange={(event) => {
              setWorkspaceId(event.target.value);
              setSnapshot(null);
              setError(null);
            }}
            disabled={workspaces.length === 0 || scanning}
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
            onClick={scan}
            disabled={!selected || scanning}
          >
            {scanning ? "Scanning…" : snapshot ? "Refresh index" : "Inspect project"}
          </button>
        </div>
      </div>

      {error ? <p className="project-index-error">{error}</p> : null}

      {!snapshot ? (
        <div className="project-index-empty">
          <strong>{selected ? `Ready to inspect ${selected.name}` : "Add a project first"}</strong>
          <p>
            RepoTunnel will use .gitignore/.ignore rules, security filters, generated-folder rules,
            binary detection, and bounded tree limits.
          </p>
        </div>
      ) : (
        <div className="project-index-content">
          <div className="project-stats">
            <div><strong>{snapshot.overview.fileCount}</strong><span>Files</span></div>
            <div><strong>{snapshot.overview.textFileCount}</strong><span>Text/code</span></div>
            <div><strong>{snapshot.overview.binaryFileCount}</strong><span>Binary</span></div>
            <div><strong>{snapshot.overview.ignoredEntryCount}</strong><span>Ignored</span></div>
            <div><strong>{formatBytes(snapshot.overview.totalBytes)}</strong><span>Visible size</span></div>
          </div>

          <div className="project-index-columns">
            <div className="project-index-card">
              <span className="project-index-label">Languages</span>
              <div className="language-list">
                {snapshot.overview.languages.length === 0 ? (
                  <span className="muted-value">No source languages detected</span>
                ) : (
                  snapshot.overview.languages.slice(0, 10).map((language) => (
                    <span className="language-chip" key={language.name}>
                      {language.name} <b>{language.files}</b>
                    </span>
                  ))
                )}
              </div>
            </div>

            <div className="project-index-card">
              <span className="project-index-label">Project manifests</span>
              <div className="manifest-list">
                {snapshot.overview.manifests.length === 0 ? (
                  <span className="muted-value">No common manifests detected</span>
                ) : (
                  snapshot.overview.manifests.slice(0, 10).map((manifest) => (
                    <code key={manifest}>{manifest}</code>
                  ))
                )}
              </div>
            </div>
          </div>

          <div className="project-tree-card">
            <div className="project-tree-heading">
              <span className="project-index-label">Filtered project tree</span>
              <span>{snapshot.entries.length} entries{snapshot.overview.truncated ? "+" : ""}</span>
            </div>
            <div className="project-tree-list">
              {snapshot.entries.slice(0, 120).map((entry) => (
                <div className="project-tree-row" key={`${entry.kind}:${entry.path}`}>
                  <span aria-hidden="true">{entry.kind === "directory" ? "▸" : "·"}</span>
                  <code>{entry.path}</code>
                  {entry.language ? <em>{entry.language}</em> : null}
                  {entry.binary ? <em>binary</em> : null}
                  {entry.large ? <em>large</em> : null}
                </div>
              ))}
            </div>
            {snapshot.entries.length > 120 ? (
              <p className="project-tree-note">Showing the first 120 indexed entries in the desktop preview.</p>
            ) : null}
          </div>
        </div>
      )}
    </section>
  );
}

export default ProjectOverviewPanel;
