// @vitest-environment jsdom
import React, { act, Profiler } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { GitRepositoryStatus, Workspace } from "../types";

const backend = vi.hoisted(() => ({
  getGitStatus: vi.fn(),
  getGitDiff: vi.fn(),
  getGitLog: vi.fn(),
  listGitActions: vi.fn(),
}));
vi.mock("../lib/backend", () => backend);
vi.mock("./ConfirmationDialog", () => ({ default: () => null }));

import GitPanel from "./GitPanel";
(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const ws = (id: string): Workspace => ({
  id, name: id, path: "/synthetic/" + id, addedAt: 1,
  accessMode: "readWrite", changePolicy: "review", commandPolicy: "review",
});
function repo(id: string): GitRepositoryStatus {
  return { available: true, branch: id, head: id, message: null,
    detached: false, ahead: 0, behind: 0, stagedCount: 0, unstagedCount: 0,
    untrackedCount: 0, conflictedCount: 0, changes: [] };
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
let host: HTMLDivElement | null = null;
let root: Root | null = null;
beforeEach(() => {
  vi.clearAllMocks();
  vi.useFakeTimers();
  backend.getGitStatus.mockImplementation(async (id: string) => repo(id));
  backend.getGitDiff.mockImplementation(async (id: string, staged: boolean) =>
    ({ staged, content: id + " synthetic diff", truncated: false }));
  backend.getGitLog.mockResolvedValue([]);
  backend.listGitActions.mockResolvedValue([]);
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

function panel(workspaces: Workspace[], running = true) {
  return <GitPanel workspaces={workspaces} gatewayRunning={running}
    onError={() => undefined} onChangeQueued={() => undefined} />;
}

describe("Git panel background read-only refresh safety", () => {
  it("does not commit React updates for identical Git status, actions, and diff snapshots", async () => {
    let commits = 0;
    await act(async () => root?.render(
      <Profiler id="git-idle" onRender={() => commits++}>
        {panel([ws("synthetic-A")])}
      </Profiler>,
    ));
    // The asynchronous initial project-selection refresh can finish during
    // the first scheduled poll. Verify *steady-state* no-op behavior after
    // that warmup instead of attributing mount work to background polling.
    await act(async () => { vi.advanceTimersByTime(3_500); });
    expect(backend.getGitStatus).toHaveBeenCalledTimes(2);
    expect(backend.getGitDiff).toHaveBeenCalledTimes(2);
    const steadyStateCommits = commits;
    for (let index = 0; index < 4; index++) {
      await act(async () => { vi.advanceTimersByTime(3_500); });
    }
    expect(backend.getGitStatus).toHaveBeenCalledTimes(6);
    expect(commits).toBe(steadyStateCommits);
  });

  it("does not duplicate a blocked 3.5-second Git poll and stops on unmount", async () => {
    const held = deferred<GitRepositoryStatus>();
    backend.getGitStatus
      .mockImplementationOnce(async () => repo("synthetic-A"))
      .mockImplementationOnce(() => held.promise);
    await act(async () => root?.render(panel([ws("synthetic-A")])));
    expect(backend.getGitStatus).toHaveBeenCalledTimes(1);

    await act(async () => { vi.advanceTimersByTime(3500); });
    expect(backend.getGitStatus).toHaveBeenCalledTimes(2);
    await act(async () => { vi.advanceTimersByTime(10500); });
    expect(backend.getGitStatus).toHaveBeenCalledTimes(2);
    expect(backend.listGitActions).toHaveBeenCalledTimes(2);

    await act(async () => held.resolve(repo("synthetic-A")));
    await act(async () => { vi.advanceTimersByTime(3500); });
    expect(backend.getGitStatus).toHaveBeenCalledTimes(3);
    await act(async () => root?.unmount());
    root = null;
    await act(async () => { vi.advanceTimersByTime(7000); });
    expect(backend.getGitStatus).toHaveBeenCalledTimes(3);
  });

  it("keeps new-project Git state when an older project diff finally resolves", async () => {
    const held = deferred<{staged: boolean; content: string; truncated: boolean}>();
    backend.getGitDiff.mockImplementation((id: string, staged: boolean) =>
      id === "synthetic-A" ? held.promise
        : Promise.resolve({ staged, content: "ONLY PROJECT B DIFF", truncated: false }));
    await act(async () => root?.render(panel([ws("synthetic-A")])));
    expect(backend.getGitDiff).toHaveBeenCalledWith("synthetic-A", false);
    await act(async () => root?.render(panel([ws("synthetic-B")])));
    expect(backend.getGitDiff).toHaveBeenCalledWith("synthetic-B", false);
    expect(host?.textContent).toContain("ONLY PROJECT B DIFF");
    await act(async () => held.resolve({
      staged: false, content: "STALE PROJECT A DIFF", truncated: false,
    }));
    expect(host?.textContent).toContain("ONLY PROJECT B DIFF");
    expect(host?.textContent).not.toContain("STALE PROJECT A DIFF");
  });

  it("ignores a delayed poll after switching between staged and unstaged diffs", async () => {
    const held = deferred<{staged:boolean;content:string;truncated:boolean}>();
    backend.getGitDiff.mockImplementation((id: string, staged: boolean) => {
      if (staged) return Promise.resolve({
        staged: true, content: "CURRENT STAGED DIFF", truncated: false,
      });
      // Initial current unstaged load is fast; later background poll is slow.
      return backend.getGitDiff.mock.calls.length === 1
        ? Promise.resolve({ staged: false, content: "CURRENT UNSTAGED DIFF", truncated: false })
        : held.promise;
    });
    await act(async () => root?.render(panel([ws("synthetic-A")])));
    await act(async () => { vi.advanceTimersByTime(3500); });
    const stagedButton = Array.from(host!.querySelectorAll("button"))
      .find(b => b.textContent === "Staged")!;
    await act(async () => stagedButton.click());
    expect(host?.textContent).toContain("CURRENT STAGED DIFF");
    await act(async () => held.resolve({
      staged: false, content: "STALE UNSTAGED DIFF", truncated: false,
    }));
    expect(host?.textContent).toContain("CURRENT STAGED DIFF");
    expect(host?.textContent).not.toContain("STALE UNSTAGED DIFF");
  });
});
