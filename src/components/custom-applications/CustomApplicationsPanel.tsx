import { useCallback, useEffect, useState } from "react";
import ApplicationPicker from "./ApplicationPicker";
import {
  listCustomApplications,
  removeCustomApplication,
  startAiWorkspace,
} from "../../lib/backend";
import type { CustomApplication } from "../../types";

type CustomApplicationsPanelProps = {
  workspaceId: string | null;
  onError: (message: string) => void;
  onNotice: (message: string) => void;
};

function CustomApplicationsPanel({ workspaceId, onError, onNotice }: CustomApplicationsPanelProps) {
  const [applications, setApplications] = useState<CustomApplication[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setApplications(await listCustomApplications());
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setLoading(false);
    }
  }, [onError]);

  useEffect(() => {
    void refresh();
    const handleFocus = () => void refresh();
    window.addEventListener("focus", handleFocus);
    return () => window.removeEventListener("focus", handleFocus);
  }, [refresh]);

  async function openApplication(application: CustomApplication) {
    if (!workspaceId || busy !== null) return;
    setBusy(application.id);
    try {
      await startAiWorkspace(workspaceId, application.id);
      onNotice(application.name + " launched in isolated AI Workspace.");
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(null);
    }
  }

  async function removeApplication(application: CustomApplication) {
    setBusy(application.id);
    try {
      setApplications(await removeCustomApplication(application.id));
      window.dispatchEvent(new Event("repotunnel:applications-updated"));
      onNotice(`${application.name} was removed from AI Workspace access.`);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="interface-settings custom-ai-applications">
      <div className="history-retention-heading">
        <div>
          <strong>Your applications</strong>
          <span>Choose installed applications for AI Workspace.</span>
        </div>
        <button
          type="button"
          className="secondary-button"
          onClick={() => setPickerOpen(true)}
          disabled={busy !== null}
        >
          Add application
        </button>
      </div>

      {loading ? <div className="panel-empty">Checking approved applications…</div> : null}


      {applications.map((application) => (
        <div className="production-actions" key={application.id}>
          <div>
            <strong>{application.name}</strong>
            <small>Native app · AI Workspace approved</small>
          </div>
          <button type="button" className="secondary-button"
            disabled={!workspaceId || busy !== null}
            onClick={() => void openApplication(application)}>Open</button>
          <button
            type="button"
            className="secondary-button"
            onClick={() => void removeApplication(application)}
            disabled={busy !== null}
          >
            {busy === application.id ? "Removing…" : "Remove"}
          </button>
        </div>
      ))}

      <small>
        Approved native applications run in isolated AI Workspace. Flatpak/Snap shortcuts open on the system desktop. Protected system utilities cannot be added as AI Workspace applications.
      </small>
      <ApplicationPicker workspaceId={workspaceId} approvedIds={applications.map((a) => a.id)}
        open={pickerOpen} onClose={() => setPickerOpen(false)}
        onApproved={refresh} onError={onError} onNotice={onNotice} />
    </div>
  );
}

export default CustomApplicationsPanel;
