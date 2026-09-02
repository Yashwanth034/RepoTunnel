import { useCallback, useEffect, useMemo, useState } from "react";
import { getProjectSetup, prepareProject } from "../lib/backend";
import type { ProjectSetupStatus, Workspace } from "../types";

type ProjectSetupPanelProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  onNotice: (message: string) => void;
};

function ProjectSetupPanel({ workspaces, selectedWorkspaceId, onNotice }: ProjectSetupPanelProps) {
  const workspace = useMemo(
    () => workspaces.find((item) => item.id === selectedWorkspaceId) ?? workspaces[0] ?? null,
    [selectedWorkspaceId, workspaces],
  );
  const [status, setStatus] = useState<ProjectSetupStatus | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    if (!workspace) {
      setStatus(null);
      return;
    }
    try {
      setStatus(await getProjectSetup(workspace.id));
    } catch (error) {
      onNotice(`Project setup: ${error instanceof Error ? error.message : String(error)}`);
    }
  }, [onNotice, workspace]);

  useEffect(() => {
    refresh().catch(() => undefined);
  }, [refresh]);

  async function prepare() {
    if (!workspace || !status?.setupNeeded) return;
    setBusy(true);
    try {
      const result = await prepareProject(workspace.id);
      setStatus(result.setup);
      onNotice(`${workspace.name} is prepared and ready.`);
    } catch (error) {
      onNotice(`Project setup failed: ${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setBusy(false);
    }
  }

  if (!workspace) return null;

  return (
    <section className="project-setup-panel">
      <div className="project-setup-heading">
        <div>
          <span className="section-kicker">Automatic project setup</span>
          <h2>{status?.setupNeeded ? "Setup needed" : "Project ready"}</h2>
          <p>{status ? `${status.framework}${status.packageManager ? ` · ${status.packageManager}` : ""}` : "Detecting framework and project tools…"}</p>
        </div>
        <span className={`project-ready-pill ${status?.setupNeeded ? "needs-setup" : "ready"}`}>{status?.setupNeeded ? "Needs setup" : "Ready"}</span>
      </div>

      {status ? (
        <div className="project-setup-details">
          <div><span>Dependencies</span><strong>{status.dependenciesReady ? "Ready" : "Not installed"}</strong></div>
          <div><span>Dev command</span><strong>{status.devCommand ?? "Not detected"}</strong></div>
          <div><span>Preview</span><strong>{status.devUrl ?? "Detected at runtime"}</strong></div>
          {status.setupNeeded && status.setupCommand ? (
            <button className="primary-button" type="button" disabled={busy} onClick={() => void prepare()}>{busy ? "Preparing…" : "Prepare project"}</button>
          ) : (
            <button className="secondary-button" type="button" disabled={busy} onClick={() => void refresh()}>Refresh</button>
          )}
        </div>
      ) : null}
      {status?.notes.length ? <p className="project-setup-note">{status.notes[0]}</p> : null}
    </section>
  );
}

export default ProjectSetupPanel;
