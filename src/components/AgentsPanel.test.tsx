// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
// @ts-expect-error -- Vitest runs in Node while the app tsconfig intentionally omits Node typings.
import { readFileSync } from "node:fs";

const appSource = readFileSync("src/App.tsx", "utf8");

const backend = vi.hoisted(() => ({
  listAgentMissions: vi.fn(),
  getAgentMission: vi.fn(),
  createAgentMission: vi.fn(),
  archiveAgentMission: vi.fn(),
  restoreArchivedAgentMission: vi.fn(),
  deleteAgentMission: vi.fn(),
  renameMissionAgent: vi.fn(),
  removeMissionAgent: vi.fn(),
  restoreMissionAgent: vi.fn(),
  submitAgentMissionDirective: vi.fn(),
  submitAgentMissionQuestion: vi.fn(),
  pauseAgentMission: vi.fn(),
  resumeAgentMission: vi.fn(),
  stopAgentMission: vi.fn(),
  approveAgentMissionGate: vi.fn(),
  rejectAgentMissionGate: vi.fn(),
  acceptAgentMissionResult: vi.fn(),
}));

vi.mock("../lib/backend", () => backend);

import AppSidebar from "./AppSidebar";
import AgentsPanel from "./AgentsPanel";
import type { Workspace } from "../types";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const workspace: Workspace = {
  id: "workspace-1",
  name: "RepoTunnel",
  path: "/project",
  addedAt: 1,
  accessMode: "readWrite",
  changePolicy: "review",
  commandPolicy: "automatic",
};

const summary = {
  id: "mission-1",
  workspaceId: "workspace-1",
  goal: "Ship Mission Control",
  status: "running",
  authorityRevision: 7,
  agentCount: 3,
  activeAgentCount: 2,
  knownTaskCount: 4,
  integratedTaskCount: 2,
  pendingHumanGateCount: 1,
  updatedAt: 100,
};

function snapshot(overrides: Record<string, unknown> = {}) {
  return {
    mission: {
      id: "mission-1",
      workspaceId: "workspace-1",
      goal: "Ship Mission Control",
      status: "running",
      autonomyPolicy: "fullLocal",
      authorityRevision: 7,
      missionRevision: 7,
      createdAt: 1,
      updatedAt: 100,
    },
    progress: {
      registeredAgentCount: 3,
      activeAgentCount: 2,
      knownTaskCount: 4,
      readyTaskCount: 1,
      inProgressTaskCount: 1,
      blockedTaskCount: 1,
      verifiedTaskCount: 0,
      integratedTaskCount: 2,
      pendingHumanGateCount: 1,
      totalCriterionCount: 3,
      verifiedCriterionCount: 2,
    },
    agents: [
      {
        id: "agent-a",
        clientLabel: "ChatGPT",
        status: "busy",
        binding: {
          lastSeenAt: 90,
          leaseExpiresAt: 999,
          acknowledgedAuthorityRevision: 7,
          assignment: { id: "assign-1", kind: "implement", taskId: "task-1", authorityRevision: 7, assignedAt: 80, leaseExpiresAt: 999 },
        },
      },
    ],
    tasks: [
      {
        id: "task-1",
        title: "Implement scheduler",
        status: "inProgress",
        priority: 5,
        requestedPaths: ["src-tauri/src/mission"],
        ownerAgentId: "agent-a",
        reviewerAgentId: null,
        evidence: [],
        review: null,
      },
      {
        id: "task-2",
        title: "External publication",
        status: "blockedExternal",
        priority: 3,
        requestedPaths: [],
        ownerAgentId: null,
        reviewerAgentId: null,
        evidence: [],
        review: null,
      },
    ],
    decisions: [
      { id: "decision-1", problem: "Pick isolation", significance: "significant", stage: "finalized", proposedOutcome: "Sparse overlay", finalOutcome: "Sparse overlay" },
    ],
    humanDirectives: [
      { id: "directive-1", text: "Keep Team Mode untouched", authorityRevision: 7, createdAt: 70 },
    ],
    humanQuestions: [
      { id: "question-1", text: "What remains?", authorityRevision: 7, createdAt: 75, synthesizedAnswer: "UI and scale gates remain.", answeredAt: 76 },
    ],
    humanGates: [
      { id: "gate-1", kind: "gitPush", status: "pending", taskId: "task-2", resource: "gitPush|sha256=x", reason: "Publish requires human approval", authorityRevision: 7, requestedAt: 90, expiresAt: 990 },
    ],
    completionAudit: null,
    ...overrides,
  };
}

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function flush() {
  await act(async () => {
    await new Promise((resolve) => window.setTimeout(resolve, 0));
  });
}

async function renderAgents(initialSnapshot = snapshot(), summaries: unknown[] = [summary]) {
  backend.listAgentMissions.mockResolvedValue(summaries);
  backend.getAgentMission.mockResolvedValue(initialSnapshot);
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => {
    root?.render(
      <AgentsPanel
        workspaces={[workspace]}
        selectedWorkspaceId="workspace-1"
        onSelectWorkspace={vi.fn()}
        onNotice={vi.fn()}
      />,
    );
  });
  await flush();
  return host;
}

beforeEach(() => {
  vi.clearAllMocks();
  backend.createAgentMission.mockResolvedValue(snapshot());
  backend.archiveAgentMission.mockResolvedValue(snapshot({ mission: { ...snapshot().mission, status: "stoppedByHuman", archived: true } }));
  backend.restoreArchivedAgentMission.mockResolvedValue(snapshot({ mission: { ...snapshot().mission, status: "stoppedByHuman", archived: false } }));
  backend.deleteAgentMission.mockResolvedValue(undefined);
  backend.renameMissionAgent.mockResolvedValue(snapshot());
  backend.removeMissionAgent.mockResolvedValue(snapshot());
  backend.restoreMissionAgent.mockResolvedValue(snapshot());
  backend.submitAgentMissionDirective.mockResolvedValue(snapshot());
  backend.submitAgentMissionQuestion.mockResolvedValue({ id: "question-new", text: "status?" });
  backend.pauseAgentMission.mockResolvedValue(snapshot({ mission: { ...snapshot().mission, status: "pausedByHuman" } }));
  backend.resumeAgentMission.mockResolvedValue(snapshot());
  backend.stopAgentMission.mockResolvedValue(snapshot({ mission: { ...snapshot().mission, status: "stoppedByHuman" } }));
  backend.approveAgentMissionGate.mockResolvedValue({ ...snapshot().humanGates[0], status: "approvedOnce" });
  backend.rejectAgentMissionGate.mockResolvedValue({ ...snapshot().humanGates[0], status: "rejected" });
  backend.acceptAgentMissionResult.mockResolvedValue(snapshot({ mission: { ...snapshot().mission, status: "completed" } }));
});

afterEach(() => {
  act(() => root?.unmount());
  host?.remove();
  root = null;
  host = null;
});

describe("Agents / Mission Control", () => {
  it("keeps archived missions read-only until explicitly restored", async () => {
    const archived = snapshot({ mission: { ...snapshot().mission, status: "stoppedByHuman", archived: true } });
    const view = await renderAgents(archived, [{ ...summary, archived: true }]);
    expect(view.textContent).toContain("Mission archived");
    expect(Array.from(view.querySelectorAll("button")).some((button) => button.textContent === "Restore Mission")).toBe(true);
    expect(Array.from(view.querySelectorAll<HTMLButtonElement>("button")).find((button) => button.textContent === "Send Directive")?.disabled).toBe(true);
    const agentsTab = Array.from(view.querySelectorAll<HTMLButtonElement>('[role="tab"]')).find((button) => button.textContent === "Agents");
    await act(async () => agentsTab?.click());
    expect(Array.from(view.querySelectorAll<HTMLButtonElement>("button")).find((button) => button.textContent === "Remove agent")?.disabled).toBe(true);
  });

  it("blocks deleting an active mission even before the confirmation flow", async () => {
    const view = await renderAgents();
    const action = Array.from(view.querySelectorAll<HTMLButtonElement>("button")).find((button) => button.textContent === "Delete Mission");
    expect(action?.disabled).toBe(true);
    await act(async () => action?.click());
    expect(view.querySelector('[aria-label="Type DELETE to confirm"]')).toBeNull();
    expect(backend.deleteAgentMission).not.toHaveBeenCalled();
  });

  it("explains an offline swarm and supports focused task-status filtering", async () => {
    const offline = snapshot({ progress: { ...snapshot().progress, activeAgentCount: 0 } });
    const view = await renderAgents(offline);
    expect(view.textContent).toContain("No agents currently connected");
    expect(view.textContent).toContain("Copy Mission ID");
    const tasks = Array.from(view.querySelectorAll<HTMLButtonElement>('[role="tab"]')).find((button) => button.textContent === "Tasks");
    await act(async () => tasks?.click());
    expect(view.textContent).toContain("Autonomous task queue");
    const filter = view.querySelector<HTMLSelectElement>('select[aria-label="Filter tasks"]');
    await act(async () => {
      if (filter) {
        filter.value = "pending";
        filter.dispatchEvent(new Event("change", { bubbles: true }));
      }
    });
    expect(view.textContent).toContain("External publication");
    expect(view.textContent).not.toContain("Implement scheduler");
  });

  it("keeps New Mission available while viewing an existing Mission", async () => {
    const view = await renderAgents();
    const newMission = Array.from(view.querySelectorAll("button")).find((button) => button.textContent?.includes("+ New Mission"));
    expect(newMission).toBeDefined();
    await act(async () => newMission?.click());
    expect(view.querySelector('textarea[name="missionGoal"]')).not.toBeNull();
    expect(view.textContent).toContain("Back to Mission");
    const goal = view.querySelector<HTMLTextAreaElement>('textarea[name="missionGoal"]');
    await act(async () => {
      if (goal) goal.value = "A new independently scoped mission";
    });
    const start = Array.from(view.querySelectorAll("button")).find((button) => button.textContent?.trim() === "Start Mission");
    await act(async () => { start?.click(); await Promise.resolve(); });
    expect(backend.createAgentMission).toHaveBeenCalledWith("workspace-1", "A new independently scoped mission");
  });

  it("requires a second confirmation before removing an agent", async () => {
    const view = await renderAgents();
    const tab = Array.from(view.querySelectorAll<HTMLButtonElement>('[role="tab"]')).find((button) => button.textContent === "Agents");
    await act(async () => tab?.click());
    expect(view.textContent).toContain("Last heartbeat:");
    const remove = Array.from(view.querySelectorAll("button")).find((button) => button.textContent === "Remove agent");
    await act(async () => remove?.click());
    expect(backend.removeMissionAgent).not.toHaveBeenCalled();
    const confirm = Array.from(view.querySelectorAll("button")).find((button) => button.textContent === "Confirm removal");
    await act(async () => { confirm?.click(); await Promise.resolve(); });
    expect(backend.removeMissionAgent).toHaveBeenCalledWith("mission-1", "agent-a");
  });

  it("archives a drained stopped mission and allows it to be restored", async () => {
    const stopped = snapshot({
      mission: { ...snapshot().mission, status: "stoppedByHuman", archived: false },
      agents: [{ ...snapshot().agents[0], status: "offline", binding: { ...snapshot().agents[0].binding, leaseExpiresAt: 0, assignment: null } }],
    });
    const view = await renderAgents(stopped);
    const archive = Array.from(view.querySelectorAll("button")).find((button) => button.textContent === "Archive Mission");
    expect(archive?.disabled).toBe(false);
    await act(async () => { archive?.click(); await Promise.resolve(); });
    expect(backend.archiveAgentMission).toHaveBeenCalledWith("mission-1");
    const restore = Array.from(view.querySelectorAll("button")).find((button) => button.textContent === "Restore Mission");
    expect(restore).toBeDefined();
  });

  it("requires explicit DELETE before invoking local-only Mission deletion", async () => {
    const stopped = snapshot({
      mission: { ...snapshot().mission, status: "stoppedByHuman" },
      agents: [{ ...snapshot().agents[0], status: "offline", binding: { ...snapshot().agents[0].binding, leaseExpiresAt: 0, assignment: null } }],
    });
    const view = await renderAgents(stopped);
    const action = Array.from(view.querySelectorAll("button")).find((button) => button.textContent === "Delete Mission");
    expect(action?.disabled).toBe(false);
    await act(async () => action?.click());
    const destructive = Array.from(view.querySelectorAll("button")).find((button) => button.textContent === "Permanently delete");
    expect(destructive?.disabled).toBe(true);
    expect(backend.deleteAgentMission).not.toHaveBeenCalled();
    const input = view.querySelector<HTMLInputElement>('input[aria-label="Type DELETE to confirm"]');
    await act(async () => {
      if (input) {
        const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
        setter?.call(input, "DELETE");
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }
    });
    expect(destructive?.disabled).toBe(false);
    backend.listAgentMissions.mockResolvedValue([]);
    await act(async () => { destructive?.click(); await Promise.resolve(); });
    expect(backend.deleteAgentMission).toHaveBeenCalledWith("mission-1");
    expect(view.textContent).toContain("Start Mission");
  });
  it("keeps Agents navigation separate from existing GPT Agents", async () => {
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    const onNavigate = vi.fn();
    await act(async () => {
      root?.render(<AppSidebar activeView="agents" pendingCount={0} onNavigate={onNavigate} />);
    });
    const labels = Array.from(host.querySelectorAll("button")).map((button) => button.textContent?.trim());
    expect(labels).toContain("Agents");
    expect(labels).toContain("GPT Agents");
    expect(host.querySelector("button.active")?.textContent).toContain("Agents");
  });

  it("creates a Mission from project plus goal without manual agent-role fields", async () => {
    const view = await renderAgents(snapshot(), []);
    const goal = view.querySelector<HTMLTextAreaElement>('textarea[name="missionGoal"]');
    expect(goal).not.toBeNull();
    expect(view.textContent).not.toContain("Agent A role");
    await act(async () => {
      if (goal) {
        goal.value = "Implement autonomous Mission Control";
        goal.dispatchEvent(new Event("input", { bubbles: true }));
      }
    });
    const start = Array.from(view.querySelectorAll("button")).find((button) => button.textContent?.includes("Start Mission"));
    await act(async () => {
      start?.click();
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });
    expect(backend.createAgentMission).toHaveBeenCalledWith("workspace-1", "Implement autonomous Mission Control");
  });

  it("submits a Directive and clears the input without pausing the Mission", async () => {
    const view = await renderAgents();
    const input = view.querySelector<HTMLTextAreaElement>('textarea[name="missionDirective"]');
    expect(input).not.toBeNull();
    await act(async () => {
      if (input) {
        input.value = "Keep compatibility with Team Mode";
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }
    });
    const send = Array.from(view.querySelectorAll("button")).find((button) => button.textContent?.includes("Send Directive"));
    await act(async () => {
      send?.click();
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });
    expect(backend.submitAgentMissionDirective).toHaveBeenCalledWith("mission-1", "Keep compatibility with Team Mode");
    expect(backend.pauseAgentMission).not.toHaveBeenCalled();
    expect(input?.value).toBe("");
  });

  it("Ask Swarm submits a question and keeps Running state", async () => {
    const view = await renderAgents();
    const input = view.querySelector<HTMLInputElement>('input[name="missionQuestion"]');
    expect(input).not.toBeNull();
    await act(async () => {
      if (input) {
        input.value = "What remains?";
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }
    });
    const ask = Array.from(view.querySelectorAll("button")).find((button) => button.textContent?.includes("Ask Swarm"));
    await act(async () => {
      ask?.click();
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });
    expect(backend.submitAgentMissionQuestion).toHaveBeenCalledWith("mission-1", "What remains?");
    expect(backend.pauseAgentMission).not.toHaveBeenCalled();
    expect(view.textContent).toContain("Running");
    expect(view.textContent).toContain("UI and scale gates remain.");
  });

  it("refreshes the selected Mission promptly while autonomous agents continue working", async () => {
    const view = await renderAgents();
    backend.getAgentMission.mockResolvedValue(
      snapshot({
        progress: {
          ...snapshot().progress,
          registeredAgentCount: 5,
          activeAgentCount: 4,
          knownTaskCount: 6,
          verifiedTaskCount: 3,
        },
      }),
    );

    await act(async () => {
      await new Promise((resolve) => window.setTimeout(resolve, 650));
    });

    expect(backend.getAgentMission.mock.calls.length).toBeGreaterThan(1);
    expect(view.textContent).toContain("4 / 5");
    expect(view.textContent).toContain("3 / 6");
  });

  it("does not let an older slow Mission selection overwrite the latest selection", async () => {
    const secondSummary = { ...summary, id: "mission-2", goal: "Older slow response" };
    const view = await renderAgents(snapshot(), [summary, secondSummary]);
    let resolveSlow: ((value: ReturnType<typeof snapshot>) => void) | null = null;
    const slow = new Promise<ReturnType<typeof snapshot>>((resolve) => {
      resolveSlow = resolve;
    });
    const missionOne = snapshot({
      mission: { ...snapshot().mission, id: "mission-1", goal: "Latest selected Mission" },
    });
    const missionTwo = snapshot({
      mission: { ...snapshot().mission, id: "mission-2", goal: "Older slow response" },
    });
    backend.getAgentMission.mockImplementation((missionId: string) =>
      missionId === "mission-2" ? slow : Promise.resolve(missionOne),
    );

    const selects = view.querySelectorAll<HTMLSelectElement>("select");
    const missionSelect = selects[1];
    await act(async () => {
      missionSelect.value = "mission-2";
      missionSelect.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await act(async () => {
      missionSelect.value = "mission-1";
      missionSelect.dispatchEvent(new Event("change", { bubbles: true }));
      await Promise.resolve();
    });
    expect(view.querySelector(".agents-overview strong")?.textContent).toBe("Latest selected Mission");

    await act(async () => {
      resolveSlow?.(missionTwo);
      await Promise.resolve();
    });
    expect(view.querySelector(".agents-overview strong")?.textContent).toBe("Latest selected Mission");
  });

  it("shows Mission ID and keeps long goals compact until explicitly expanded", async () => {
    const longGoal = `${"Build reliable autonomous Mission Control. ".repeat(30)}END-OF-FULL-GOAL`;
    const view = await renderAgents(snapshot({ mission: { ...snapshot().mission, goal: longGoal } }));
    expect(view.textContent).toContain("Mission ID");
    expect(view.textContent).toContain("mission-1");
    expect(view.textContent).toContain("Copy ID");
    expect(view.textContent).toContain("View full goal");
    expect(view.textContent).not.toContain("END-OF-FULL-GOAL");

    const expand = Array.from(view.querySelectorAll("button")).find((button) => button.textContent?.includes("View full goal"));
    await act(async () => expand?.click());
    expect(view.textContent).toContain("END-OF-FULL-GOAL");
    expect(view.textContent).toContain("Collapse goal");
  });

  it("shows Resume Mission for a stopped Mission instead of leaving it terminal in the UI", async () => {
    const stopped = snapshot({ mission: { ...snapshot().mission, status: "stoppedByHuman" } });
    const view = await renderAgents(stopped);
    const resume = Array.from(view.querySelectorAll("button")).find((button) => button.textContent?.includes("Resume Mission"));
    expect(resume).not.toBeUndefined();
    await act(async () => {
      resume?.click();
      await Promise.resolve();
    });
    expect(backend.resumeAgentMission).toHaveBeenCalledWith("mission-1");
  });

  it("wires Agents and GPT Agents to independent workspace-selection state", () => {
    expect(appSource).toContain("agentsWorkspaceId");
    expect(appSource).toContain("teamWorkspaceId");
    expect(appSource).toContain("selectedWorkspaceId={agentsWorkspaceId}");
    expect(appSource).toContain("selectedWorkspaceId={teamWorkspaceId}");
    expect(appSource).toContain("onSelectWorkspace={setAgentsWorkspaceId}");
    expect(appSource).toContain("onSelectWorkspace={setTeamWorkspaceId}");
  });

  it("presents human-authority actions as deferred without implying Mission work is blocked", async () => {
    const view = await renderAgents();
    expect(view.textContent).toContain("Deferred actions");
    expect(view.textContent).toContain("Deferred human actions");
    expect(view.textContent).toContain("Local Mission work continues.");
    expect(view.textContent).not.toContain("Human gates / blockers");
  });

  it("shows operational state, human gates, controls, and ready-for-review acceptance", async () => {
    const ready = snapshot({
      mission: { ...snapshot().mission, status: "readyForHumanReview" },
      progress: { ...snapshot().progress, verifiedCriterionCount: 3, readyTaskCount: 0, inProgressTaskCount: 0, blockedTaskCount: 0, integratedTaskCount: 4 },
      humanGates: [],
      completionAudit: { authorityRevision: 7, requiredAuditorCount: 2, assignedAgentIds: ["agent-b", "agent-c"], passedAgentIds: ["agent-b", "agent-c"], findings: [], startedAt: 80, completedAt: 90 },
    });
    const view = await renderAgents(ready);
    expect(view.textContent).toContain("Ready for human review");
    expect(view.textContent).toContain("3 / 3 criteria proven");
    expect(view.textContent).toContain("Tasks");
    expect(view.textContent).toContain("Agents");
    expect(view.textContent).toContain("Decisions");
    expect(view.textContent).toContain("Evidence");
    expect(view.textContent).toContain("Activity");
    expect(view.textContent).toContain("Accept local result");
    expect(view.textContent).toContain("Pause");
    expect(view.textContent).toContain("Stop");
  });
});
