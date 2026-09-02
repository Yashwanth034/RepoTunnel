import { useCallback, useEffect, useMemo, useState } from "react";
import { getProjectMemory, updateProjectMemory } from "../lib/backend";
import type { ProjectMemory, Workspace } from "../types";

type ProjectMemoryPanelProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  onNotice: (message: string) => void;
};

function lines(value: string): string[] {
  return value.split("\n").map((item) => item.trim()).filter(Boolean);
}

function ProjectMemoryPanel({ workspaces, selectedWorkspaceId, onNotice }: ProjectMemoryPanelProps) {
  const workspace = useMemo(
    () => workspaces.find((item) => item.id === selectedWorkspaceId) ?? workspaces[0] ?? null,
    [selectedWorkspaceId, workspaces],
  );
  const [memory, setMemory] = useState<ProjectMemory | null>(null);
  const [summary, setSummary] = useState("");
  const [goals, setGoals] = useState("");
  const [decisions, setDecisions] = useState("");
  const [preferences, setPreferences] = useState("");
  const [nextSteps, setNextSteps] = useState("");
  const [editing, setEditing] = useState(false);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    if (!workspace) return;
    try {
      const value = await getProjectMemory(workspace.id);
      setMemory(value);
      setSummary(value.summary);
      setGoals(value.goals.join("\n"));
      setDecisions(value.decisions.join("\n"));
      setPreferences(value.preferences.join("\n"));
      setNextSteps(value.nextSteps.join("\n"));
    } catch (error) {
      onNotice(`Project memory: ${error instanceof Error ? error.message : String(error)}`);
    }
  }, [onNotice, workspace]);

  useEffect(() => {
    setEditing(false);
    load().catch(() => undefined);
  }, [load]);

  async function save() {
    if (!workspace) return;
    setBusy(true);
    try {
      const value = await updateProjectMemory(workspace.id, {
        summary,
        goals: lines(goals),
        decisions: lines(decisions),
        preferences: lines(preferences),
        nextSteps: lines(nextSteps),
      });
      setMemory(value);
      setEditing(false);
      onNotice("Project memory saved. Connected AIs can use it when resuming this project.");
    } catch (error) {
      onNotice(`Could not save project memory: ${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setBusy(false);
    }
  }

  if (!workspace) return null;
  const empty = !memory || (!memory.summary && memory.goals.length === 0 && memory.decisions.length === 0 && memory.preferences.length === 0 && memory.nextSteps.length === 0);

  return (
    <section className="project-memory-panel">
      <div className="project-memory-heading">
        <div>
          <span className="section-kicker">Project memory</span>
          <h2>Remember the important context</h2>
          <p>Goals, decisions and preferences stay with this project across later AI chats without adding files to the repository.</p>
        </div>
        <button className="secondary-button" type="button" onClick={() => setEditing((value) => !value)}>{editing ? "Close editor" : empty ? "Add memory" : "Edit memory"}</button>
      </div>

      {editing ? (
        <div className="project-memory-editor">
          <label>Project summary<textarea rows={3} value={summary} onChange={(event) => setSummary(event.target.value)} placeholder="What this project is and where it currently stands" /></label>
          <div className="project-memory-grid">
            <label>Goals<textarea rows={4} value={goals} onChange={(event) => setGoals(event.target.value)} placeholder="One goal per line" /></label>
            <label>Preferences / constraints<textarea rows={4} value={preferences} onChange={(event) => setPreferences(event.target.value)} placeholder="One preference per line" /></label>
            <label>Important decisions<textarea rows={4} value={decisions} onChange={(event) => setDecisions(event.target.value)} placeholder="One decision per line" /></label>
            <label>Next steps<textarea rows={4} value={nextSteps} onChange={(event) => setNextSteps(event.target.value)} placeholder="One next step per line" /></label>
          </div>
          <div className="project-memory-actions"><button className="primary-button" type="button" disabled={busy} onClick={() => void save()}>{busy ? "Saving…" : "Save memory"}</button></div>
        </div>
      ) : empty ? (
        <div className="project-memory-empty">No saved context yet. The AI can also update this memory after meaningful project decisions.</div>
      ) : (
        <div className="project-memory-summary">
          {memory?.summary ? <p>{memory.summary}</p> : null}
          <div className="project-memory-chips">
            {memory?.goals.slice(0, 4).map((item) => <span key={`goal:${item}`}>Goal · {item}</span>)}
            {memory?.preferences.slice(0, 4).map((item) => <span key={`pref:${item}`}>Preference · {item}</span>)}
            {memory?.nextSteps.slice(0, 3).map((item) => <span key={`next:${item}`}>Next · {item}</span>)}
          </div>
        </div>
      )}
    </section>
  );
}

export default ProjectMemoryPanel;
