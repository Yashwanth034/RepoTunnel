// @vitest-environment jsdom
import React, { act, Profiler } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { GitRepositoryStatus } from "../types";
import { useProjectGitStatus } from "./projectGitStatus";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

function status(branch: string): GitRepositoryStatus {
  return {
    available: true, message: null, branch, head: branch,
    detached: false, ahead: 0, behind: 0, stagedCount: 0,
    unstagedCount: 1, untrackedCount: 0, conflictedCount: 0,
    changes: [{ path: "synthetic.ts", indexStatus: " ", worktreeStatus: "M",
      staged: false, unstaged: true, untracked: false, conflicted: false }],
  };
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

let root: Root | null;
let host: HTMLDivElement | null;
let refresh: (() => Promise<void>) | null;
let commits: number;
beforeEach(() => {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  refresh = null;
  commits = 0;
});
afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

function fixture(id: string | null, reader: (id: string) => Promise<GitRepositoryStatus>) {
  function Probe() {
    const result = useProjectGitStatus(id, reader);
    refresh = result.refresh;
    return <span>{result.status?.branch ?? "No git status"}</span>;
  }
  return <Profiler id="root-project-git" onRender={() => commits++}><Probe /></Profiler>;
}
async function show(id: string | null, reader: (id: string) => Promise<GitRepositoryStatus>) {
  await act(async () => root?.render(fixture(id, reader)));
}
async function updateStatus() {
  await act(async () => { await refresh?.(); });
}

describe("project rail git-status polling", () => {
  it("does not expose stale project A after project B is selected", async () => {
    const held = deferred<GitRepositoryStatus>();
    const read = vi.fn((id: string) =>
      id === "A" ? held.promise : Promise.resolve(status(id)),
    );
    await show("A", read);
    let oldPoll!: Promise<void>;
    await act(async () => { oldPoll = refresh!(); });
    await show("B", read);
    // The old project cannot flash under the new selected project.
    expect(host?.textContent).toBe("No git status");
    await updateStatus();
    expect(host?.textContent).toBe("B");
    await act(async () => held.resolve(status("A")));
    await oldPoll;
    expect(host?.textContent).toBe("B");
    expect(read).toHaveBeenCalledWith("A");
    expect(read).toHaveBeenCalledWith("B");
  });

  it("shares one slow IPC request and performs one trailing refresh", async () => {
    const slow = deferred<GitRepositoryStatus>();
    const reader = vi.fn()
      .mockImplementationOnce(() => slow.promise)
      .mockResolvedValue(status("A"));
    await show("A", reader);
    const original = refresh!();
    expect(reader).toHaveBeenCalledTimes(1);
    expect(refresh!()).toBe(original);
    expect(refresh!()).toBe(original);
    expect(refresh!()).toBe(original);
    expect(reader).toHaveBeenCalledTimes(1);
    await act(async () => slow.resolve(status("A")));
    await original;
    expect(reader).toHaveBeenCalledTimes(2);
    expect(host?.textContent).toBe("A");
  });

  it("keeps React status identity across identical complete repository snapshots", async () => {
    const reader = vi.fn().mockImplementation(async () => status("A"));
    await show("A", reader);
    await updateStatus();
    // Allow React to complete any one-time commit from the first loaded
    // snapshot before measuring the steady-state polling path.
    await updateStatus();
    const baseline = commits;
    for (let i = 0; i < 8; i++) await updateStatus();
    expect(commits).toBe(baseline);
    expect(reader).toHaveBeenCalledTimes(10);
  });

  it("clears stale state when selected project is removed and recovers from a read error", async () => {
    const reader = vi.fn()
      .mockResolvedValueOnce(status("A"))
      .mockRejectedValueOnce(new Error("git checkout unavailable"))
      .mockResolvedValueOnce(status("B"));
    await show("A", reader);
    await updateStatus();
    expect(host?.textContent).toBe("A");
    await show("B", reader);
    expect(host?.textContent).toBe("No git status");
    await updateStatus();
    expect(host?.textContent).toBe("No git status");
    await updateStatus();
    expect(host?.textContent).toBe("B");
    await show(null, reader);
    expect(host?.textContent).toBe("No git status");
    await updateStatus();
    expect(reader).toHaveBeenCalledTimes(3);
  });
});
