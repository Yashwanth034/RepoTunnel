// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Workspace, MonitoringStatus, MonitoringSnapshot } from "../types";

const backend = vi.hoisted(() => ({
  getMonitoringStatus: vi.fn(),
  getMonitoringSnapshot: vi.fn(),
  startWorkspaceMonitoring: vi.fn(),
  stopWorkspaceMonitoring: vi.fn(),
}));
vi.mock("../lib/backend", () => backend);

import MonitoringPanel from "./MonitoringPanel";
(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
const workspace = (id: string): Workspace => ({
  id, name: "Synthetic " + id, path: "/synthetic/" + id,
  addedAt: 1, accessMode: "readWrite", changePolicy: "review", commandPolicy: "review",
});
function monitoringStatus(id: string): MonitoringStatus {
  return {
    enabled: false, running: false, workspaceId: id, workspaceName: "Only " + id,
    startedAt: null, lastScanAt: null, scannedFileCount: 0,
    fileScanTruncated: false, message: "Only " + id,
  };
}
function monitoringSnapshot(id: string): MonitoringSnapshot {
  return {
    status: monitoringStatus(id), processes: [], terminal: [], ports: [],
    fileEvents: [],
    browser: { status: {
      available: false, running: false, workspaceId: id, browserId: null,
      browserName: null, executable: null, pid: null, debugPort: null,
      startedAt: null, sessionId: null, activeTabId: null, message: null,
    }, tabs: [], consoleEntries: [], networkFailures: [] },
  };
}
let root: Root | null;
let host: HTMLDivElement | null;
beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  backend.getMonitoringStatus.mockImplementation(async (id: string) => monitoringStatus(id));
  backend.getMonitoringSnapshot.mockImplementation(async (id: string) => monitoringSnapshot(id));
  backend.startWorkspaceMonitoring.mockImplementation(async (id: string) => ({ ...monitoringStatus(id), running: true, enabled: true }));
  backend.stopWorkspaceMonitoring.mockImplementation(async (id: string) => monitoringStatus(id));
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

describe("Monitoring snapshot polling lifecycle", () => {
  it("does not start overlapping snapshot reads and cancels polling on unmount", async () => {
    const slow = deferred<MonitoringSnapshot>();
    backend.getMonitoringSnapshot.mockResolvedValueOnce(monitoringSnapshot("A"))
      .mockImplementationOnce(() => slow.promise);
    await act(async () => root?.render(
      <MonitoringPanel workspace={workspace("A")} gatewayRunning={false} onError={() => undefined} />,
    ));
    expect(backend.getMonitoringSnapshot).toHaveBeenCalledTimes(1);
    await act(async () => { vi.advanceTimersByTime(7000); });
    expect(backend.getMonitoringSnapshot).toHaveBeenCalledTimes(2);
    await act(async () => { vi.advanceTimersByTime(28000); });
    expect(backend.getMonitoringSnapshot).toHaveBeenCalledTimes(2);
    await act(async () => slow.resolve(monitoringSnapshot("A")));
    await act(async () => { vi.advanceTimersByTime(7000); });
    expect(backend.getMonitoringSnapshot).toHaveBeenCalledTimes(3);
    await act(async () => root?.unmount());
    root = null;
    await act(async () => { vi.advanceTimersByTime(14000); });
    expect(backend.getMonitoringSnapshot).toHaveBeenCalledTimes(3);
  });

  it("ignores delayed workspace-A status and snapshot after navigating to B", async () => {
    const slow = deferred<MonitoringSnapshot>();
    backend.getMonitoringSnapshot.mockImplementation((id: string) =>
      id === "A" ? slow.promise : Promise.resolve(monitoringSnapshot(id)));
    const error = vi.fn();
    await act(async () => root?.render(
      <MonitoringPanel workspace={workspace("A")} gatewayRunning={false} onError={error} />,
    ));
    await act(async () => root?.render(
      <MonitoringPanel workspace={workspace("B")} gatewayRunning={false} onError={error} />,
    ));
    expect(host?.textContent).toContain("Only B");
    await act(async () => slow.resolve(monitoringSnapshot("A")));
    expect(host?.textContent).toContain("Only B");
    expect(host?.textContent).not.toContain("Only A");
    expect(error).not.toHaveBeenCalled();
  });

  it("prioritizes explicit Start over an older in-flight snapshot and clears busy state", async () => {
    const stale = deferred<MonitoringSnapshot>();
    const start = deferred<MonitoringStatus>();
    backend.getMonitoringSnapshot.mockResolvedValueOnce(monitoringSnapshot("A"))
      .mockImplementationOnce(() => stale.promise)
      .mockResolvedValueOnce(monitoringSnapshot("A"));
    backend.startWorkspaceMonitoring.mockImplementationOnce(() => start.promise);
    await act(async () => root?.render(
      <MonitoringPanel workspace={workspace("A")} gatewayRunning={false} onError={() => undefined} />,
    ));
    await act(async () => { vi.advanceTimersByTime(7000); });
    expect(backend.getMonitoringSnapshot).toHaveBeenCalledTimes(2);
    const startButton = Array.from(host!.querySelectorAll("button"))
      .find(node => node.textContent === "Start monitoring")!;
    await act(async () => startButton.click());
    expect(host?.textContent).toContain("Updating…");
    await act(async () => { vi.advanceTimersByTime(21000); });
    expect(backend.getMonitoringSnapshot).toHaveBeenCalledTimes(2);

    await act(async () => start.resolve({
      ...monitoringStatus("A"), enabled: true, running: true,
    }));
    expect(host?.textContent).toContain("Monitoring active");
    expect(host?.textContent).not.toContain("Updating…");
    expect(backend.getMonitoringSnapshot).toHaveBeenCalledTimes(3);

    await act(async () => stale.resolve(monitoringSnapshot("A")));
    expect(host?.textContent).toContain("Monitoring active");
  });

  it("does not let an older background poll overwrite a manual refresh", async () => {
    const slow = deferred<MonitoringSnapshot>();
    backend.getMonitoringStatus
      .mockResolvedValueOnce(monitoringStatus("A"))
      .mockResolvedValueOnce(monitoringStatus("A"))
      .mockResolvedValueOnce({ ...monitoringStatus("A"), message: "Manual is current" });
    backend.getMonitoringSnapshot.mockResolvedValueOnce(monitoringSnapshot("A"))
      .mockImplementationOnce(() => slow.promise)
      .mockResolvedValueOnce({ ...monitoringSnapshot("A"), status: { ...monitoringStatus("A"), message: "Manual is current" } });
    const error = vi.fn();
    await act(async () => root?.render(
      <MonitoringPanel workspace={workspace("A")} gatewayRunning={false} onError={error} />,
    ));
    await act(async () => { vi.advanceTimersByTime(7000); });
    const refresh = Array.from(host!.querySelectorAll("button"))
      .find(button => button.textContent === "Refresh")!;
    await act(async () => refresh.click());
    expect(host?.textContent).toContain("Manual is current");
    await act(async () => slow.resolve(monitoringSnapshot("A")));
    expect(host?.textContent).toContain("Manual is current");
    expect(error).not.toHaveBeenCalled();
  });
});
