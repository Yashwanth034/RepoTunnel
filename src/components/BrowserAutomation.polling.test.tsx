// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  listAutomationBrowsers: vi.fn(),
  getBrowserAutomationStatus: vi.fn(),
  listBrowserHistory: vi.fn(),
  listBrowserTabs: vi.fn(),
  getBrowserDiagnostics: vi.fn(),
  getBrowserVisualSelection: vi.fn(),
  browserTakeScreenshot: vi.fn(),
}));
vi.mock("../lib/backend", () => backend);

import BrowserAutomation from "./BrowserAutomation";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

let mount: HTMLDivElement | null = null;
let root: Root | null = null;
const workspace = {
  id: "synthetic-browser-workspace",
  name: "Synthetic Browser Test",
  path: "/isolated/synthetic-browser-workspace",
  addedAt: 1,
  accessMode: "readWrite" as const,
  changePolicy: "review" as const,
  commandPolicy: "review" as const,
};

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  backend.listAutomationBrowsers.mockResolvedValue([]);
  backend.getBrowserAutomationStatus.mockResolvedValue({
    available: true, running: true, workspaceId: workspace.id,
    browserId: "synthetic", browserName: "Synthetic", executable: null,
    pid: 123, debugPort: null, startedAt: 1, sessionId: "synthetic-session",
    activeTabId: "synthetic-tab", message: null,
  });
  backend.listBrowserHistory.mockResolvedValue([]);
  backend.listBrowserTabs.mockResolvedValue([{
    id: "synthetic-tab", title: "Synthetic Tab", url: "about:blank", active: true,
  }]);
  backend.getBrowserDiagnostics.mockResolvedValue({
    consoleEntries: [], networkFailures: [],
  });
  backend.getBrowserVisualSelection.mockResolvedValue(null);
  backend.browserTakeScreenshot.mockResolvedValue({
    id: "image-1", tabId: "synthetic-tab", createdAt: 1, mimeType: "image/png",
    dataBase64: "aGVsbG8=", sizeBytes: 5, fullPage: false,
  });
  mount = document.createElement("div");
  document.body.append(mount);
  root = createRoot(mount);
});

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  mount?.remove();
  mount = null;
  vi.useRealTimers();
});

async function renderPanel() {
  await act(async () => root?.render(
    <BrowserAutomation workspace={workspace} gatewayRunning={false} onError={() => undefined} />,
  ));
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
}

describe("Browser Controls background observation lifecycle", () => {
  it("does not overlap slow background status checks", async () => {
    const held = deferred<{
      available: boolean; running: boolean; workspaceId: string;
      browserId: string | null; browserName: string | null;
      executable: string | null; pid: number | null; debugPort: number | null;
      startedAt: number | null; sessionId: string | null;
      activeTabId: string | null; message: string | null;
    }>();
    backend.getBrowserAutomationStatus.mockResolvedValueOnce({
      available: true, running: true, workspaceId: workspace.id,
      browserId: "synthetic", browserName: "Synthetic",
      executable: null, pid: 123, debugPort: null, startedAt: 1,
      sessionId: "synthetic-session", activeTabId: "synthetic-tab", message: null,
    }).mockImplementationOnce(() => held.promise);
    await renderPanel();
    expect(backend.getBrowserAutomationStatus).toHaveBeenCalledTimes(1);
    await act(async () => { vi.advanceTimersByTime(4_500); });
    expect(backend.getBrowserAutomationStatus).toHaveBeenCalledTimes(2);
    await act(async () => { vi.advanceTimersByTime(13_500); });
    expect(backend.getBrowserAutomationStatus).toHaveBeenCalledTimes(2);
    await act(async () => held.resolve({
      available: true, running: true, workspaceId: workspace.id,
      browserId: "synthetic", browserName: "Synthetic",
      executable: null, pid: 123, debugPort: null, startedAt: 1,
      sessionId: "synthetic-session", activeTabId: "synthetic-tab", message: null,
    }));
    await act(async () => { vi.advanceTimersByTime(4_500); });
    expect(backend.getBrowserAutomationStatus).toHaveBeenCalledTimes(3);
  });

  it("ignores an old workspace status result after switching project", async () => {
    const held = deferred<{
      available: boolean; running: boolean; workspaceId: string;
      browserId: string | null; browserName: string | null;
      executable: string | null; pid: number | null; debugPort: number | null;
      startedAt: number | null; sessionId: string | null;
      activeTabId: string | null; message: string | null;
    }>();
    backend.getBrowserAutomationStatus.mockImplementation((id: string) =>
      id === workspace.id ? held.promise : Promise.resolve({
        available: true, running: true, workspaceId: id,
        browserId: "synthetic-b", browserName: "ONLY PROJECT B BROWSER",
        executable: null, pid: 333, debugPort: null, startedAt: 2,
        sessionId: "synthetic-b-session", activeTabId: "synthetic-tab", message: null,
      }));
    await act(async () => root?.render(
      <BrowserAutomation workspace={workspace} gatewayRunning={false} onError={() => undefined} />,
    ));
    const next = { ...workspace, id: "synthetic-B", name: "Synthetic B" };
    await act(async () => root?.render(
      <BrowserAutomation workspace={next} gatewayRunning={false} onError={() => undefined} />,
    ));
    expect(mount?.textContent).toContain("ONLY PROJECT B BROWSER");
    await act(async () => held.resolve({
      available: true, running: true, workspaceId: workspace.id,
      browserId: "synthetic-a", browserName: "STALE PROJECT A BROWSER",
      executable: null, pid: 999, debugPort: null, startedAt: 1,
      sessionId: "synthetic-a-session", activeTabId: "synthetic-tab", message: null,
    }));
    expect(mount?.textContent).toContain("ONLY PROJECT B BROWSER");
    expect(mount?.textContent).not.toContain("STALE PROJECT A BROWSER");
  });

  it("coalesces slow diagnostics instead of queuing overlapping 2.5s requests", async () => {
    const slow = deferred<{ consoleEntries: unknown[]; networkFailures: unknown[] }>();
    backend.getBrowserDiagnostics.mockImplementationOnce(() => slow.promise);
    await renderPanel();
    expect(backend.getBrowserDiagnostics).toHaveBeenCalledTimes(1);
    await act(async () => { vi.advanceTimersByTime(10_000); });
    expect(backend.getBrowserDiagnostics).toHaveBeenCalledTimes(1);
    await act(async () => slow.resolve({ consoleEntries: [], networkFailures: [] }));
    await act(async () => { vi.advanceTimersByTime(2_500); });
    expect(backend.getBrowserDiagnostics).toHaveBeenCalledTimes(2);
    await act(async () => root?.unmount());
    root = null;
    await act(async () => { vi.advanceTimersByTime(10_000); });
    expect(backend.getBrowserDiagnostics).toHaveBeenCalledTimes(2);
  });

  it("coalesces live preview screenshots and ignores a late frame after stopping", async () => {
    const slow = deferred<{
      id: string; tabId: string; createdAt: number; mimeType: string;
      dataBase64: string; sizeBytes: number; fullPage: boolean;
    }>();
    backend.browserTakeScreenshot.mockImplementationOnce(() => slow.promise);
    await renderPanel();
    const start = [...mount!.querySelectorAll("button")]
      .find((node) => node.textContent?.includes("Start live preview"))!;
    await act(async () => start.click());
    expect(backend.browserTakeScreenshot).toHaveBeenCalledTimes(1);
    await act(async () => { vi.advanceTimersByTime(9_000); });
    expect(backend.browserTakeScreenshot).toHaveBeenCalledTimes(1);
    const stop = [...mount!.querySelectorAll("button")]
      .find((node) => node.textContent?.includes("Stop live preview"))!;
    await act(async () => stop.click());
    await act(async () => slow.resolve({
      id: "stale-frame", tabId: "synthetic-tab", createdAt: 2,
      mimeType: "image/png", dataBase64: "c3RhbGU=", sizeBytes: 5, fullPage: false,
    }));
    expect(mount?.querySelector('img[src*="c3RhbGU="]')).toBeNull();
    await act(async () => { vi.advanceTimersByTime(3_600); });
    expect(backend.browserTakeScreenshot).toHaveBeenCalledTimes(1);
  });
});
