// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  aiWorkspaceAction: vi.fn(),
  getAiWorkspaceFrame: vi.fn(),
  getAiWorkspaceStatus: vi.fn(),
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
