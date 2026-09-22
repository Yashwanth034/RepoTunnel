// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  cancelVideoAnalysis: vi.fn(),
  clearVideoCache: vi.fn(),
  getVideoAnalysisJob: vi.fn(),
  getVideoToolsStatus: vi.fn(),
  getVideoAnalysisResult: vi.fn(),
  installVideoTools: vi.fn(),
  listVideoAnalysisJobs: vi.fn(),
  listVideoProjects: vi.fn(),
  startVideoAnalysis: vi.fn(),
}));

vi.mock("../lib/backend", () => backend);
vi.mock("./VideoProductionPanel", () => ({
  default: () => <div data-testid="video-projects-stub">Projects</div>,
}));

import VideoPanel from "./VideoPanel";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const workspace = { id: "workspace-1a05e4cea94", name: "RepoTunnel" } as any;
const project = {
  id: "video-1",
  workspaceId: workspace.id,
  name: "Demo Project",
  currentPreview: "video-projects/demo/assets/video/main.mp4",
  pinned: false,
  updatedAt: 2,
} as any;

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function settle() {
  await act(async () => {
    await new Promise((resolve) => window.setTimeout(resolve, 0));
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  backend.listVideoProjects.mockResolvedValue([project]);
  backend.listVideoAnalysisJobs.mockResolvedValue([]);
  backend.getVideoToolsStatus.mockResolvedValue({
    ready: true,
    ytDlp: { available: true, source: "managed", version: "test" },
    ffmpeg: { available: true, source: "system", version: "test" },
    cacheBytes: 1024,
    cacheItems: 1,
    cacheLimitBytes: 1024 * 1024,
    message: "Ready",
  });
});

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

describe("VideoPanel analyze polish", () => {
  it("keeps media helpers visible while presenting unrestricted analysis focus modes", async () => {
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(<VideoPanel workspaces={[workspace]} selectedWorkspaceId={workspace.id} onNotice={() => undefined} />);
    });

    const analyzeTab = Array.from(host.querySelectorAll(".video-section-tabs button"))
      .find((button) => button.textContent?.includes("Analyze")) as HTMLButtonElement;
    await act(async () => analyzeTab.click());
    await settle();
    await settle();

    const text = host.textContent ?? "";
    expect(text).toContain("Analyze media");
    expect(text).toContain("Full");
    expect(text).toContain("Transcript focus");
    expect(text).toContain("Visual focus");
    expect(text).toContain("Tutorial focus");
    expect(text).toContain("can still use relevant audio, transcript and visual context");
    expect(text).toContain("Media helpers");
    expect(text).toContain("yt-dlp");
    expect(text).toContain("FFmpeg");
    expect(text).toContain("Clear cache");
    expect(backend.getVideoToolsStatus).toHaveBeenCalled();
  });
});
