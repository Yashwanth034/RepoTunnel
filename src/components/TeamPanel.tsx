import { useCallback, useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import ConfirmationDialog from "./ConfirmationDialog";
import {
  completeTeamSession,
  createTeamSession,
  deleteTeamSession,
  getTeamSession,
  listTeamSessions,
  pauseTeamSession,
  resumeTeamSession,
} from "../lib/backend";
import type {
  TeamAgent,
  TeamLock,
  TeamSessionSummary,
  TeamSnapshot,
  TeamTask,
  Workspace,
} from "../types";

const DEFAULT_AGENT_A_ROLE = "Act as a proactive full product engineer. Plan with Engineer B, then personally implement one meaningful non-overlapping part of every work request, test/debug it, and cross-review Engineer B. Never let Engineer B become the only implementer and never duplicate Engineer B's task.";
const DEFAULT_AGENT_B_ROLE = "Act as a proactive full product engineer. Plan with Engineer A, then personally implement one meaningful non-overlapping part of every work request, test/debug it, and cross-review Engineer A. If Engineer A started first, immediately find and claim remaining independent implementation work instead of waiting.";

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function formatTime(timestamp: number | null): string {
  if (!timestamp) return "Not joined";
  return new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", month: "short", day: "numeric" }).format(new Date(timestamp));
}

function relativeHeartbeat(timestamp: number | null): string {
  if (!timestamp) return "Waiting to join";
  const seconds = Math.max(0, Math.round((Date.now() - timestamp) / 1000));
  if (seconds < 15) return "Active now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  return formatTime(timestamp);
}

function statusLabel(value: string): string {
  return value.replace(/([A-Z])/g, " $1").replace(/^./, (char) => char.toUpperCase());
}

function agentName(snapshot: TeamSnapshot, agentId: string | null): string {
  if (!agentId) return "Unassigned";
  return snapshot.session.agents.find((agent) => agent.id === agentId)?.name ?? "Unknown agent";
}

function currentTasks(snapshot: TeamSnapshot): TeamTask[] {
  return snapshot.session.tasks.filter((task) => task.cycleNumber === snapshot.session.cycleNumber);
}

function agentContributed(snapshot: TeamSnapshot, agentId: string): boolean {
  return currentTasks(snapshot).some((task) =>
    task.status === "done" && task.contributorAgentIds.includes(agentId),
  );
}

function taskStatusClass(task: TeamTask): string {
  return `team-task-status team-task-status-${task.status.toLowerCase()}`;
}

function agentPresenceLabel(snapshot: TeamSnapshot, agent: TeamAgent): string {
  const reviewRequired = currentTasks(snapshot).some((task) =>
    task.status === "review" && task.reviewerAgentId === agent.id,
  );
  if (reviewRequired) return "Review required";
  const submittedOwnScope = currentTasks(snapshot).some((task) =>
    task.ownerAgentId === agent.id && (task.status === "review" || task.status === "done"),
  );
  if (
    (agent.status === "idle" || agent.status === "active")
    && snapshot.session.phase !== "complete"
    && agent.currentTaskId === null
    && submittedOwnScope
  ) {
    return "Waiting for teammate";
  }
  return statusLabel(agent.status);
}

function lockRemaining(lock: TeamLock): string {
  const seconds = Math.max(0, Math.round((lock.expiresAt - Date.now()) / 1000));
  if (seconds < 60) return `${seconds}s`;
  return `${Math.ceil(seconds / 60)}m`;
}

function persistentInstructions(): string {
  return `IMPORTANT PERSISTENT TEAM RULES:
- You join this RepoTunnel Team only once. Keep this same A/B identity until the user explicitly ends the Team in the RepoTunnel desktop app.
- JOIN BARRIER: after joining, do not plan, create tasks, claim files, or code until team_status confirms BOTH Engineer A and Engineer B are joined.
- SHARED PLANNING: once both are joined, each engineer must post one concise Plan message describing the product/request, required implementation, a sensible non-overlapping split, and likely integration/testing risks. Read the other engineer's plan before creating work.
- SPLIT AGREEMENT: after both plans exist, each engineer creates exactly one meaningful initial implementation task for a distinct scope. Check that files/responsibilities do not overlap. Then each engineer posts a Decision confirming the agreed split. RepoTunnel unlocks implementation only after BOTH confirmations.
- PARALLEL WORK: after the split is locked, both engineers claim their own task paths and implement simultaneously. Do not re-plan unless a real blocker appears. Keep one owner per task and never duplicate the other engineer's work.
- BOTH engineers must personally implement meaningful product work in every request. Review/testing alone does not count as implementation contribution.
- CROSS-REVIEW: each engineer reviews the other engineer's implementation. If a bug/error/integration mismatch is found, send concrete review feedback back to the task owner. Discuss the smallest correct fix, let the owner fix it, then re-review. Do not silently patch the other engineer's owned files.
- BROWSER SAFETY: before interactive managed-browser actions (navigate/click/type/reload/start/stop), claim the reserved @browser resource using team_action lock_paths with paths=["@browser"] and no task_id. Release @browser when finished. Never interact with the same managed browser concurrently.
- Use terminal/build/tests in parallel where safe. Prefer one shared dev server owner if a fixed port would collide.
- STAY ONLINE THROUGH COMPLETION: do not voluntarily stop or go offline in the middle of an active request. If your own task is finished first, remain attached, use team_status long-polling while waiting when useful, help with review/verification, and wait for the teammate until the current request has a verified completion/final report.
- Finishing the current work does NOT end the Team. team_action complete closes only the current request, then you remain ready for more work.
- After a request is completed, if the human tells YOU in this same chat to add, improve, redesign, fix, change, or extend the product, immediately post the exact instruction to the existing Team as a Decision beginning exactly: USER REQUEST: <human request>. The SAME Team starts the next cycle; never ask for another kickoff/session.
- Repeat: both join once → plan together → agree split → implement in parallel → cross-review → fix/retest → verify → complete current request, until the user ends the Team.`;
}


function kickoffPrompt(snapshot: TeamSnapshot, agent: TeamAgent): string {
  const criteria = snapshot.session.successCriteria.map((item, index) => `${index + 1}. ${item}`).join("\n");
  return `You are ${agent.name}, one of two persistent AI engineers collaborating through RepoTunnel Team Mode.

Project: ${snapshot.session.workspaceName}
Team session: ${snapshot.session.id}
Your agent ID: ${agent.id}
Your role: ${agent.role}

Initial product goal:
${snapshot.session.goal}

Current success criteria:
${criteria}

Start by calling team_status with session_id=${snapshot.session.id} and agent_id=${agent.id}, then call team_action action=join with the same session_id and agent_id plus a short client_label.

${persistentInstructions()}

Always re-check team_status before each coordination transition. First join, then wait until BOTH engineers are connected. Do not race ahead. Once both are joined, follow RepoTunnel's planning barrier exactly: both post plans, each creates one distinct initial implementation task, both confirm the split, then claim paths and implement in parallel. Cross-review each other; when review finds bugs, communicate the issue and let the owner fix it. Claim @browser before interactive browser testing and release it afterward. Do not voluntarily go offline while the current request is active: if you finish first, remain attached, long-poll team_status while waiting when useful, and stay available for the teammate's review/verification until completion or the final report. If global AI access is paused, stop immediately.`;
}

type TeamPanelProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  onSelectWorkspace: (workspaceId: string) => void;
  onNotice: (message: string) => void;
};

function TeamPanel({ workspaces, selectedWorkspaceId, onSelectWorkspace, onNotice }: TeamPanelProps) {
  const workspace = useMemo(
    () => workspaces.find((item) => item.id === selectedWorkspaceId) ?? workspaces[0] ?? null,
    [selectedWorkspaceId, workspaces],
  );
  const [sessions, setSessions] = useState<TeamSessionSummary[]>([]);
  const [selectedSessionId, setSelectedSessionId] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<TeamSnapshot | null>(null);
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const [goal, setGoal] = useState("");
  const [criteria, setCriteria] = useState("");
  const [agentAName, setAgentAName] = useState("Engineer A");
  const [agentARole, setAgentARole] = useState(DEFAULT_AGENT_A_ROLE);
  const [agentBName, setAgentBName] = useState("Engineer B");
  const [agentBRole, setAgentBRole] = useState(DEFAULT_AGENT_B_ROLE);
  const [confirmAction, setConfirmAction] = useState<"end" | "delete" | null>(null);
  const hasLiveSession = sessions.some((session) => session.status === "active" || session.status === "paused");

  const refresh = useCallback(async (preferredSessionId?: string | null) => {
    if (!workspace) {
      setSessions([]);
      setSnapshot(null);
      setSelectedSessionId(null);
      return;
    }
    setLoading(true);
    try {
      const nextSessions = await listTeamSessions(workspace.id);
      setSessions(nextSessions);
      const currentStillExists = preferredSessionId
        ? nextSessions.some((session) => session.id === preferredSessionId)
        : selectedSessionId && nextSessions.some((session) => session.id === selectedSessionId);
      const nextId = currentStillExists
        ? (preferredSessionId ?? selectedSessionId)
        : nextSessions.find((session) => session.status === "active" || session.status === "paused")?.id ?? nextSessions[0]?.id ?? null;
      setSelectedSessionId(nextId);
      setSnapshot(nextId ? await getTeamSession(nextId) : null);
    } catch (error) {
      onNotice(`Team Mode: ${errorMessage(error)}`);
    } finally {
      setLoading(false);
    }
  }, [onNotice, selectedSessionId, workspace]);

  useEffect(() => {
    void refresh(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspace?.id]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void listen<string>("repotunnel://team-updated", (event) => {
      if (disposed) return;
      if (!selectedSessionId || event.payload === selectedSessionId) void refresh(event.payload || selectedSessionId);
      else if (workspace) void listTeamSessions(workspace.id).then(setSessions).catch(() => undefined);
    }).then((remove) => {
      if (disposed) remove();
      else unlisten = remove;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [refresh, selectedSessionId, workspace]);

  useEffect(() => {
    if (!selectedSessionId || !snapshot || !["active", "paused"].includes(snapshot.session.status)) return;
    const timer = window.setInterval(() => {
      void getTeamSession(selectedSessionId).then(setSnapshot).catch(() => undefined);
    }, 4_000);
    return () => window.clearInterval(timer);
  }, [selectedSessionId, snapshot?.session.status]);

  async function handleCreate() {
    if (!workspace) return;
    const successCriteria = criteria.split("\n").map((item) => item.trim()).filter(Boolean);
    setBusy(true);
    try {
      const created = await createTeamSession(workspace.id, goal, successCriteria, agentAName, agentARole, agentBName, agentBRole);
      setSnapshot(created);
      setSelectedSessionId(created.session.id);
      setGoal("");
      setCriteria("");
      await refresh(created.session.id);
      onNotice("Persistent AI Team created. Copy Engineer A kickoff to AI chat A and Engineer B kickoff to AI chat B once; you will not need another kickoff until you end the Team.");
    } catch (error) {
      onNotice(`Could not create Team Mode session: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function copyText(value: string, label: string) {
    try {
      await navigator.clipboard.writeText(value);
      onNotice(`${label} copied.`);
    } catch {
      onNotice(`Could not copy ${label.toLowerCase()}.`);
    }
  }

  async function handleSessionSelect(sessionId: string) {
    setSelectedSessionId(sessionId);
    setLoading(true);
    try {
      setSnapshot(await getTeamSession(sessionId));
    } catch (error) {
      onNotice(`Team Mode: ${errorMessage(error)}`);
    } finally {
      setLoading(false);
    }
  }

  async function mutate(action: () => Promise<TeamSnapshot>, notice: string) {
    setBusy(true);
    try {
      const next = await action();
      setSnapshot(next);
      await refresh(next.session.id);
      onNotice(notice);
    } catch (error) {
      onNotice(`Team Mode: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleEndTeam() {
    if (!snapshot) return;
    await mutate(
      () => completeTeamSession(snapshot.session.id, "Persistent Team ended by the user from the RepoTunnel desktop app."),
      "AI Team ended.",
    );
    setConfirmAction(null);
  }

  async function handleDelete() {
    if (!snapshot) return;
    setBusy(true);
    try {
      await deleteTeamSession(snapshot.session.id);
      setSnapshot(null);
      setSelectedSessionId(null);
      await refresh(null);
      onNotice("Ended Team record deleted.");
      setConfirmAction(null);
    } catch (error) {
      onNotice(`Team Mode: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  if (!workspace) {
    return <section className="panel team-panel-empty"><h2>AI Team</h2><p>Add a project first, then attach two AI engineers to it.</p></section>;
  }

  const tasks = snapshot ? currentTasks(snapshot) : [];
  const joinedCount = snapshot?.session.agents.filter((agent) => agent.joinedAt).length ?? 0;
  const waitingForNextRequest = snapshot?.session.status === "active" && snapshot.session.phase === "complete";

  return (
    <div className="team-page-stack team-simple-ui">
      <section className="panel team-hero-panel team-simple-hero">
        <div className="team-hero-copy">
          <span className="team-eyebrow">AI TEAM</span>
          <h2>Two AI engineers. One project.</h2>
          <p>Create the Team once. Engineer A and B stay attached until you press <strong>End Team</strong>.</p>
        </div>
        <div className="team-project-picker">
          <label htmlFor="team-project">Project</label>
          <select id="team-project" value={workspace.id} onChange={(event) => onSelectWorkspace(event.target.value)}>
            {workspaces.map((item) => <option key={item.id} value={item.id}>{item.name}</option>)}
          </select>
          <span>{workspace.changePolicy === "automatic" ? "AI Auto" : "AI Review"} · {workspace.accessMode === "readWrite" ? "Read / write" : "Read only"}</span>
        </div>
      </section>

      {!snapshot ? (
        <section className="panel team-create-panel team-simple-create">
          <div className="panel-heading-row"><div><h3>Create your AI Team</h3><p>Give the product goal once. You will paste one kickoff into each AI chat once.</p></div><span className="team-mode-badge">A + B</span></div>
          <label className="team-field"><span>Product goal</span><textarea value={goal} onChange={(event) => setGoal(event.target.value)} placeholder="Describe the product you want both AIs to build…" rows={5} /></label>
          <label className="team-field"><span>Success criteria <small>What must be true before this request is complete? One requirement per line.</small></span><textarea value={criteria} onChange={(event) => setCriteria(event.target.value)} placeholder={"Core features work\nBuild/tests pass\nBrowser verification passes"} rows={4} /></label>
          <div className="team-agent-setup-grid">
            <div className="team-agent-setup-card"><span className="team-agent-index">AI 1</span><label className="team-field"><span>Name</span><input value={agentAName} onChange={(event) => setAgentAName(event.target.value)} /></label></div>
            <div className="team-agent-setup-card"><span className="team-agent-index">AI 2</span><label className="team-field"><span>Name</span><input value={agentBName} onChange={(event) => setAgentBName(event.target.value)} /></label></div>
          </div>
          <details className="team-advanced-details team-role-details">
            <summary>Advanced role instructions</summary>
            <div className="team-agent-setup-grid">
              <label className="team-field"><span>{agentAName} role</span><textarea rows={4} value={agentARole} onChange={(event) => setAgentARole(event.target.value)} /></label>
              <label className="team-field"><span>{agentBName} role</span><textarea rows={4} value={agentBRole} onChange={(event) => setAgentBRole(event.target.value)} /></label>
            </div>
          </details>
          <button type="button" className="primary-button team-create-button" disabled={busy || !goal.trim() || !criteria.trim()} onClick={() => void handleCreate()}>{busy ? "Creating…" : "Create AI Team"}</button>
        </section>
      ) : null}

      {sessions.length > 1 ? (
        <section className="panel team-session-switcher team-simple-switcher">
          <div><strong>Saved Teams</strong><span>{workspace.name}</span></div>
          <div className="team-session-switch-controls">
            <select value={selectedSessionId ?? ""} onChange={(event) => void handleSessionSelect(event.target.value)}>
              {sessions.map((session) => <option key={session.id} value={session.id}>{statusLabel(session.status)} · {session.goal.slice(0, 70)}</option>)}
            </select>
            <button type="button" disabled={hasLiveSession} onClick={() => { setSelectedSessionId(null); setSnapshot(null); }}>New Team</button>
          </div>
        </section>
      ) : null}

      {loading && !snapshot ? <section className="panel team-loading">Loading AI Team…</section> : null}

      {snapshot ? (
        <>
          <section className="panel team-session-header team-simple-status-card">
            <div className="team-session-heading">
              <div className="team-session-title-row">
                <span className={`team-session-status team-session-status-${snapshot.session.status}`}>{snapshot.session.status === "active" ? "Team active" : statusLabel(snapshot.session.status)}</span>
                <span className="team-phase-pill">Request #{snapshot.session.cycleNumber} · {waitingForNextRequest ? "Ready" : statusLabel(snapshot.session.phase)}</span>
              </div>
              <h3>{snapshot.session.currentRequest ?? snapshot.session.goal}</h3>
              {waitingForNextRequest ? (
                <div className="team-ready-banner"><strong>Current request finished.</strong><span>Keep using the same AI A/B chats. Ask either AI for your next change — no new session, no new kickoff.</span></div>
              ) : (
                <><div className="team-progress-track"><span style={{ width: `${snapshot.progress.progressPercent}%` }} /></div><p>{snapshot.progress.doneTaskCount} done · {snapshot.progress.openTaskCount} open · {snapshot.progress.blockedTaskCount} blocked · {snapshot.progress.verifiedCriterionCount}/{snapshot.progress.totalCriterionCount} checks verified</p></>
              )}
            </div>
            <div className="team-session-actions">
              {snapshot.session.status === "active" ? <button type="button" disabled={busy} onClick={() => void mutate(() => pauseTeamSession(snapshot.session.id), "AI Team paused.")}>Pause</button> : null}
              {snapshot.session.status === "paused" ? <button type="button" disabled={busy} onClick={() => void mutate(() => resumeTeamSession(snapshot.session.id), "AI Team resumed.")}>Resume</button> : null}
              {["active", "paused"].includes(snapshot.session.status) ? <button type="button" className="danger-soft team-end-button" disabled={busy} onClick={() => setConfirmAction("end")}>End Team</button> : null}
              {["completed", "cancelled"].includes(snapshot.session.status) ? <button type="button" className="danger-soft" disabled={busy} onClick={() => setConfirmAction("delete")}>Delete record</button> : null}
            </div>
          </section>

          <section className="panel team-workflow-panel">
            <div className="team-workflow-title"><div><h3>Team workflow</h3><p>RepoTunnel keeps both engineers synchronized and prevents racing ahead.</p></div><span>{waitingForNextRequest ? "Ready" : statusLabel(snapshot.session.phase)}</span></div>
            <div className="team-workflow-steps">
              <div className={joinedCount === 2 ? "done" : "active"}><strong>1</strong><span>Both join</span></div>
              <div className={snapshot.session.phase !== "planning" || waitingForNextRequest ? "done" : joinedCount === 2 ? "active" : ""}><strong>2</strong><span>Plan together</span></div>
              <div className={["executing", "reviewing", "verifying", "complete"].includes(snapshot.session.phase) ? "done" : ""}><strong>3</strong><span>Split locked</span></div>
              <div className={["reviewing", "verifying", "complete"].includes(snapshot.session.phase) ? "done" : snapshot.session.phase === "executing" ? "active" : ""}><strong>4</strong><span>Parallel build</span></div>
              <div className={["verifying", "complete"].includes(snapshot.session.phase) ? "done" : snapshot.session.phase === "reviewing" ? "active" : ""}><strong>5</strong><span>Cross-review</span></div>
              <div className={snapshot.session.phase === "complete" ? "done" : snapshot.session.phase === "verifying" ? "active" : ""}><strong>6</strong><span>Verify</span></div>
            </div>
          </section>

          <section className="panel team-agents-panel team-simple-agents">
            <div className="panel-heading-row">
              <div><h3>Your AI engineers</h3><p>They keep these identities for this project until you end the Team.</p></div>
              <div className="team-agent-heading-actions"><span>{joinedCount}/2 connected</span>{joinedCount === 2 ? <span className="team-connected-note">Connected — kickoff is no longer needed</span> : <span className="team-connected-note">Use the separate A/B kickoff buttons below</span>}</div>
            </div>
            <div className="team-agent-grid">
              {snapshot.session.agents.map((agent, index) => {
                const currentTask = snapshot.session.tasks.find((task) => task.id === agent.currentTaskId)?.title;
                return (
                  <article className="team-agent-card team-simple-agent-card" key={agent.id}>
                    <div className="team-agent-card-head"><span className="team-agent-avatar">AI {index + 1}</span><span className={`team-agent-presence team-agent-presence-${agent.status}`}>{agentPresenceLabel(snapshot, agent)}</span></div>
                    <strong>{agent.name}</strong>
                    <div className="team-agent-primary-status">{currentTask ?? (waitingForNextRequest ? "Ready for your next request" : agent.joinedAt ? "Coordinating with team" : "Waiting to join")}</div>
                    <div className="team-agent-quick-meta"><span>{agent.clientLabel ?? "Not connected"}</span><span>{relativeHeartbeat(agent.lastSeenAt)}</span><span>{agentContributed(snapshot, agent.id) ? "Implemented work" : "Implementation pending"}</span></div>
                    {!agent.joinedAt ? <button type="button" onClick={() => void copyText(kickoffPrompt(snapshot, agent), `${agent.name} kickoff`)}>Copy {agent.name} kickoff</button> : null}
                  </article>
                );
              })}
            </div>
          </section>

          <div className="team-simple-work-grid">
            <section className="panel team-tasks-panel">
              <div className="panel-heading-row"><div><h3>Current work</h3><p>Each coding task has one owner; the other AI takes different work or reviews it.</p></div><span>{tasks.length} tasks</span></div>
              {tasks.length === 0 ? <div className="team-empty-board">{waitingForNextRequest ? "No active work. Ask either AI chat for your next feature, fix, or improvement." : "The AIs are planning this request."}</div> : (
                <div className="team-task-grid team-simple-task-grid">
                  {tasks.slice().sort((left, right) => right.priority - left.priority || left.createdAt - right.createdAt).map((task) => (
                    <article className="team-task-card" key={task.id}>
                      <div className="team-task-head"><span className={taskStatusClass(task)}>{statusLabel(task.status)}</span><span>P{task.priority}</span></div>
                      <strong>{task.title}</strong>
                      <div className="team-task-meta"><span>Owner: {agentName(snapshot, task.ownerAgentId)}</span>{task.reviewerAgentId ? <span>Reviewer: {agentName(snapshot, task.reviewerAgentId)}</span> : null}</div>
                      {task.blockedReason ? <div className="team-task-blocked">Blocked: {task.blockedReason}</div> : null}
                      {task.result ? <div className="team-task-result">{task.result}</div> : null}
                    </article>
                  ))}
                </div>
              )}
            </section>

            <section className="panel team-criteria-panel team-simple-checks">
              <div className="panel-heading-row"><div><h3>Verification</h3><p>Proof that this request is actually done.</p></div><span>{snapshot.progress.verifiedCriterionCount}/{snapshot.progress.totalCriterionCount}</span></div>
              <div className="team-criterion-list">
                {snapshot.session.criterionChecks.map((criterion, index) => (
                  <article className={`team-criterion-row ${criterion.verified ? "verified" : "pending"}`} key={criterion.id}>
                    <span className="team-criterion-mark">{criterion.verified ? "✓" : index + 1}</span>
                    <div><strong>{criterion.text}</strong>{criterion.verified && criterion.evidence ? <details><summary>View evidence</summary><p>{criterion.evidence}{criterion.verifiedByAgentId ? ` · ${agentName(snapshot, criterion.verifiedByAgentId)}` : ""}</p></details> : null}</div>
                  </article>
                ))}
              </div>
            </section>
          </div>

          <details className="panel team-advanced-details team-runtime-details">
            <summary>Advanced Team details</summary>
            <div className="team-advanced-grid">
              <div><strong>Session ID</strong><code>{snapshot.session.id}</code></div>
              <div><strong>Revision</strong><span>{snapshot.session.revision}</span></div>
              <div><strong>Completed requests</strong><span>{snapshot.session.completedCycles.length}</span></div>
              <div><strong>Active file claims</strong><span>{snapshot.session.locks.length}</span></div>
            </div>
            {snapshot.session.completedCycles.length ? <div className="team-cycle-history"><h4>Previous requests</h4>{snapshot.session.completedCycles.slice().reverse().slice(0, 12).map((cycle) => <div key={`${cycle.number}-${cycle.completedAt}`}><strong>#{cycle.number} {cycle.request}</strong><span>{cycle.summary}</span></div>)}</div> : null}
            {snapshot.session.locks.length ? <div className="team-lock-list">{snapshot.session.locks.map((lock) => <div className="team-lock-row" key={lock.id}><code>{lock.path}</code><span>{agentName(snapshot, lock.agentId)}</span><small>{lockRemaining(lock)} left</small></div>)}</div> : null}
            {joinedCount < 2 ? <div className="team-advanced-kickoffs">{snapshot.session.agents.map((agent) => <button key={agent.id} type="button" onClick={() => void copyText(kickoffPrompt(snapshot, agent), `${agent.name} kickoff`)}>Copy {agent.name} kickoff</button>)}</div> : null}
            {snapshot.recommendedAction ? <div className="team-next-action"><span>RepoTunnel guidance</span><p>{snapshot.recommendedAction}</p></div> : null}
          </details>
        </>
      ) : null}

      {confirmAction === "end" ? (
        <ConfirmationDialog
          title="End this persistent AI Team?"
          message="Engineer A and Engineer B will be detached from this project. Only end the Team when you are finished using these two persistent AI sessions."
          confirmLabel="End Team"
          busy={busy}
          busyLabel="Ending…"
          onCancel={() => setConfirmAction(null)}
          onConfirm={() => void handleEndTeam()}
        />
      ) : null}

      {confirmAction === "delete" ? (
        <ConfirmationDialog
          title="Delete ended Team record?"
          message="This removes only the ended coordination record. Project files and RepoTunnel History are not deleted."
          confirmLabel="Delete record"
          busy={busy}
          busyLabel="Deleting…"
          onCancel={() => setConfirmAction(null)}
          onConfirm={() => void handleDelete()}
        />
      ) : null}
    </div>
  );
}

export default TeamPanel;
