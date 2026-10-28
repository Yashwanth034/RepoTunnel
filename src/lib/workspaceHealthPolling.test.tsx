// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { WorkspaceHealth } from "../types";
import {
  collectWorkspaceHealth, reconcileWorkspaceHealth, useWorkspaceHealthPolling,
  type HealthWorkspace,
} from "./workspaceHealthPolling";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const workspace = (id: string): HealthWorkspace => ({ id, path: "/test-only/" + id });
const healthy = (id: string): WorkspaceHealth => ({ workspaceId: id, available: true, message: null });
function deferred<T>() {
  let resolve!: (result: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

let host: HTMLDivElement | null;
let root: Root | null;

beforeEach(() => {
  vi.useFakeTimers();
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

describe("project health scanner backpressure", () => {
  it("limits a large project's backend health checks to four concurrent IPC calls", async () => {
    const projects = Array.from({ length: 32 }, (_, index) => workspace("id-" + index));
    const blocked = projects.map(({ id }) => deferred<WorkspaceHealth>());
    let inFlight = 0;
    let peak = 0;
    let calls = 0;
    const reader = vi.fn((id: string) => {
      calls++;
      inFlight++;
      peak = Math.max(peak, inFlight);
      const index = Number(id.slice(3));
      return blocked[index].promise.finally(() => { inFlight--; });
    });
    const collection = collectWorkspaceHealth(projects, reader);
    expect(calls).toBe(4);
    for (let index = 0; index < 32; index++) {
      // Each release lets at most one worker pick up another project.
      if (index === 13) blocked[index].reject(new Error("Synthetic missing folder"));
      else blocked[index].resolve(healthy(projects[index].id));
      await Promise.resolve();
      await Promise.resolve();
    }
    const snapshot = await collection;
    expect(Object.keys(snapshot)).toHaveLength(32);
    expect(reader).toHaveBeenCalledTimes(32);
    expect(peak).toBe(4);
    expect(snapshot["id-13"]).toEqual({
      workspaceId: "id-13", available: false, message: "Synthetic missing folder",
    });
    expect(snapshot["id-31"]).toEqual(healthy("id-31"));
  });

  it("preserves the same state identity on unchanged scans and detects removals and changes", () => {
    const current = { A: healthy("A"), B: healthy("B") };
    expect(reconcileWorkspaceHealth(current, { A: healthy("A"), B: healthy("B") })).toBe(current);
    expect(reconcileWorkspaceHealth(current, { A: healthy("A") })).not.toBe(current);
    expect(reconcileWorkspaceHealth(current, { A: healthy("A"), B: {
      workspaceId: "B", available: false, message: "Offline",
    } })).not.toBe(current);
    expect(reconcileWorkspaceHealth(current, { A: healthy("A"), B: healthy("B"), C: healthy("C") })).not.toBe(current);
  });

  it("coalesces slow automatic scans; ignores results after project changes or unmount", async () => {
    const held = deferred<WorkspaceHealth>();
    const reader = vi.fn((id: string) => id === "A" ? held.promise : Promise.resolve(healthy(id)));
    const render = async (projects: HealthWorkspace[]) => {
      await act(async () => root?.render(<Fixture projects={projects} read={reader} />));
    };
    await render([workspace("A")]);
    expect(reader).toHaveBeenCalledTimes(1);
    await act(async () => { vi.advanceTimersByTime(120_000); });
    expect(reader).toHaveBeenCalledTimes(1);
    await render([workspace("B")]);
    expect(reader).toHaveBeenCalledTimes(2);
    expect(host?.textContent).toContain("B");
    await act(async () => held.resolve({ workspaceId: "A", available: false, message: "stale" }));
    expect(host?.textContent).toContain("B");
    expect(host?.textContent).not.toContain("stale");
    await act(async () => root?.unmount());
    root = null;
    await act(async () => { vi.advanceTimersByTime(60_000); });
    expect(reader).toHaveBeenCalledTimes(2);
  });

  it("updates an explicitly retried project without allowing an old scan to undo it", async () => {
    const held = deferred<WorkspaceHealth>();
    const reader = vi.fn(() => held.promise);
    let retry: ((id: string, health: WorkspaceHealth) => void) | null = null;
    function RetryFixture() {
      const result = useWorkspaceHealthPolling([workspace("A")], reader);
      retry = result.update;
      return <span>{result.health.A?.message ?? "pending"}</span>;
    }
    await act(async () => root?.render(<RetryFixture />));
    await act(async () => retry?.("A", { workspaceId: "A", available: true, message: "Recovered" }));
    expect(host?.textContent).toContain("Recovered");
    await act(async () => held.resolve({ workspaceId: "A", available: false, message: "stale" }));
    expect(host?.textContent).toContain("Recovered");
  });
});

function Fixture({ projects, read }: {
  projects: HealthWorkspace[];
  read: (id: string) => Promise<WorkspaceHealth>;
}) {
  const { health } = useWorkspaceHealthPolling(projects, read);
  return <div>{Object.entries(health).map(([id, value]) => <span key={id}>{id}:{value.message ?? "healthy"}</span>)}</div>;
}
