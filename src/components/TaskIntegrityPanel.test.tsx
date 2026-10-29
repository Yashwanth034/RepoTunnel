// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  getTaskIntegrityStatus: vi.fn(),
  approveTaskIntegrityExpansion: vi.fn(),
  rejectTaskIntegrityExpansion: vi.fn(),
  cancelTaskIntegrity: vi.fn(),
}));

vi.mock("../lib/backend", () => backend);

import TaskIntegrityPanel from "./TaskIntegrityPanel";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function renderPanel(status: unknown) {
  backend.getTaskIntegrityStatus.mockResolvedValue(status);
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  await act(async () => {
    root?.render(
      <TaskIntegrityPanel
        workspaceId="workspace-1"
        onError={vi.fn()}
        onNotice={vi.fn()}
      />,
    );
  });
  await act(async () => {
    await new Promise((resolve) => window.setTimeout(resolve, 0));
  });
  return host;
}

function baseTask(overrides: Record<string, unknown> = {}) {
  return {
    taskId: "task-1",
    intentSummary: "Keep edits inside the approved source files.",
    status: "active",
    scopeRevision: 3,
    scopeSummary: ["src/main.rs · edit", "src/lib.rs · edit"],
    actualChangedCount: 2,
    remainingBudget: {
      existingFiles: 3,
      newFiles: 1,
      deletes: 0,
      renames: 0,
      changedLines: 120,
    },
    protectedDirtyCount: 1,
    verification: {
      requiredLevel: "subsystemRegression",
      completedLevel: null,
      evidenceCount: 0,
      complete: false,
    },
    pendingReview: false,
    conflict: false,
    integrityViolation: false,
    completionReceipt: null,
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  backend.approveTaskIntegrityExpansion.mockResolvedValue(undefined);
  backend.rejectTaskIntegrityExpansion.mockResolvedValue(undefined);
  backend.cancelTaskIntegrity.mockResolvedValue(undefined);
});

afterEach(() => {
  act(() => root?.unmount());
  host?.remove();
  root = null;
  host = null;
});

describe("TaskIntegrityPanel", () => {
  it("renders the compact no-task state", async () => {
    const view = await renderPanel({ task: null, pendingExpansions: [] });
    expect(view.textContent).toContain("Task Integrity");
    expect(view.textContent).toContain("No active task");
  });

  it("renders active task scope and factual usage", async () => {
    const view = await renderPanel({ task: baseTask(), pendingExpansions: [] });
    expect(view.textContent).toContain("Active");
    expect(view.textContent).toContain("Keep edits inside the approved source files.");
    expect(view.textContent).toContain("Scope revision 3");
    expect(view.textContent).toContain("2 changed");
    expect(view.textContent).toContain("1 protected dirty");
    expect(view.textContent).toContain("src/main.rs");
  });

  it("shows awaiting scope approval with local approve and reject actions", async () => {
    const view = await renderPanel({
      task: baseTask({ status: "awaitingScopeApproval" }),
      pendingExpansions: [
        {
          expansionId: "expand-1",
          relativePath: "src/helper.rs",
          operation: "sourceEdit",
          reason: "Compiler diagnostic proves the helper is required.",
        },
      ],
    });
    expect(view.textContent).toContain("Awaiting scope approval");
    expect(view.textContent).toContain("src/helper.rs");
    expect(Array.from(view.querySelectorAll("button")).map((button) => button.textContent)).toEqual(
      expect.arrayContaining(["Approve", "Reject"]),
    );
  });

  it("surfaces pending approvals for two active tasks and acts on the exact expansion", async () => {
    const first = baseTask({
      taskId: "task-a",
      intentSummary: "First active principal",
      status: "awaitingScopeApproval",
    });
    const second = baseTask({
      taskId: "task-b",
      intentSummary: "Second active principal",
      status: "awaitingScopeApproval",
    });
    const status = {
      task: second,
      activeTasks: [second, first],
      pendingExpansions: [
        {
          expansionId: "expand-a",
          taskId: "task-a",
          relativePath: "src/a.rs",
          operation: "sourceEdit",
          reason: "First task approval.",
        },
        {
          expansionId: "expand-b",
          taskId: "task-b",
          relativePath: "src/b.rs",
          operation: "sourceEdit",
          reason: "Second task approval.",
        },
      ],
    };
    const view = await renderPanel(status);
    expect(view.textContent).toContain("First active principal");
    expect(view.textContent).toContain("Second active principal");
    expect(view.textContent).toContain("src/a.rs");
    expect(view.textContent).toContain("src/b.rs");

    const secondApproval = Array.from(view.querySelectorAll("article")).find((article) =>
      article.textContent?.includes("src/b.rs"),
    );
    const approve = secondApproval?.querySelector<HTMLButtonElement>("button.primary-button");
    expect(approve).not.toBeNull();
    await act(async () => {
      approve?.click();
      await new Promise((resolve) => window.setTimeout(resolve, 0));
    });
    expect(backend.approveTaskIntegrityExpansion).toHaveBeenCalledWith("workspace-1", "expand-b");
  });

  it("surfaces a frozen integrity violation prominently", async () => {
    const view = await renderPanel({
      task: baseTask({
        status: "frozenIntegrityViolation",
        integrityViolation: true,
      }),
      pendingExpansions: [],
    });
    expect(view.textContent).toContain("Integrity violation");
    expect(view.textContent).toContain("Task mutations are frozen");
  });

  it("shows factual verification progress", async () => {
    const view = await renderPanel({
      task: baseTask({
        status: "verifying",
        verification: {
          requiredLevel: "subsystemRegression",
          completedLevel: "changeSpecific",
          evidenceCount: 1,
          complete: false,
        },
      }),
      pendingExpansions: [],
    });
    expect(view.textContent).toContain("Verifying");
    expect(view.textContent).toContain("Subsystem regression");
    expect(view.textContent).toContain("1 evidence");
  });

  it("renders the succeeded Integrity Receipt summary", async () => {
    const view = await renderPanel({
      task: baseTask({
        status: "succeeded",
        verification: {
          requiredLevel: "subsystemRegression",
          completedLevel: "subsystemRegression",
          evidenceCount: 3,
          complete: true,
        },
        completionReceipt: {
          changedPathCount: 3,
          changedPathsTruncated: false,
          acceptedScopeExpansions: 1,
          deniedScopeExpansions: 0,
          completedAt: 1_700_000_000_000,
        },
      }),
      pendingExpansions: [],
    });
    expect(view.textContent).toContain("Succeeded");
    expect(view.textContent).toContain("3 changed paths");
    expect(view.textContent).toContain("Integrity Receipt");
  });
});
