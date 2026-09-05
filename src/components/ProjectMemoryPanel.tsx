import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useMemo, useState } from "react";
import { getProjectMemory, getResumeSnapshot, updateProjectMemory } from "../lib/backend";
import type { ProjectMemory, ResumeSnapshot, Workspace } from "../types";

type ProjectMemoryPanelProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  onNotice: (message: string) => void;
};

function lines(value: string): string[] {
  return value.split("\n").map((item) => item.trim()).filter(Boolean);
}

function compact(value: string | null | undefined, fallback: string): string {
  const text = value?.trim();
  return text ? text : fallback;
}

function ProjectMemoryPanel({ workspaces, selectedWorkspaceId, onNotice }: ProjectMemoryPanelProps) {
  const workspace = useMemo(
    () => workspaces.find((item) => item.id === selectedWorkspaceId) ?? workspaces[0] ?? null,
    [selectedWorkspaceId, workspaces],
  );
  const [memory, setMemory] = useState<ProjectMemory | null>(null);
  const [resume, setResume] = useState<ResumeSnapshot | null>(null);
  const [summary, setSummary] = useState("");
  const [goals, setGoals] = useState("");
  const [decisions, setDecisions] = useState("");
  const [preferences, setPreferences] = useState("");
  const [nextSteps, setNextSteps] = useState("");
  const [editing, setEditing] = useState(false);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async (quiet = false) => {
    if (!workspace) return;
    try {
      const [memoryValue, resumeValue] = await Promise.all([
        getProjectMemory(workspace.id),
        getResumeSnapshot(workspace.id),
      ]);
      setMemory(memoryValue);
      setResume(resumeValue);
      setSummary(memoryValue.summary);
      setGoals(memoryValue.goals.join("\n"));
      setDecisions(memoryValue.decisions.join("\n"));
      setPreferences(memoryValue.preferences.join("\n"));
      setNextSteps(memoryValue.nextSteps.join("\n"));
    } catch (error) {
      if (!quiet) onNotice(`Project continuity: ${error instanceof Error ? error.message : String(error)}`);
    }
  }, [onNotice, workspace]);

  useEffect(() => {
    setEditing(false);
    void load();
  }, [load]);

  useEffect(() => {
    if (!workspace) return undefined;
    let disposed = false;
    let unlisten: (() => void) | null = null;
    let timer: number | null = null;

    void listen("repotunnel://activity-updated", () => {
      if (disposed) return;
      if (timer !== null) window.clearTimeout(timer);
      timer = window.setTimeout(() => void load(true), 180);
    }).then((remove) => {
      if (disposed) remove();
      else unlisten = remove;
    }).catch(() => undefined);

    return () => {
      disposed = true;
      if (timer !== null) window.clearTimeout(timer);
      unlisten?.();
    };
  }, [load, workspace]);

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
      await load(true);
      onNotice("Project context saved. Live progress continues to update automatically.");
    } catch (error) {
      onNotice(`Could not save project context: ${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setBusy(false);
    }
  }

  if (!workspace) return null;

  const empty = !memory || (!memory.summary && memory.goals.length === 0 && memory.decisions.length === 0 && memory.preferences.length === 0 && memory.nextSteps.length === 0);
  const git = resume?.brief.git;
  const shortHead = git?.head?.slice(0, 7) ?? "no HEAD";
  const current = git?.available
    ? `${git.branch ?? "detached"} · ${shortHead} · ${git.workingTree}`
    : "Git state unavailable";
  const completed = resume?.brief.lastCompleted[0]
    ?? resume?.milestones.find((item) => item.outcome === "completed")?.summary
    ?? "No completed milestone recorded yet";
  const next = resume?.brief.next[0]
    ?? resume?.context.savedNextSteps[0]
    ?? "No next step saved";
  const active = resume?.brief.active[0] ?? null;
  const needsAttention = resume?.brief.attentionRequired ?? false;
  const continuityLabel = active ? "In progress" : needsAttention ? "Needs attention" : "Ready to continue";

  return (
    <section className="project-memory-panel">
      <div className="project-memory-heading">
        <div>
          <span className="section-kicker">Project continuity</span>
          <h2>{continuityLabel}</h2>
          <p>RepoTunnel keeps the factual work state current automatically. New AI chats can resume from one small snapshot without replaying the full history.</p>
        </div>
        <button className="secondary-button" type="button" onClick={() => setEditing((value) => !value)}>
          {editing ? "Close context" : empty ? "Add context" : "Edit context"}
        </button>
      </div>

      <div className="continuity-status-grid">
        <div>
          <span>Current</span>
          <strong title={current}>{current}</strong>
        </div>
        <div>
          <span>{active ? "Working on" : "Last completed"}</span>
          <strong title={active ?? completed}>{compact(active ?? completed, "No activity yet")}</strong>
        </div>
        <div>
          <span>Next</span>
          <strong title={next}>{next}</strong>
        </div>
      </div>

      {resume?.context.memoryState === "stale" ? (
        <div className="continuity-note">Live Git/activity is newer than the saved semantic context. RepoTunnel is using the live factual state first.</div>
      ) : null}

      {editing ? (
        <div className="project-memory-editor">
          <label>Project summary<textarea rows={3} value={summary} onChange={(event) => setSummary(event.target.value)} placeholder="What this project is and the current goal" /></label>
          <div className="project-memory-grid">
            <label>Goals<textarea rows={4} value={goals} onChange={(event) => setGoals(event.target.value)} placeholder="One goal per line" /></label>
            <label>Preferences / constraints<textarea rows={4} value={preferences} onChange={(event) => setPreferences(event.target.value)} placeholder="One stable constraint per line" /></label>
            <label>Important decisions<textarea rows={4} value={decisions} onChange={(event) => setDecisions(event.target.value)} placeholder="One decision per line" /></label>
            <label>Intended next steps<textarea rows={4} value={nextSteps} onChange={(event) => setNextSteps(event.target.value)} placeholder="Only semantic next steps RepoTunnel cannot infer automatically" /></label>
          </div>
          <div className="project-memory-actions"><button className="primary-button" type="button" disabled={busy} onClick={() => void save()}>{busy ? "Saving…" : "Save context"}</button></div>
        </div>
      ) : empty ? (
        <div className="project-memory-empty">No semantic context saved yet. Factual Git, file, process and verification progress is still tracked automatically.</div>
      ) : (
        <div className="project-memory-summary">
          {memory?.summary ? <p>{memory.summary}</p> : null}
          <div className="project-memory-chips">
            {memory?.goals.slice(0, 3).map((item) => <span key={`goal:${item}`}>Goal · {item}</span>)}
            {memory?.preferences.slice(0, 3).map((item) => <span key={`pref:${item}`}>Constraint · {item}</span>)}
            {memory?.decisions.slice(0, 2).map((item) => <span key={`decision:${item}`}>Decision · {item}</span>)}
          </div>
        </div>
      )}
    </section>
  );
}

export default ProjectMemoryPanel;
