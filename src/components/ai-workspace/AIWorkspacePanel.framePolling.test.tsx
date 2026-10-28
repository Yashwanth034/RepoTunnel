// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AiWorkspaceFrame, AiWorkspaceStatus, Workspace } from "../../types";

const backend = vi.hoisted(() => ({
  aiWorkspaceAction: vi.fn(),
  getAiWorkspaceFrame: vi.fn(),
  getAiWorkspaceStatus: vi.fn(),
  getVideoProjectRecording: vi.fn(),
  listVideoProjects: vi.fn(),
  startVideoProjectRecording: vi.fn(),
  stopVideoProjectRecording: vi.fn(),
  listAiWorkspaceRecoveryEvidence: vi.fn(),
  queueAiWorkspaceRecoveryForReview: vi.fn(),
  startAiWorkspace: vi.fn(),
  stopAiWorkspace: vi.fn(),
}));
vi.mock("../../lib/backend", () => backend);
import AIWorkspacePanel from "./AIWorkspacePanel";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const project = (id: string) => ({
  id, name: "Only " + id, path: "/test-only/" + id, addedAt: 1,
  accessMode: "readWrite", changePolicy: "review", commandPolicy: "review",
}) as Workspace;
const app = (id: string) => ({
  appSessionId: id, applicationId: id,
  applicationName: "Synthetic " + id, pid: 123,
  startedAt: 123, primary: id.endsWith("A"), running: true,
});
function status(id: string, session = "desktop-" + id, apps = ["app-A", "app-B"]): AiWorkspaceStatus {
  return {
    sessionId: session, workspaceId: id, supported: true, unsupportedReason: null,
    running: true, ready: true, applicationId: apps[0], applicationName: "Synthetic",
    display: "native", width: 800, height: 600, startedAt: 123,
    applications: apps.map(app), applicationCount: apps.length,
    maxConcurrentApplications: 6, lastStartedAppSessionId: apps[0] ?? null,
    resourceAdmission: null, recoveryEvidenceCount: 0, message: null,
  };
}
const frame = (sessionId: string, windowId: string, base64: string): AiWorkspaceFrame => ({
  sessionId, windowId, mimeType: "image/png", width: 800, height: 600,
  sourceWidth: 800, sourceHeight: 600, sizeBytes: 1, activeTitle: windowId,
  dataBase64: base64,
});
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>(resolvePromise => { resolve = resolvePromise; });
  return { promise, resolve };
}
let root: Root | null;
let host: HTMLDivElement | null;
const error = vi.fn();
async function renderProject(id: string) {
  await act(async () => root?.render(
    <AIWorkspacePanel workspace={project(id)} applications={[]}
      desktopEnabled={true} onError={error} />,
  ));
}
async function settle() {
  await act(async () => { await Promise.resolve(); await Promise.resolve(); });
}
const screenshot = () => host?.querySelector<HTMLImageElement>("img.ai-workspace-screen");

beforeEach(() => {
  vi.clearAllMocks();
  backend.listVideoProjects.mockResolvedValue([]);
  backend.getVideoProjectRecording.mockResolvedValue(null);
  backend.getAiWorkspaceStatus.mockImplementation(async (id: string) => status(id));
  backend.getAiWorkspaceFrame.mockImplementation(async (id: string) =>
    frame("desktop-" + id, "window-" + id, "AQ=="));
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});
afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

describe("AI Workspace selected-frame and status isolation", () => {
  it("never publishes the old app's screenshot after selecting a different app", async () => {
    const old = deferred<AiWorkspaceFrame>();
    backend.getAiWorkspaceFrame.mockImplementation((_id: string, appId: string) =>
      appId === "app-A" ? old.promise : Promise.resolve(frame("desktop-A", "window-B", "Ag==")));
    await renderProject("A");
    await settle();
    expect(backend.getAiWorkspaceFrame).toHaveBeenCalledWith("A", "app-A", 1440);
    const select = host!.querySelector<HTMLSelectElement>('[aria-label="AI Workspace active application"]')!;
    await act(async () => {
      select.value = "app-B";
      select.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await settle();
    expect(backend.getAiWorkspaceFrame).toHaveBeenCalledWith("A", "app-B", 1440);
    expect(screenshot()?.src).toContain("Ag==");
    await act(async () => old.resolve(frame("desktop-A", "OLD-APP-WINDOW", "Aw==")));
    expect(screenshot()?.src).toContain("Ag==");
    expect(screenshot()?.src).not.toContain("Aw==");
    expect(host?.textContent).not.toContain("OLD-APP-WINDOW");
    expect(backend.aiWorkspaceAction).not.toHaveBeenCalled();
  });

  it("rejects a screenshot signed for an earlier desktop session even when app ID is the same", async () => {
    backend.getAiWorkspaceFrame.mockResolvedValue(
      frame("outdated-desktop-session", "window-invalid", "Aw=="));
    await renderProject("A");
    await settle();
    expect(screenshot()).toBeNull();
    expect(host?.textContent).toContain("Loading selected application");
    expect(backend.aiWorkspaceAction).not.toHaveBeenCalled();
  });

  it("does not replay an obsolete frame after switching A → B → A with reused identifiers", async () => {
    const olderA = deferred<AiWorkspaceFrame>();
    const newerA = deferred<AiWorkspaceFrame>();
    let readsA = 0;
    backend.getAiWorkspaceFrame.mockImplementation((_id: string, appId: string) => {
      if (appId === "app-B") return Promise.resolve(frame("desktop-A", "window-B", "Ag=="));
      return ++readsA === 1 ? olderA.promise : newerA.promise;
    });
    await renderProject("A");
    await settle();
    const select = host!.querySelector<HTMLSelectElement>('[aria-label="AI Workspace active application"]')!;
    await act(async () => { select.value = "app-B"; select.dispatchEvent(new Event("change", { bubbles: true })); });
    await settle();
    expect(screenshot()?.src).toContain("Ag==");
    await act(async () => { select.value = "app-A"; select.dispatchEvent(new Event("change", { bubbles: true })); });
    await settle();
    expect(screenshot()).toBeNull();
    await act(async () => newerA.resolve(frame("desktop-A", "CURRENT-A-window", "Bw==")));
    expect(screenshot()?.src).toContain("Bw==");
    await act(async () => olderA.resolve(frame("desktop-A", "STALE-A-window", "Aw==")));
    expect(screenshot()?.src).toContain("Bw==");
    expect(host?.textContent).not.toContain("STALE-A-window");
  });

  it("hides the frame while the app is not ready and requires a new capture after recovery", async () => {
    vi.useFakeTimers();
    try {
      let ready = true;
      backend.getAiWorkspaceStatus.mockImplementation(async () => ({
        ...status("A"), ready,
      }));
      const refreshFrame = deferred<AiWorkspaceFrame>();
      backend.getAiWorkspaceFrame.mockResolvedValue(frame("desktop-A", "first-window", "AQ=="));
      await renderProject("A");
      await settle();
      expect(screenshot()?.src).toContain("AQ==");
      ready = false;
      await act(async () => { await vi.advanceTimersByTimeAsync(2500); });
      expect(screenshot()).toBeNull();
      // The ordinary 700-ms timer is allowed to fetch old ready-state
      // frames before status changes. Make the *post-recovery* capture
      // definitely pending instead of counting on a mock-call index.
      backend.getAiWorkspaceFrame.mockReset().mockImplementation(() => refreshFrame.promise);
      ready = true;
      await act(async () => { await vi.advanceTimersByTimeAsync(2500); });
      expect(screenshot()).toBeNull();
      await act(async () => refreshFrame.resolve(frame("desktop-A", "recovered-window", "BA==")));
      expect(screenshot()?.src).toContain("BA==");
      expect(backend.aiWorkspaceAction).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not show stale status or an old project's controls after switching workspace", async () => {
    const heldStatus = deferred<AiWorkspaceStatus>();
    backend.getAiWorkspaceStatus.mockImplementation((id: string) =>
      id === "A" ? heldStatus.promise
        : Promise.resolve(status("B", "desktop-B", ["app-B"])));
    backend.getAiWorkspaceFrame.mockResolvedValue(frame("desktop-B", "window-B", "BA=="));
    await renderProject("A");
    expect(backend.getAiWorkspaceStatus).toHaveBeenCalledWith("A");
    await renderProject("B");
    await settle();
    expect(host?.textContent).toContain("Session app-B");
    expect(screenshot()?.src).toContain("BA==");
    await act(async () => heldStatus.resolve(status("A")));
    expect(host?.textContent).toContain("Session app-B");
    expect(host?.textContent).not.toContain("Session app-A");
    expect(screenshot()?.src).toContain("BA==");
    expect(error).not.toHaveBeenCalled();
  });

  it("does not publish frames for a previous project when the same session IDs were reused", async () => {
    const old = deferred<AiWorkspaceFrame>();
    backend.getAiWorkspaceStatus.mockImplementation(async (id: string) =>
      status(id, "reused-session", ["reused-app"]));
    backend.getAiWorkspaceFrame.mockImplementation((id: string) =>
      id === "A" ? old.promise :
        Promise.resolve(frame("reused-session", "current-B-window", "BQ==")));
    await renderProject("A");
    await settle();
    await renderProject("B");
    await settle();
    expect(screenshot()?.src).toContain("BQ==");
    await act(async () => old.resolve(frame("reused-session", "STALE-A-window", "Bg==")));
    expect(screenshot()?.src).toContain("BQ==");
    expect(screenshot()?.src).not.toContain("Bg==");
    expect(host?.textContent).not.toContain("STALE-A-window");
  });
});
