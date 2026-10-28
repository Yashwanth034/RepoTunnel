// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

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

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function settle() {
  await act(async () => {
    await new Promise((resolve) => window.setTimeout(resolve, 0));
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  backend.listVideoProjects.mockResolvedValue([]);
  backend.getVideoProjectRecording.mockResolvedValue(null);
  backend.startVideoProjectRecording.mockResolvedValue(null);
  backend.stopVideoProjectRecording.mockResolvedValue(null);
  backend.getAiWorkspaceStatus.mockResolvedValue({
    sessionId: null,
    workspaceId: "workspace-test",
    supported: false,
    unsupportedReason: "AI Workspace is currently supported on Linux only.",
    running: false,
    ready: false,
    applicationId: null,
    applicationName: null,
    display: null,
    width: 1365,
    height: 768,
    startedAt: null,
    applications: [],
    applicationCount: 0,
    maxConcurrentApplications: 6,
    lastStartedAppSessionId: null,
    resourceAdmission: null,
    recoveryEvidenceCount: 0,
    message: "AI Workspace is currently supported on Linux only.",
  });
});

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

describe("AIWorkspacePanel platform capability", () => {
  it("keeps native AI Workspace launch unavailable without explicit Desktop permission", async () => {
    backend.getAiWorkspaceStatus.mockResolvedValue({
      sessionId: null,
      workspaceId: "workspace-test",
      supported: true,
      unsupportedReason: null,
      running: false,
      ready: false,
      applicationId: null,
      applicationName: null,
      display: null,
      width: 1440,
      height: 900,
      startedAt: null,
      applications: [],
      applicationCount: 0,
      maxConcurrentApplications: 6,
      lastStartedAppSessionId: null,
      resourceAdmission: null,
      recoveryEvidenceCount: 0,
      message: null,
    });
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Synthetic QA" } as any}
          applications={[{ id: "synthetic-local-app", name: "Synthetic QA App", executable: "synthetic-qa" } as any]}
          desktopEnabled={false}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    expect(host.textContent).toContain("AI Workspace");
    expect(host.textContent).toContain("Enable Desktop to use");
    expect(host.textContent).toContain("Desktop permission is required");
    expect(host.querySelector('select[aria-label="AI Workspace application"]')).toBeNull();
    expect([...host.querySelectorAll("button")].some(
      (button) => button.textContent?.includes("Start AI Workspace"),
    )).toBe(false);
    expect(backend.startAiWorkspace).not.toHaveBeenCalled();
    expect(backend.aiWorkspaceAction).not.toHaveBeenCalled();
  });

  it("warns about saved unreconciled project edits even when the desktop is stopped", async () => {
    backend.getAiWorkspaceStatus.mockResolvedValue({
      sessionId: null,
      workspaceId: "workspace-test",
      supported: true,
      unsupportedReason: null,
      running: false,
      ready: false,
      applicationId: null,
      applicationName: null,
      display: null,
      width: 1440,
      height: 900,
      startedAt: null,
      applications: [],
      applicationCount: 0,
      maxConcurrentApplications: 6,
      lastStartedAppSessionId: null,
      resourceAdmission: null,
      recoveryEvidenceCount: 2,
      message: null,
    });
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Test" } as any}
          applications={[]}
          desktopEnabled={true}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    const warning = host.querySelector('[role="status"]');
    expect(warning?.textContent).toContain("2 AI Workspace task recovery evidence records");
    expect(warning?.textContent).toContain("Edits are never applied automatically");
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Test" } as any}
          applications={[]}
          desktopEnabled={false}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    expect(host.textContent).toContain("2 AI Workspace task recovery evidence records");
    expect(host.textContent).toContain("Desktop permission is required");
  });

  it("shows read-only recovered file changes without invoking any replay action", async () => {
    backend.getAiWorkspaceStatus.mockResolvedValue({
      sessionId: null,
      workspaceId: "workspace-test",
      supported: true,
      unsupportedReason: null,
      running: false,
      ready: false,
      applicationId: null,
      applicationName: null,
      display: null,
      width: 1440,
      height: 900,
      startedAt: null,
      applications: [],
      applicationCount: 0,
      maxConcurrentApplications: 6,
      lastStartedAppSessionId: null,
      resourceAdmission: null,
      recoveryEvidenceCount: 1,
      message: null,
    });
    backend.listAiWorkspaceRecoveryEvidence.mockResolvedValue([
      {
        id: "overlay-synthetic.json",
        taskId: "task-123",
        appSessionId: "app-123",
        recordedAt: 1790000000000,
        reason: "Task application exited before reconciliation",
        reviewError: null,
        changes: [
          { operation: "edit", paths: ["src/main.rs"] },
          { operation: "create", paths: ["src/new.rs"] },
        ],
      },
    ]);
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Test" } as any}
          applications={[]}
          desktopEnabled={false}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    const reviewButton = [...host.querySelectorAll("button")].find(
      (button) => button.textContent?.includes("Review saved edits"),
    );
    expect(reviewButton).toBeDefined();
    await act(async () => {
      reviewButton?.click();
      await Promise.resolve();
    });
    await settle();
    expect(backend.listAiWorkspaceRecoveryEvidence).toHaveBeenCalledWith("workspace-test");
    expect(host.textContent).toContain("Task task-123");
    expect(host.textContent).toContain("edit: src/main.rs");
    expect(host.textContent).toContain("create: src/new.rs");
    expect(host.textContent).toContain("No edit will be restored or approved automatically");
    expect(backend.startAiWorkspace).not.toHaveBeenCalled();
    expect(backend.stopAiWorkspace).not.toHaveBeenCalled();
    backend.getAiWorkspaceStatus.mockResolvedValue({
      sessionId: null,
      workspaceId: "workspace-other",
      supported: true,
      unsupportedReason: null,
      running: false,
      ready: false,
      applicationId: null,
      applicationName: null,
      display: null,
      width: 1440,
      height: 900,
      startedAt: null,
      applications: [],
      applicationCount: 0,
      maxConcurrentApplications: 6,
      lastStartedAppSessionId: null,
      resourceAdmission: null,
      recoveryEvidenceCount: 0,
      message: null,
    });
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-other", name: "Other" } as any}
          applications={[]}
          desktopEnabled={false}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    expect(host.textContent).not.toContain("Task task-123");
    expect(host.textContent).not.toContain("src/main.rs");
    expect(host.textContent).not.toContain("Review saved edits");
  });

  it("shows native GUI recovery copy status without revealing paths or enabling replay", async () => {
    backend.getAiWorkspaceStatus.mockResolvedValue({
      sessionId: null,
      workspaceId: "workspace-test",
      supported: true,
      unsupportedReason: null,
      running: false,
      ready: false,
      applicationId: null,
      applicationName: null,
      display: null,
      width: 1440,
      height: 900,
      startedAt: null,
      applications: [],
      applicationCount: 0,
      maxConcurrentApplications: 6,
      lastStartedAppSessionId: null,
      resourceAdmission: null,
      recoveryEvidenceCount: 1,
      message: null,
    });
    backend.listAiWorkspaceRecoveryEvidence.mockResolvedValue([{
      id: "overlay-native-review.json",
      taskId: "task-native",
      appSessionId: "app-native",
      recordedAt: 1790000000000,
      reason: "Native task GUI closed before reconciliation",
      changes: [],
      reviewError: "Native GUI edits require manual review.",
      nativeGuiEvidenceStatus: "present-unverified",
    }]);
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Test" } as any}
          applications={[]}
          desktopEnabled={false}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    const reviewButton = [...host.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Review saved edits")
    );
    expect(reviewButton).toBeDefined();
    await act(async () => {
      reviewButton?.click();
      await Promise.resolve();
    });
    await settle();
    expect(host.textContent).toContain("preserved on disk, but its contents are unverified");
    expect(host.textContent).toContain("No files are replayed");
    expect(host.textContent).toContain("Native GUI edits require manual review.");
    expect(host.textContent).not.toContain("repotunnel-windows-project-");
    expect(host.textContent).not.toContain(".macos-projection-");
    expect(host.textContent).not.toContain("The task remains frozen");
    expect(backend.queueAiWorkspaceRecoveryForReview).not.toHaveBeenCalled();
    expect(backend.startAiWorkspace).not.toHaveBeenCalled();
  });

  it("labels experimental native sessions truthfully and never emits a double-click", async () => {
    backend.getAiWorkspaceStatus.mockResolvedValue({
      sessionId: "session-native",
      workspaceId: "workspace-test",
      supported: true,
      unsupportedReason: null,
      running: true,
      ready: true,
      applicationId: "editor-native",
      applicationName: "Native Editor",
      display: "native",
      width: 800,
      height: 600,
      startedAt: 1790000000000,
      applications: [{
        appSessionId: "app-native",
        applicationId: "editor-native",
        applicationName: "Native Editor",
        pid: 45678,
        startedAt: 1790000000000,
        primary: true,
        running: true,
      }],
      applicationCount: 1,
      maxConcurrentApplications: 6,
      lastStartedAppSessionId: "app-native",
      resourceAdmission: null,
      recoveryEvidenceCount: 0,
      message: "Experimental native application",
    });
    backend.getAiWorkspaceFrame.mockResolvedValue({
      sessionId: "session-native",
      windowId: "uia-window-abcdef012345",
      mimeType: "image/png",
      dataBase64: "AA==",
      width: 640,
      height: 480,
      sourceWidth: 640,
      sourceHeight: 480,
      activeTitle: "",
      sizeBytes: 1,
    });
    backend.aiWorkspaceAction.mockResolvedValue({});
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Test" } as any}
          applications={[]}
          desktopEnabled={true}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    await settle();
    expect(host.textContent).toContain("experimental native app session");
    expect(host.textContent).toContain("not a separate desktop");
    expect(host.textContent).not.toContain("shared isolated display");
    expect(host.textContent).not.toContain("AI control isolated from your desktop");
    const screen = host.querySelector<HTMLImageElement>(".ai-workspace-screen");
    expect(screen).not.toBeNull();
    screen!.getBoundingClientRect = () => ({
      left: 10,
      top: 20,
      width: 400,
      height: 200,
      right: 410,
      bottom: 220,
      x: 10,
      y: 20,
      toJSON: () => ({}),
    });
    await act(async () => {
      screen!.dispatchEvent(new MouseEvent("click", {
        bubbles: true, detail: 1, clientX: 210, clientY: 120,
      }));
      await Promise.resolve();
    });
    expect(backend.aiWorkspaceAction).toHaveBeenCalledWith(
      "workspace-test",
      "app-native",
      "click",
      { windowId: "uia-window-abcdef012345", xRatio: 0.5, yRatio: 0.5, clickCount: 1 },
    );
    await act(async () => {
      screen!.dispatchEvent(new MouseEvent("click", {
        bubbles: true, detail: 2, clientX: 210, clientY: 120,
      }));
      await Promise.resolve();
    });
    expect(backend.aiWorkspaceAction).toHaveBeenCalledTimes(1);
    await act(async () => {
      screen!.dispatchEvent(new MouseEvent("click", {
        bubbles: true, detail: 1, clientX: 30, clientY: 120,
      }));
      await Promise.resolve();
    });
    expect(backend.aiWorkspaceAction).toHaveBeenCalledTimes(1);
    // Bitmap: 640x480, displayed inside 400x200 object-fit:contain image.
    // The left letterbox ends at x ~= 76.67, so x=90 maps to 0.05.
    await act(async () => {
      screen!.dispatchEvent(new MouseEvent("click", {
        bubbles: true, detail: 1, clientX: 90, clientY: 120,
      }));
      await Promise.resolve();
    });
    expect(backend.aiWorkspaceAction).toHaveBeenCalledTimes(2);
    const mapped = backend.aiWorkspaceAction.mock.calls[1][3];
    expect(mapped.xRatio).toBeCloseTo(0.05, 5);
    expect(mapped.yRatio).toBeCloseTo(0.5, 5);
    expect(mapped.clickCount).toBe(1);
    expect(mapped.windowId).toBe("uia-window-abcdef012345");

    // Native typing is opt-in for exactly one click, uses only the captured
    // signed app window, and cannot accidentally become a global key event.
    const entry = host.querySelector<HTMLInputElement>(
      'input[aria-label="Native text for one editable control"]',
    );
    const arm = [...host.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Type at next click"),
    );
    expect(entry).not.toBeNull();
    expect(arm).toBeDefined();
    expect(arm!.disabled).toBe(true);
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype, "value",
      )?.set;
      setter?.call(entry, "bounded native text");
      entry!.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(arm!.disabled).toBe(false);
    await act(async () => { arm!.click(); });
    expect(arm!.getAttribute("aria-pressed")).toBe("true");
    await act(async () => {
      screen!.dispatchEvent(new MouseEvent("click", {
        bubbles: true, detail: 1, clientX: 210, clientY: 120,
      }));
      await Promise.resolve();
    });
    expect(backend.aiWorkspaceAction).toHaveBeenCalledTimes(3);
    expect(backend.aiWorkspaceAction).toHaveBeenLastCalledWith(
      "workspace-test",
      "app-native",
      "type",
      {
        windowId: "uia-window-abcdef012345",
        xRatio: 0.5,
        yRatio: 0.5,
        text: "bounded native text",
      },
    );
    expect(entry?.value).toBe("");
    expect(arm?.getAttribute("aria-pressed")).toBe("false");

    // JavaScript maxLength measures UTF-16 units, not UTF-8 bytes; a
    // 9,000-emoji string is under maxLength but over the 32 KiB native cap.
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype, "value",
      )?.set;
      setter?.call(entry, "😀".repeat(9_000));
      entry!.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(arm!.disabled).toBe(true);
    expect(backend.aiWorkspaceAction).toHaveBeenCalledTimes(3);

    // Native scroll also requires an explicitly armed screenshot click, and
    // goes through the signed semantic scrollbar, never the user's desktop.
    const scrollArm = [...host.querySelectorAll("button")].find((button) =>
      button.textContent?.includes("Scroll at next click"),
    );
    expect(scrollArm).toBeDefined();
    await act(async () => { scrollArm!.click(); });
    expect(scrollArm!.getAttribute("aria-pressed")).toBe("true");
    await act(async () => {
      screen!.dispatchEvent(new MouseEvent("click", {
        bubbles: true, detail: 1, clientX: 210, clientY: 120,
      }));
      await Promise.resolve();
    });
    expect(backend.aiWorkspaceAction).toHaveBeenCalledTimes(4);
    expect(backend.aiWorkspaceAction).toHaveBeenLastCalledWith(
      "workspace-test",
      "app-native",
      "scroll",
      {
        windowId: "uia-window-abcdef012345",
        xRatio: 0.5,
        yRatio: 0.5,
        deltaX: 0,
        deltaY: 120,
      },
    );
    expect(scrollArm!.getAttribute("aria-pressed")).toBe("false");
  });

  it("queues a quarantined edit only after explicit review confirmation without applying it", async () => {
    backend.getAiWorkspaceStatus.mockResolvedValue({
      sessionId: null,
      workspaceId: "workspace-test",
      supported: true,
      unsupportedReason: null,
      running: false,
      ready: false,
      applicationId: null,
      applicationName: null,
      display: null,
      width: 1440,
      height: 900,
      startedAt: null,
      applications: [],
      applicationCount: 0,
      maxConcurrentApplications: 6,
      lastStartedAppSessionId: null,
      resourceAdmission: null,
      recoveryEvidenceCount: 1,
      message: null,
    });
    backend.listAiWorkspaceRecoveryEvidence.mockResolvedValue([{
      id: "overlay-task123.json",
      taskId: "task-123",
      appSessionId: "app-123",
      recordedAt: 1790000000000,
      reason: "Task stopped before source reconciliation",
      reviewError: null,
      changes: [{ operation: "edit", paths: ["src/main.rs"] }],
    }]);
    backend.queueAiWorkspaceRecoveryForReview.mockResolvedValue(["change-pending-1"]);
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Test" } as any}
          applications={[]}
          desktopEnabled={false}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    await act(async () => {
      [...host!.querySelectorAll("button")].find(
        (button) => button.textContent?.includes("Review saved edits"),
      )?.click();
    });
    await settle();
    const queueButton = [...host.querySelectorAll("button")].find(
      (button) => button.textContent?.includes("Queue in Change History for approval"),
    );
    expect(queueButton).toBeDefined();
    expect(queueButton?.disabled).toBe(true);
    expect(backend.queueAiWorkspaceRecoveryForReview).not.toHaveBeenCalled();
    const confirmation = host.querySelector('input[type="checkbox"]') as HTMLInputElement;
    expect(confirmation).not.toBeNull();
    await act(async () => {
      confirmation.click();
    });
    expect(queueButton?.disabled).toBe(false);
    await act(async () => {
      queueButton?.click();
    });
    await settle();
    expect(backend.queueAiWorkspaceRecoveryForReview).toHaveBeenCalledTimes(1);
    expect(backend.queueAiWorkspaceRecoveryForReview)
      .toHaveBeenCalledWith("workspace-test", "overlay-task123.json");
    expect(host.textContent).toContain("1 Change History request queued");
    expect(host.textContent).toContain("Project files are unchanged");
    expect(host.textContent).toContain("approve or reject each request");
    expect(backend.startAiWorkspace).not.toHaveBeenCalled();
    expect(backend.stopAiWorkspace).not.toHaveBeenCalled();
  });

  it("reports a stale recovery snapshot as unsafe instead of an empty successful review", async () => {
    backend.getAiWorkspaceStatus.mockResolvedValue({
      sessionId: null,
      workspaceId: "workspace-test",
      supported: true,
      unsupportedReason: null,
      running: false,
      ready: false,
      applicationId: null,
      applicationName: null,
      display: null,
      width: 1440,
      height: 900,
      startedAt: null,
      applications: [],
      applicationCount: 0,
      maxConcurrentApplications: 6,
      lastStartedAppSessionId: null,
      resourceAdmission: null,
      recoveryEvidenceCount: 1,
      message: null,
    });
    backend.listAiWorkspaceRecoveryEvidence.mockResolvedValue([
      {
        id: "overlay-stale.json",
        taskId: "task-old",
        appSessionId: "app-old",
        recordedAt: 1790000000000,
        reason: "Crash",
        changes: [],
        reviewError: "Original source changed",
      },
    ]);
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Test" } as any}
          applications={[]}
          desktopEnabled={true}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    const reviewButton = [...host.querySelectorAll("button")].find(
      (button) => button.textContent?.includes("Review saved edits"),
    );
    await act(async () => {
      reviewButton?.click();
      await Promise.resolve();
    });
    await settle();
    expect(host.textContent).toContain("Cannot safely preview these edits");
    expect(host.textContent).toContain("Original source changed");
    expect(host.textContent).not.toContain("0 pending file changes");
  });

  it("starts and stops an owned app recording into the chosen Video Project", async () => {
    backend.getAiWorkspaceStatus.mockResolvedValue({
      sessionId: "native-session",
      workspaceId: "workspace-test",
      supported: true,
      unsupportedReason: null,
      running: true,
      ready: true,
      applicationId: "approved-editor",
      applicationName: "Editor",
      display: "native",
      width: 800,
      height: 600,
      startedAt: 1790000000000,
      applications: [{
        appSessionId: "owned-editor",
        applicationId: "approved-editor",
        applicationName: "Editor",
        pid: 4321,
        startedAt: 1790000000000,
        primary: true,
        running: true,
      }],
      applicationCount: 1,
      maxConcurrentApplications: 6,
      lastStartedAppSessionId: "owned-editor",
      resourceAdmission: null,
      recoveryEvidenceCount: 0,
      message: "Experimental native app",
    });
    backend.listVideoProjects.mockResolvedValue([
      { id: "video-project", workspaceId: "workspace-test", name: "Recording Destination" },
    ]);
    const running = {
      id: "record-1",
      workspaceId: "workspace-test",
      projectId: "video-project",
      appSessionId: "owned-editor",
      status: "recording",
      message: "Owned app-only capture",
    };
    backend.startVideoProjectRecording.mockResolvedValue(running);
    backend.stopVideoProjectRecording.mockResolvedValue({
      ...running, status: "completed", message: "Recording saved",
    });
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Test" } as any}
          applications={[]}
          desktopEnabled={true}
          onError={() => undefined}
        />,
      );
    });
    await settle();
    await settle();
    expect((host.querySelector('select[aria-label="Recording Video Project"]') as HTMLSelectElement).value)
      .toBe("video-project");
    const recordButton = [...host.querySelectorAll("button")]
      .find((button) => button.textContent?.includes("Record selected app"));
    expect(recordButton?.disabled).toBe(false);
    await act(async () => {
      recordButton?.click();
      await Promise.resolve();
    });
    await settle();
    expect(backend.startVideoProjectRecording).toHaveBeenCalledWith(
      "workspace-test", "video-project", "owned-editor",
    );
    expect(host.textContent).toContain("Recording app session owned-editor only");

    const stopButton = [...host.querySelectorAll("button")]
      .find((button) => button.textContent?.includes("Stop app recording"));
    await act(async () => {
      stopButton?.click();
      await Promise.resolve();
    });
    await settle();
    expect(backend.stopVideoProjectRecording).toHaveBeenCalledWith(
      "workspace-test", "video-project",
    );
    expect(host.textContent).toContain("completed: Recording saved");
  });

  it("shows unsupported platforms without exposing the start flow", async () => {
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);

    await act(async () => {
      root?.render(
        <AIWorkspacePanel
          workspace={{ id: "workspace-test", name: "Test" } as any}
          applications={[]}
          desktopEnabled={true}
          onError={() => undefined}
        />,
      );
    });
    await settle();

    const text = host.textContent ?? "";
    expect(text).toContain("Unavailable on this platform");
    expect(text).toContain("AI Workspace is currently supported on Linux only.");
    expect(text).not.toContain("Start AI Workspace");
    expect(backend.startAiWorkspace).not.toHaveBeenCalled();
  });
});
