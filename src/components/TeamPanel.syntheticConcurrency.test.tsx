// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { TeamSnapshot, Workspace } from "../types";

const backend = vi.hoisted(() => ({
  getTeamSession: vi.fn(),
  listTeamSessions: vi.fn(),
  createTeamSession: vi.fn(),
  disconnectTeamAgent: vi.fn(),
  completeTeamSession: vi.fn(),
  deleteTeamSession: vi.fn(),
  pauseTeamSession: vi.fn(),
  resumeTeamSession: vi.fn(),
}));
const listeners = vi.hoisted(() => ({ callbacks: [] as Array<(event: { payload: string }) => void> }));
vi.mock("../lib/backend", () => backend);
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_name: string, callback: (event: { payload: string }) => void) => {
    listeners.callbacks.push(callback);
    return () => { listeners.callbacks = listeners.callbacks.filter(item => item !== callback); };
  }),
}));

import TeamPanel from "./TeamPanel";
(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const workspace: Workspace = {
  id: "synthetic-parallel-team", name: "Private Synthetic Team",
  path: "/test-only/parallel", addedAt: 1,
  accessMode: "readWrite", changePolicy: "review", commandPolicy: "review",
};

type TeamPhase = "planning" | "executing" | "reviewing" | "verifying";
function snapshot(revision: number, phase: TeamPhase, completed = false): TeamSnapshot {
  const agents = [
    { id: "agent-a-synthetic", name: "Engineer A", role: "Platform",
      status: completed ? "idle" : "active", joinedAt: 100, lastSeenAt: 100,
      clientLabel: "Synthetic A", currentTaskId: completed ? null : "task-a",
      manuallyDisconnected: false },
    { id: "agent-b-synthetic", name: "Engineer B", role: "Product",
      status: completed ? "idle" : "active", joinedAt: 100, lastSeenAt: 100,
      clientLabel: "Synthetic B", currentTaskId: completed ? null : "task-b",
      manuallyDisconnected: false },
  ];
  const tasks = [
    { id: "task-a", title: "Platform implementation", status: completed ? "done" : "inProgress",
      ownerAgentId: agents[0].id, reviewerAgentId: agents[1].id,
      contributorAgentIds: completed ? [agents[0].id] : [],
      cycleNumber: 1, priority: 4, createdAt: 1 },
    { id: "task-b", title: "Product implementation", status: completed ? "done" : "inProgress",
      ownerAgentId: agents[1].id, reviewerAgentId: agents[0].id,
      contributorAgentIds: completed ? [agents[1].id] : [],
      cycleNumber: 1, priority: 4, createdAt: 2 },
  ];
  return {
    session: {
      id: "session-existing-synthetic", workspaceId: workspace.id,
      workspaceName: workspace.name, goal: "Test two-agent UI safely",
      currentRequest: "Synthetic independent product improvements",
      status: "active", phase, cycleNumber: 1, revision,
      successCriteria: ["Both agents implement"], criterionChecks: [],
      completedCycles: [], locks: [], tasks, agents,
    },
    progress: {
      progressPercent: completed ? 100 : 30,
      doneTaskCount: completed ? 2 : 0,
      openTaskCount: completed ? 0 : 2,
      blockedTaskCount: 0, verifiedCriterionCount: completed ? 1 : 0,
      totalCriterionCount: 1,
    },
    recommendedAction: null,
  } as unknown as TeamSnapshot;
}

let root: Root | null = null;
let host: HTMLDivElement | null = null;
const onNotice = vi.fn();

beforeEach(() => {
  vi.clearAllMocks();
  vi.useFakeTimers();
  listeners.callbacks.length = 0;
  backend.listTeamSessions.mockResolvedValue([{
    id: "session-existing-synthetic", workspaceId: workspace.id,
    status: "active", updatedAt: 100, goal: "Test two-agent UI safely",
  }]);
  backend.getTeamSession.mockResolvedValue(snapshot(1, "planning"));
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});
afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
  vi.useRealTimers();
});
async function mount() {
  await act(async () => root?.render(
    <TeamPanel workspaces={[workspace]} selectedWorkspaceId={workspace.id}
      onSelectWorkspace={() => undefined} onNotice={onNotice} />,
  ));
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
}

describe("synthetic two-agent Team UI safety, without any real Team backend", () => {
  it("shows distinct A and B ownership during parallel work and cross-review", async () => {
    await mount();
    expect(host?.textContent).toContain("2/2 attached");
    backend.getTeamSession.mockResolvedValue(snapshot(2, "executing"));
    await act(async () => { await vi.advanceTimersByTimeAsync(4_000); });
    const cards = [...host!.querySelectorAll(".team-agent-card")];
    expect(cards).toHaveLength(2);
    expect(cards[0].textContent).toContain("Platform implementation");
    expect(cards[1].textContent).toContain("Product implementation");
    expect(host?.textContent).toContain("Reviewer: Engineer B");
    expect(host?.textContent).toContain("Reviewer: Engineer A");
    backend.getTeamSession.mockResolvedValue(snapshot(3, "reviewing", true));
    await act(async () => { await vi.advanceTimersByTimeAsync(4_000); });
    expect(host?.textContent).toContain("2 done");
    const latest = [...host!.querySelectorAll(".team-agent-card")];
    expect(latest[0].textContent).toContain("Implemented work");
    expect(latest[1].textContent).toContain("Implemented work");
    expect(backend.createTeamSession).not.toHaveBeenCalled();
    expect(backend.disconnectTeamAgent).not.toHaveBeenCalled();
  });

  it("refreshes a running Team from synthetic events without destructive actions", async () => {
    await mount();
    expect(listeners.callbacks.length).toBeGreaterThan(0);
    backend.getTeamSession.mockResolvedValue(snapshot(7, "verifying", true));
    await act(async () => {
      for (const handler of [...listeners.callbacks]) {
        handler({ payload: "session-existing-synthetic" });
      }
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(host?.textContent).toContain("2/2 attached");
    expect(host?.textContent).toContain("Platform implementation");
    expect(host?.textContent).toContain("Product implementation");
    expect(host?.textContent).toContain("Implemented work");
    for (const mutation of [
      backend.createTeamSession, backend.disconnectTeamAgent,
      backend.completeTeamSession, backend.deleteTeamSession,
      backend.pauseTeamSession, backend.resumeTeamSession,
    ]) expect(mutation).not.toHaveBeenCalled();
  });

  it("keeps both visible agent roles when a transient polling read fails", async () => {
    await mount();
    expect(host?.querySelectorAll(".team-agent-card")).toHaveLength(2);
    backend.getTeamSession.mockRejectedValueOnce(new Error("simulated transient read failure"));
    await act(async () => { await vi.advanceTimersByTimeAsync(4_000); });
    expect(host?.querySelectorAll(".team-agent-card")).toHaveLength(2);
    expect(host?.textContent).toContain("2/2 attached");
    expect(onNotice).not.toHaveBeenCalled();
    expect(backend.createTeamSession).not.toHaveBeenCalled();
  });
});
