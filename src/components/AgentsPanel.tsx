import { useEffect, useMemo, useRef, useState } from "react";
import {
  acceptAgentMissionResult,
  approveAgentMissionGate,
  archiveAgentMission,
  createAgentMission,
  deleteAgentMission,
  getAgentMission,
  listAgentMissions,
  pauseAgentMission,
  rejectAgentMissionGate,
  removeMissionAgent,
  renameMissionAgent,
  restoreArchivedAgentMission,
  restoreMissionAgent,
  resumeAgentMission,
  stopAgentMission,
  submitAgentMissionDirective,
  submitAgentMissionQuestion,
} from "../lib/backend";
import type {
  AgentMissionSnapshot,
  AgentMissionSummary,
  MissionHumanGate,
  MissionStatus,
  Workspace,
} from "../types";

type AgentsPanelProps = {
  workspaces: Workspace[];
  selectedWorkspaceId: string | null;
  onSelectWorkspace: (workspaceId: string) => void;
  onNotice: (message: string) => void;
};

type AgentsTab = "overview" | "tasks" | "agents" | "decisions" | "evidence" | "activity" | "gates";

const tabs: Array<{ id: AgentsTab; label: string }> = [
  { id: "overview", label: "Overview" },
  { id: "tasks", label: "Tasks" },
  { id: "agents", label: "Agents" },
  { id: "decisions", label: "Decisions" },
  { id: "evidence", label: "Evidence" },
  { id: "activity", label: "Activity" },
  { id: "gates", label: "Deferred Actions" },
];

const statusLabels: Record<MissionStatus, string> = {
  draft: "Draft",
  running: "Running",
  pausedByHuman: "Paused by human",
  replanning: "Replanning",
  humanGateRequired: "Deferred action pending",
  readyForHumanReview: "Ready for human review",
  completed: "Completed",
  stoppedByHuman: "Stopped by human",
  failedUnrecoverable: "Failed unrecoverable",
};

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}

function formatStatus(status: string) {
  return status
    .replace(/([a-z])([A-Z])/g, "$1 $2")
    .replace(/^./, (value) => value.toUpperCase());
}

function lastSeenLabel(lastSeenAt: number) {
  if (!lastSeenAt) return "No heartbeat recorded";
  const seconds = Math.max(0, Math.floor((Date.now() - lastSeenAt) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

function AgentsPanel({ workspaces, selectedWorkspaceId, onSelectWorkspace, onNotice }: AgentsPanelProps) {
  const workspaceId = selectedWorkspaceId ?? workspaces[0]?.id ?? null;
  const [missions, setMissions] = useState<AgentMissionSummary[]>([]);
  const [snapshot, setSnapshot] = useState<AgentMissionSnapshot | null>(null);
  const [selectedMissionId, setSelectedMissionId] = useState<string | null>(null);
  const selectedMissionIdRef = useRef<string | null>(null);
  const snapshotRequestGenerationRef = useRef(0);
  const missionGoalRef = useRef<HTMLTextAreaElement>(null);
  const directiveRef = useRef<HTMLTextAreaElement>(null);
  const questionRef = useRef<HTMLInputElement>(null);
  const [activeTab, setActiveTab] = useState<AgentsTab>("overview");
  const [goalExpanded, setGoalExpanded] = useState(false);
  const [creatingMission, setCreatingMission] = useState(false);
  const [showArchived, setShowArchived] = useState(false);
  const [agentSearch, setAgentSearch] = useState("");
  const [taskFilter, setTaskFilter] = useState<"all" | "active" | "pending" | "review" | "verified">("all");
  const [renamingAgentId, setRenamingAgentId] = useState<string | null>(null);
  const [agentName, setAgentName] = useState("");
  const [removingAgentId, setRemovingAgentId] = useState<string | null>(null);
  const [deletingMission, setDeletingMission] = useState(false);
  const [deleteConfirmation, setDeleteConfirmation] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);

  function chooseMission(missionId: string | null) {
    selectedMissionIdRef.current = missionId;
    setSelectedMissionId(missionId);
  }

  function nextSnapshotRequestGeneration() {
    snapshotRequestGenerationRef.current += 1;
    return snapshotRequestGenerationRef.current;
  }

  function applySnapshotIfCurrent(
    missionId: string,
    generation: number,
    nextSnapshot: AgentMissionSnapshot,
  ) {
    if (
      snapshotRequestGenerationRef.current === generation
      && selectedMissionIdRef.current === missionId
    ) {
      setSnapshot(nextSnapshot);
    }
  }

  useEffect(() => {
    let cancelled = false;
    if (!workspaceId) {
      nextSnapshotRequestGeneration();
      setMissions([]);
      setSnapshot(null);
      chooseMission(null);
      return () => {
        cancelled = true;
        nextSnapshotRequestGeneration();
      };
    }

    setLoading(true);
    setCreatingMission(false);
    setDeletingMission(false);
    setDeleteConfirmation("");
    void listAgentMissions(workspaceId)
      .then(async (items) => {
        if (cancelled) return;
        setMissions(items);
        const currentMissionId = selectedMissionIdRef.current;
        const nextMissionId = items.some((item) => item.id === currentMissionId)
          ? currentMissionId
          : items[0]?.id ?? null;
        chooseMission(nextMissionId);
        if (!nextMissionId) {
          nextSnapshotRequestGeneration();
          setSnapshot(null);
          return;
        }
        const generation = nextSnapshotRequestGeneration();
        const nextSnapshot = await getAgentMission(nextMissionId);
        if (!cancelled) applySnapshotIfCurrent(nextMissionId, generation, nextSnapshot);
      })
      .catch((error) => {
        if (!cancelled) onNotice(`Could not load Agents missions: ${errorMessage(error)}`);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });

    return () => {
      cancelled = true;
      nextSnapshotRequestGeneration();
    };
  }, [workspaceId]);

  useEffect(() => {
    setGoalExpanded(false);
  }, [selectedMissionId]);

  useEffect(() => {
    if (!selectedMissionId || busy) return;
    let cancelled = false;
    let inFlight = false;
    const interval = window.setInterval(() => {
      if (cancelled || inFlight || selectedMissionIdRef.current !== selectedMissionId) return;
      inFlight = true;
      const generation = nextSnapshotRequestGeneration();
      void getAgentMission(selectedMissionId)
        .then((nextSnapshot) => {
          if (!cancelled) applySnapshotIfCurrent(selectedMissionId, generation, nextSnapshot);
        })
        .catch(() => {
          // Poll failures are transient and must not spam the global notice surface.
        })
        .finally(() => {
          inFlight = false;
        });
    }, 500);
    return () => {
      cancelled = true;
      window.clearInterval(interval);
    };
  }, [selectedMissionId, busy]);

  async function selectMission(missionId: string) {
    setCreatingMission(false);
    setDeletingMission(false);
    setDeleteConfirmation("");
    chooseMission(missionId || null);
    const generation = nextSnapshotRequestGeneration();
    if (!missionId) {
      setSnapshot(null);
      return;
    }
    setLoading(true);
    try {
      const nextSnapshot = await getAgentMission(missionId);
      applySnapshotIfCurrent(missionId, generation, nextSnapshot);
    } catch (error) {
      if (selectedMissionIdRef.current === missionId) {
        onNotice(`Could not load Mission: ${errorMessage(error)}`);
      }
    } finally {
      if (selectedMissionIdRef.current === missionId) setLoading(false);
    }
  }

  async function handleCreateMission() {
    const goal = missionGoalRef.current?.value.trim() ?? "";
    if (!workspaceId || !goal || busy) return;
    nextSnapshotRequestGeneration();
    setBusy(true);
    try {
      const created = await createAgentMission(workspaceId, goal);
      setSnapshot(created);
      chooseMission(created.mission.id);
      setCreatingMission(false);
      setShowArchived(false);
      if (missionGoalRef.current) missionGoalRef.current.value = "";
      setMissions((current) => {
        const summary: AgentMissionSummary = {
          id: created.mission.id,
          workspaceId: created.mission.workspaceId,
          goal: created.mission.goal,
          status: created.mission.status,
          archived: created.mission.archived ?? false,
          autonomyPolicy: created.mission.autonomyPolicy,
          missionRevision: created.mission.missionRevision,
          authorityRevision: created.mission.authorityRevision,
          createdAt: created.mission.createdAt,
          updatedAt: created.mission.updatedAt,
          progress: created.progress,
        };
        return [summary, ...current.filter((item) => item.id !== summary.id)];
      });
      onNotice("Mission started. Agents can join independently through Mission Control.");
    } catch (error) {
      onNotice(`Could not start Mission: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleDirective() {
    const text = directiveRef.current?.value.trim() ?? "";
    if (!snapshot || !text || busy) return;
    const missionId = snapshot.mission.id;
    const generation = nextSnapshotRequestGeneration();
    setBusy(true);
    try {
      const next = await submitAgentMissionDirective(missionId, text);
      applySnapshotIfCurrent(missionId, generation, next);
      if (directiveRef.current && selectedMissionIdRef.current === missionId) directiveRef.current.value = "";
      onNotice("Directive recorded. Mission workers continue under the new authority revision.");
    } catch (error) {
      onNotice(`Could not submit directive: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function handleQuestion() {
    const text = questionRef.current?.value.trim() ?? "";
    if (!snapshot || !text || busy) return;
    const missionId = snapshot.mission.id;
    const generation = nextSnapshotRequestGeneration();
    setBusy(true);
    try {
      await submitAgentMissionQuestion(missionId, text);
      if (questionRef.current && selectedMissionIdRef.current === missionId) questionRef.current.value = "";
      const next = await getAgentMission(missionId);
      applySnapshotIfCurrent(missionId, generation, next);
      onNotice("Ask Swarm question queued without pausing Mission workers.");
    } catch (error) {
      onNotice(`Could not ask swarm: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function updateMission(action: "pause" | "resume" | "stop" | "accept") {
    if (!snapshot || busy) return;
    const missionId = snapshot.mission.id;
    const generation = nextSnapshotRequestGeneration();
    setBusy(true);
    try {
      const next =
        action === "pause"
          ? await pauseAgentMission(missionId)
          : action === "resume"
            ? await resumeAgentMission(missionId)
            : action === "stop"
              ? await stopAgentMission(missionId)
              : await acceptAgentMissionResult(missionId);
      applySnapshotIfCurrent(missionId, generation, next);
    } catch (error) {
      onNotice(`Mission action failed: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function manageAgent(action: "rename" | "remove" | "restore", agentId: string) {
    if (!snapshot || busy) return;
    const missionId = snapshot.mission.id;
    const generation = nextSnapshotRequestGeneration();
    setBusy(true);
    try {
      const next = action === "rename"
        ? await renameMissionAgent(missionId, agentId, agentName.trim())
        : action === "remove"
          ? await removeMissionAgent(missionId, agentId)
          : await restoreMissionAgent(missionId, agentId);
      applySnapshotIfCurrent(missionId, generation, next);
      setRenamingAgentId(null);
      setRemovingAgentId(null);
      onNotice(action === "remove" ? "Agent removed and its work returned to Mission scheduling." : action === "restore" ? "Agent may now rejoin from its original client." : "Agent name updated.");
    } catch (error) {
      onNotice(`Agent action failed: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function manageMissionRecords(action: "archive" | "restore" | "delete") {
    if (!snapshot || busy) return;
    const missionId = snapshot.mission.id;
    const generation = nextSnapshotRequestGeneration();
    setBusy(true);
    try {
      if (action === "delete") {
        if (deleteConfirmation !== "DELETE") return;
        await deleteAgentMission(missionId);
        const remaining = await listAgentMissions(workspaceId ?? undefined);
        setMissions(remaining);
        const selected = remaining.find((item) => !item.archived) ?? remaining[0];
        chooseMission(selected?.id ?? null);
        const followupGeneration = nextSnapshotRequestGeneration();
        if (selected) {
          const next = await getAgentMission(selected.id);
          applySnapshotIfCurrent(selected.id, followupGeneration, next);
        } else {
          setSnapshot(null);
          setCreatingMission(true);
        }
        setDeletingMission(false);
        setDeleteConfirmation("");
        onNotice("Mission records deleted. Project source files were not touched.");
      } else {
        const next = action === "archive"
          ? await archiveAgentMission(missionId)
          : await restoreArchivedAgentMission(missionId);
        applySnapshotIfCurrent(missionId, generation, next);
        setMissions((current) => current.map((mission) => mission.id === missionId
          ? { ...mission, archived: Boolean(next.mission.archived), updatedAt: next.mission.updatedAt }
          : mission));
        onNotice(action === "archive" ? "Mission archived. Restore it to continue." : "Mission restored to the mission list.");
      }
    } catch (error) {
      onNotice(`Mission management failed: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function updateGate(gate: MissionHumanGate, action: "approve" | "reject") {
    if (!snapshot || busy) return;
    const missionId = snapshot.mission.id;
    const generation = nextSnapshotRequestGeneration();
    setBusy(true);
    try {
      if (action === "approve") await approveAgentMissionGate(gate.id);
      else await rejectAgentMissionGate(gate.id);
      const next = await getAgentMission(missionId);
      applySnapshotIfCurrent(missionId, generation, next);
    } catch (error) {
      onNotice(`Human gate action failed: ${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  const latestAnswer = useMemo(
    () => snapshot?.humanQuestions.slice().reverse().find((item) => item.synthesizedAnswer)?.synthesizedAnswer ?? null,
    [snapshot],
  );

  const evidence = useMemo(
    () => snapshot?.tasks.flatMap((task) => task.evidence.map((item) => ({ task, item }))) ?? [],
    [snapshot],
  );

  const pendingGates = snapshot?.humanGates.filter((gate) => gate.status === "pending") ?? [];
  const visibleMissions = missions.filter((mission) => !mission.archived || showArchived || mission.id === selectedMissionId);
  const terminalMission = snapshot != null && ["stoppedByHuman", "completed", "failedUnrecoverable"].includes(snapshot.mission.status);
  const liveAgents = snapshot?.agents.filter((agent) => !agent.binding.revoked && agent.binding.leaseExpiresAt > Date.now()).length ?? 0;
  const missionCanBeArchivedOrDeleted = terminalMission && liveAgents === 0 && !snapshot?.agents.some((agent) => Boolean(agent.binding.assignment));
  const filteredTasks = snapshot?.tasks.filter((task) => {
    if (taskFilter === "active") return task.status === "inProgress";
    if (taskFilter === "review") return task.status === "review";
    if (taskFilter === "pending") return ["proposed", "ready", "stale", "blockedInternal", "blockedExternal"].includes(task.status);
    if (taskFilter === "verified") return ["verified", "integrated"].includes(task.status);
    return true;
  }) ?? [];
  const filteredAgents = snapshot?.agents.filter((agent) => {
    const text = `${agent.displayName ?? ""} ${agent.clientLabel ?? ""} ${agent.id} ${agent.status}`.toLowerCase();
    return text.includes(agentSearch.trim().toLowerCase());
  }) ?? [];
  const goalPreviewLimit = 280;
  const goalNeedsExpansion = Boolean(snapshot && snapshot.mission.goal.length > goalPreviewLimit);
  const displayedGoal = !snapshot
    ? ""
    : goalExpanded || !goalNeedsExpansion
      ? snapshot.mission.goal
      : `${snapshot.mission.goal.slice(0, goalPreviewLimit).trimEnd()}…`;

  async function copyMissionId() {
    if (!snapshot) return;
    try {
      if (!navigator.clipboard?.writeText) throw new Error("Clipboard API unavailable");
      await navigator.clipboard.writeText(snapshot.mission.id);
      onNotice("Mission ID copied.");
    } catch {
      onNotice(`Mission ID: ${snapshot.mission.id}`);
    }
  }

  return (
    <section className="agents-panel" aria-label="Agents / Mission Control">
      <div className="agents-header">
        <div>
          <span className="agents-eyebrow">Agents / Mission Control</span>
          <h2>Autonomous project mission</h2>
          <p>One project and one goal, with durable multi-agent planning, execution, review, evidence, and recovery.</p>
        </div>
        <div className="agents-header-actions">
          {snapshot && !creatingMission ? <span className={`agents-status agents-status-${snapshot.mission.status}`}>{snapshot.mission.archived ? "Archived · " : ""}{statusLabels[snapshot.mission.status]}</span> : null}
          <button type="button" className="agents-new-mission-button" disabled={!workspaceId || busy} onClick={() => { setCreatingMission((current) => !current); setDeletingMission(false); }}>
            {creatingMission ? "Cancel" : "+ New Mission"}
          </button>
        </div>
      </div>

      <div className="agents-project-row">
        <label>
          Project
          <select
            value={workspaceId ?? ""}
            onChange={(event) => onSelectWorkspace(event.target.value)}
            disabled={workspaces.length === 0}
          >
            {workspaces.length === 0 ? <option value="">No approved project</option> : null}
            {workspaces.map((workspace) => (
              <option key={workspace.id} value={workspace.id}>{workspace.name}</option>
            ))}
          </select>
        </label>
        {missions.length > 0 ? (
          <label>
            Mission
            <select value={selectedMissionId ?? ""} onChange={(event) => void selectMission(event.target.value)}>
              {visibleMissions.map((mission) => (
                <option key={mission.id} value={mission.id}>{mission.archived ? "[Archived] " : ""}{mission.goal.slice(0, 105)}</option>
              ))}
            </select>
          </label>
        ) : null}
        {missions.some((mission) => mission.archived) ? (
          <label className="agents-archive-toggle">
            <input type="checkbox" checked={showArchived} onChange={(event) => setShowArchived(event.target.checked)} />
            Show archived
          </label>
        ) : null}
      </div>

      {!snapshot || creatingMission ? (
        <div className="agents-card agents-create-card">
          <div>
            <h3>Start Mission</h3>
            <p>Mission Control derives work dynamically. There are no permanent Agent A/B role fields.</p>
          </div>
          <textarea
            ref={missionGoalRef}
            name="missionGoal"
            placeholder="Describe the project goal and factual acceptance outcome..."
            rows={5}
          />
          <div className="agents-create-actions">
            <button type="button" className="primary-button" disabled={!workspaceId || busy} onClick={() => void handleCreateMission()}>
              Start Mission
            </button>
            {snapshot ? <button type="button" disabled={busy} onClick={() => setCreatingMission(false)}>Back to Mission</button> : null}
          </div>
          {loading ? <span className="agents-muted">Loading missions…</span> : null}
        </div>
      ) : (
        <>
          {snapshot.mission.archived ? (
            <div className="agents-archive-banner">
              <strong>Mission archived</strong>
              <span>Worker mutations are blocked. Restore the mission to use it again.</span>
              <button type="button" disabled={busy} onClick={() => void manageMissionRecords("restore")}>Restore Mission</button>
            </div>
          ) : null}
          <div className="agents-summary-grid">
            <article className="agents-stat"><span>Revision</span><strong>{snapshot.mission.authorityRevision}</strong></article>
            <article className="agents-stat"><span>Agents</span><strong>{snapshot.progress.activeAgentCount} / {snapshot.progress.registeredAgentCount}</strong><small>active / registered</small></article>
            <article className="agents-stat"><span>Tasks</span><strong>{snapshot.progress.verifiedTaskCount} / {snapshot.progress.knownTaskCount}</strong><small>verified / known</small></article>
            <article className="agents-stat"><span>Criteria</span><strong>{snapshot.progress.verifiedCriterionCount} / {snapshot.progress.totalCriterionCount}</strong><small>criteria proven</small></article>
            <article className="agents-stat"><span>Blockers</span><strong>{snapshot.progress.unresolvedBlockerCount}</strong></article>
            <article className="agents-stat"><span>Deferred actions</span><strong>{snapshot.progress.pendingHumanGateCount}</strong></article>
          </div>

          {!snapshot.mission.archived && snapshot.progress.activeAgentCount === 0 ? (
            <div className="agents-health-banner" role="status">
              <div>
                <strong>{snapshot.mission.status === "pausedByHuman" ? "Mission paused · no active agents" : "No agents currently connected"}</strong>
                <span>Registered agents are not necessarily live workers. Open the same Mission ID in an authorized AI conversation and have its worker rejoin; RepoTunnel will then resume normal scheduling when the Mission is running.</span>
              </div>
              <button type="button" onClick={() => void copyMissionId()}>Copy Mission ID</button>
            </div>
          ) : null}

          <div className="agents-control-grid">
            <div className="agents-card">
              <h3>Directive to agents</h3>
              <p>Authoritative instruction. It advances Mission authority but does not imply pause.</p>
              <textarea
                ref={directiveRef}
                name="missionDirective"
                placeholder="Add a constraint, correction, priority change, or scope instruction..."
                rows={3}
              />
              <button type="button" disabled={busy || !!snapshot.mission.archived} onClick={() => void handleDirective()}>Send Directive</button>
            </div>

            <div className="agents-card">
              <h3>Ask Swarm</h3>
              <p>Read-only question. Worker leases and Mission scheduling continue.</p>
              <div className="agents-question-row">
                <input
                  ref={questionRef}
                  name="missionQuestion"
                  placeholder="What is blocking us? Which evidence is still missing?"
                />
                <button type="button" disabled={busy || !!snapshot.mission.archived} onClick={() => void handleQuestion()}>Ask Swarm</button>
              </div>
              {latestAnswer ? <div className="agents-answer"><strong>Answer</strong><span>{latestAnswer}</span></div> : null}
            </div>
          </div>

          <div className="agents-tabs" role="tablist" aria-label="Mission detail views">
            {tabs.map((tab) => (
              <button
                key={tab.id}
                type="button"
                role="tab"
                aria-selected={activeTab === tab.id}
                className={activeTab === tab.id ? "active" : ""}
                onClick={() => setActiveTab(tab.id)}
              >
                {tab.label}
              </button>
            ))}
          </div>

          <div className="agents-card agents-detail-card">
            {activeTab === "overview" ? (
              <div className="agents-overview">
                <div className="agents-overview-goal">
                  <span>Goal</span>
                  <strong>{displayedGoal}</strong>
                  {goalNeedsExpansion ? (
                    <button type="button" className="agents-inline-action" onClick={() => setGoalExpanded((current) => !current)}>
                      {goalExpanded ? "Collapse goal" : "View full goal"}
                    </button>
                  ) : null}
                </div>
                <div className="agents-overview-id">
                  <span>Mission ID</span>
                  <code>{snapshot.mission.id}</code>
                  <button type="button" className="agents-inline-action" onClick={() => void copyMissionId()}>Copy ID</button>
                </div>
                <div><span>Status</span><strong>{statusLabels[snapshot.mission.status]}</strong></div>
                <div><span>Autonomy</span><strong>Full local</strong></div>
                <div><span>Active work</span><strong>{snapshot.progress.activeTaskCount}</strong></div>
                <div><span>Criteria</span><strong>{snapshot.progress.verifiedCriterionCount} / {snapshot.progress.totalCriterionCount} criteria proven</strong></div>
                <div><span>Latest directive</span><strong>{snapshot.humanDirectives.at(-1)?.text ?? "None"}</strong></div>
              </div>
            ) : null}

            {activeTab === "tasks" ? (
              <div className="agents-management">
                <div className="agents-list-toolbar">
                  <div><strong>Autonomous task queue</strong><small>{snapshot.tasks.length} tasks · managed by the Mission scheduler</small></div>
                  <select aria-label="Filter tasks" value={taskFilter} onChange={(event) => setTaskFilter(event.target.value as typeof taskFilter)}>
                    <option value="all">All tasks</option>
                    <option value="active">In progress</option>
                    <option value="pending">Waiting / blocked</option>
                    <option value="review">In review</option>
                    <option value="verified">Verified / integrated</option>
                  </select>
                </div>
                <div className="agents-list agents-task-list">
                  {snapshot.tasks.length === 0 ? <p className="agents-muted">The scheduler has not proposed any tasks yet. Connected workers will plan work automatically.</p> : null}
                  {snapshot.tasks.length > 0 && filteredTasks.length === 0 ? <p className="agents-muted">No tasks match this status.</p> : null}
                  {filteredTasks.map((task) => (
                    <article key={task.id} className="agents-task-card">
                      <div><strong>{task.title}</strong><span>{formatStatus(task.status)}</span></div>
                      <small>{task.id} · Priority {task.priority} · {task.evidence.length} evidence receipt{task.evidence.length === 1 ? "" : "s"}</small>
                      <small>{task.ownerAgentId ? `Owner ${task.ownerAgentId}` : "Unassigned"}{task.reviewerAgentId ? ` · Reviewer ${task.reviewerAgentId}` : ""}</small>
                      {task.dependsOn?.length ? <small>Depends on: {task.dependsOn.join(", ")}</small> : null}
                      {task.requestedPaths?.length ? <small>Scope: {task.requestedPaths.slice(0, 3).join(", ")}{task.requestedPaths.length > 3 ? "…" : ""}</small> : null}
                      {task.needsDiversification ? <small className="agents-task-warning">Needs a different approach after repeated failures</small> : null}
                    </article>
                  ))}
                </div>
              </div>
            ) : null}

            {activeTab === "agents" ? (
              <div className="agents-management">
                <div className="agents-list-toolbar">
                  <div><strong>Registered workers</strong><small>{snapshot.agents.filter((agent) => !agent.binding.revoked).length} enabled · {snapshot.agents.filter((agent) => agent.binding.revoked).length} removed</small></div>
                  <input aria-label="Search agents" placeholder="Search agents or status…" value={agentSearch} onChange={(event) => setAgentSearch(event.target.value)} />
                </div>
                <div className="agents-list agents-worker-list">
                  {snapshot.agents.length === 0 ? <p className="agents-muted">No agents joined yet. Invite a worker using this Mission ID from its ChatGPT conversation.</p> : null}
                  {snapshot.agents.length > 0 && filteredAgents.length === 0 ? <p className="agents-muted">No agents match this search.</p> : null}
                  {filteredAgents.map((agent) => {
                    const removed = Boolean(agent.binding.revoked);
                    const online = !removed && agent.status !== "retired" && agent.binding.leaseExpiresAt > Date.now();
                    const status = removed ? "Removed" : agent.status === "retired" ? "Retired" : online ? formatStatus(agent.status) : "Offline";
                    const agentTitle = agent.displayName || agent.clientLabel || `Agent ${agent.id.slice(-8)}`;
                    return (
                      <article className="agents-worker" key={agent.id}>
                        <div className="agents-worker-header">
                          <div className="agents-worker-identity">
                            <span className={`agents-presence-dot ${online ? "is-online" : ""}`} aria-hidden="true" />
                            <div><strong>{agentTitle}</strong><small>{agent.id}</small></div>
                          </div>
                          <span className={`agents-worker-status ${online ? "is-online" : ""}`}>{status}</span>
                        </div>
                        <div className="agents-worker-details">
                          <span>Last heartbeat: {lastSeenLabel(agent.binding.lastSeenAt)}</span>
                          <span>{agent.binding.assignment ? `Assignment: ${formatStatus(agent.binding.assignment.kind)}${agent.binding.assignment.taskId ? ` · ${agent.binding.assignment.taskId}` : ""}` : removed ? "Access revoked" : online ? "Awaiting scheduler assignment" : "Reconnect the original worker to resume"}</span>
                        </div>
                        {renamingAgentId === agent.id ? (
                          <div className="agents-inline-form">
                            <input aria-label="Agent display name" maxLength={80} value={agentName} onChange={(event) => setAgentName(event.target.value)} />
                            <button type="button" disabled={busy || !!snapshot.mission.archived || !agentName.trim()} onClick={() => void manageAgent("rename", agent.id)}>Save name</button>
                            <button type="button" onClick={() => setRenamingAgentId(null)}>Cancel</button>
                          </div>
                        ) : null}
                        {removingAgentId === agent.id ? (
                          <div className="agents-confirm-inline" role="alert">
                            <span>Remove this agent? Its authority will be revoked and unfinished work returned to the scheduler.</span>
                            <button type="button" className="danger-button" disabled={busy || !!snapshot.mission.archived} onClick={() => void manageAgent("remove", agent.id)}>Confirm removal</button>
                            <button type="button" onClick={() => setRemovingAgentId(null)}>Cancel</button>
                          </div>
                        ) : null}
                        <div className="agents-worker-actions">
                          <button type="button" disabled={busy || !!snapshot.mission.archived} onClick={() => { setRenamingAgentId(agent.id); setAgentName(agent.displayName ?? agent.clientLabel ?? ""); setRemovingAgentId(null); }}>Rename</button>
                          {removed ? (
                            <button type="button" disabled={busy || !!snapshot.mission.archived} onClick={() => void manageAgent("restore", agent.id)}>Restore access</button>
                          ) : (
                            <button type="button" className="danger-button" disabled={busy || !!snapshot.mission.archived} onClick={() => { setRemovingAgentId(agent.id); setRenamingAgentId(null); }}>Remove agent</button>
                          )}
                        </div>
                      </article>
                    );
                  })}
                </div>
              </div>
            ) : null}

            {activeTab === "decisions" ? (
              <div className="agents-list">
                {snapshot.decisions.length === 0 ? <p className="agents-muted">No formal decisions yet.</p> : snapshot.decisions.map((decision) => (
                  <article key={decision.id}>
                    <div><strong>{decision.problem}</strong><span>{formatStatus(decision.stage)}</span></div>
                    <small>{decision.selectedOutcome ?? decision.proposedOutcome}</small>
                  </article>
                ))}
              </div>
            ) : null}

            {activeTab === "evidence" ? (
              <div className="agents-list">
                {evidence.length === 0 ? <p className="agents-muted">No factual evidence receipts attached yet.</p> : evidence.map(({ task, item }) => (
                  <article key={`${task.id}-${item.sourceId}`}>
                    <div><strong>{task.title}</strong><span>{formatStatus(item.verificationLevel)}</span></div>
                    <small>{item.summary}</small>
                  </article>
                ))}
              </div>
            ) : null}

            {activeTab === "activity" ? (
              <div className="agents-list">
                {[...snapshot.humanDirectives].reverse().map((item) => (
                  <article key={item.id}><div><strong>Directive</strong><span>Revision {item.authorityRevision}</span></div><small>{item.text}</small></article>
                ))}
                {[...snapshot.humanQuestions].reverse().map((item) => (
                  <article key={item.id}><div><strong>Ask Swarm</strong><span>{item.synthesizedAnswer ? "Answered" : "Open"}</span></div><small>{item.text}</small></article>
                ))}
              </div>
            ) : null}

            {activeTab === "gates" ? (
              <div className="agents-list">
                {snapshot.humanGates.length === 0 ? <p className="agents-muted">No deferred human actions.</p> : snapshot.humanGates.map((gate) => (
                  <article key={gate.id}>
                    <div><strong>{formatStatus(gate.kind)}</strong><span>{formatStatus(gate.status)}</span></div>
                    <small>{gate.reason}</small>
                    {gate.status === "pending" ? (
                      <div className="agents-gate-actions">
                        <button type="button" disabled={busy || !!snapshot.mission.archived} onClick={() => void updateGate(gate, "approve")}>Approve once</button>
                        <button type="button" disabled={busy || !!snapshot.mission.archived} onClick={() => void updateGate(gate, "reject")}>Reject</button>
                      </div>
                    ) : null}
                  </article>
                ))}
              </div>
            ) : null}
          </div>

          {pendingGates.length > 0 ? (
            <div className="agents-card agents-blocker-card">
              <strong>Deferred human actions</strong>
              <span>{pendingGates.length} action{pendingGates.length === 1 ? "" : "s"} await explicit human approval. Local Mission work continues.</span>
            </div>
          ) : null}

          {snapshot.mission.status === "readyForHumanReview" ? (
            <div className="agents-card agents-review-card">
              <div>
                <h3>Ready for human review</h3>
                <p>{snapshot.progress.verifiedCriterionCount} / {snapshot.progress.totalCriterionCount} criteria proven. Review local diff and evidence before any Git publication action.</p>
              </div>
              <button type="button" className="primary-button" disabled={busy} onClick={() => void updateMission("accept")}>Accept local result</button>
            </div>
          ) : null}

          <div className="agents-lifecycle-actions">
            {snapshot.mission.archived ? null : snapshot.mission.status === "stoppedByHuman" ? (
              <button type="button" className="primary-button" disabled={busy} onClick={() => void updateMission("resume")}>Resume Mission</button>
            ) : snapshot.mission.status === "pausedByHuman" ? (
              <button type="button" disabled={busy} onClick={() => void updateMission("resume")}>Resume</button>
            ) : snapshot.mission.status !== "completed" ? (
              <button type="button" disabled={busy} onClick={() => void updateMission("pause")}>Pause</button>
            ) : null}
            {!snapshot.mission.archived && snapshot.mission.status !== "completed" && snapshot.mission.status !== "stoppedByHuman" ? (
              <button type="button" className="danger-button" disabled={busy} onClick={() => void updateMission("stop")}>Stop</button>
            ) : null}
          </div>

          <div className="agents-record-management agents-card">
            <div>
              <strong>Mission records</strong>
              <p>{snapshot.mission.archived ? "Archived missions are read-only until restored." : "Archive a finished mission, or permanently delete only its private Mission Control records. Project files are never deleted."}</p>
              {!snapshot.mission.archived && terminalMission && !missionCanBeArchivedOrDeleted ? (
                <small>Wait for active worker leases to expire or remove/disconnect remaining agents before archiving or deleting.</small>
              ) : null}
            </div>
            <div className="agents-record-actions">
              {snapshot.mission.archived ? (
                <button type="button" disabled={busy} onClick={() => void manageMissionRecords("restore")}>Restore Mission</button>
              ) : (
                <button type="button" disabled={busy || !missionCanBeArchivedOrDeleted} title={!missionCanBeArchivedOrDeleted ? "Stop the mission and drain all agent leases first" : ""} onClick={() => void manageMissionRecords("archive")}>Archive Mission</button>
              )}
              <button type="button" className="danger-button" disabled={busy || !missionCanBeArchivedOrDeleted} onClick={() => { setDeletingMission(true); setDeleteConfirmation(""); }}>Delete Mission</button>
            </div>
            {deletingMission ? (
              <div className="agents-delete-confirm" role="alertdialog" aria-label="Confirm Mission deletion" aria-modal="false">
                <strong>Delete Mission permanently?</strong>
                <p>Only this Mission's private records, agent registrations, task history and owned temporary state will be deleted. The actual project and its Git history are preserved. This cannot be undone.</p>
                <label>Type DELETE to confirm
                  <input aria-label="Type DELETE to confirm" value={deleteConfirmation} onChange={(event) => setDeleteConfirmation(event.target.value)} autoComplete="off" />
                </label>
                <div className="agents-record-actions">
                  <button type="button" className="danger-button" disabled={busy || deleteConfirmation !== "DELETE"} onClick={() => void manageMissionRecords("delete")}>Permanently delete</button>
                  <button type="button" onClick={() => { setDeletingMission(false); setDeleteConfirmation(""); }}>Cancel</button>
                </div>
              </div>
            ) : null}
          </div>
        </>
      )}
    </section>
  );
}

export default AgentsPanel;
