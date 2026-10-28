// @vitest-environment jsdom

import React, { act, Profiler, useState } from "react";
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

import VideoPanel, { mergePolledVideoJobs, sameVideoAnalysisJob } from "./VideoPanel";
import type { VideoAnalysisJob } from "../types";

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
  it("defers the expensive helper and cache probe until the Analyze section opens", async () => {
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);
    await act(async () => {
      root?.render(
        <VideoPanel workspaces={[workspace]} selectedWorkspaceId={workspace.id} onNotice={() => undefined} />,
      );
    });
    await settle();
    expect(backend.getVideoToolsStatus).not.toHaveBeenCalled();

    const analyzeTab = Array.from(host.querySelectorAll(".video-section-tabs button"))
      .find((button) => button.textContent?.includes("Analyze")) as HTMLButtonElement;
    await act(async () => analyzeTab.click());
    await settle();
    expect(backend.getVideoToolsStatus).toHaveBeenCalledTimes(1);
  });

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

const runningJob: VideoAnalysisJob = {
  id: "job-1", workspaceId: workspace.id, source: "fixture.mp4", mode: "full",
  status: "running", phase: "frame processing", progress: 0.42,
  message: "Processing", title: "Synthetic video", durationSeconds: 80,
  transcriptAvailable: false, frameCount: 12, audioChunkCount: 3,
  cacheKey: "synthetic", cacheHit: false, createdAt: 100, updatedAt: 101,
};

describe("VideoPanel background polling responsiveness", () => {
  it("preserves job identity for identical backend snapshots, including recent list state", () => {
    const copy = { ...runningJob };
    expect(sameVideoAnalysisJob(runningJob, copy)).toBe(true);
    const existing = [runningJob];
    expect(mergePolledVideoJobs(existing, copy)).toBe(existing);
  });

  it("propagates real progress, phase and status changes rather than hiding them", () => {
    const current = [runningJob];
    for (const patch of [
      { progress: 0.8 }, { phase: "audio" }, { status: "completed" as const },
      { message: "Updated" }, { updatedAt: 102 },
    ]) {
      const next = { ...runningJob, ...patch };
      expect(sameVideoAnalysisJob(runningJob, next)).toBe(false);
      const merged = mergePolledVideoJobs(current, next);
      expect(merged).not.toBe(current);
      expect(merged[0]).toBe(next);
    }
  });

  it("moves the updated job to the front and respects the ten-job cap", () => {
    const other = Array.from({ length: 10 }, (_, index) => ({
      ...runningJob, id: `job-${index + 2}`, updatedAt: index + 1,
    }));
    const next = { ...other[7], progress: 0.99 };
    const reordered = mergePolledVideoJobs(other, next);
    expect(reordered).toHaveLength(10);
    expect(reordered[0]).toBe(next);
    expect(reordered.filter((job) => job.id === next.id)).toHaveLength(1);
  });

  it("measures unchanged poll React commits for old and corrected state paths", async () => {
    async function measure(previous: boolean): Promise<number> {
      const node = document.createElement("div");
      document.body.append(node);
      const instance = createRoot(node);
      const baseline = [runningJob];
      let poll: (() => void) | undefined;
      let commits = 0;
      function PollProbe() {
        const [jobs, setJobs] = useState(baseline);
        poll = () => setJobs((current) => previous
          ? [{ ...runningJob }, ...current.filter((item) => item.id !== runningJob.id)].slice(0, 10)
          : mergePolledVideoJobs(current, { ...runningJob }));
        return <span>{jobs.length}</span>;
      }
      try {
        await act(async () => instance.render(
          <Profiler id="video-poll" onRender={() => commits++}><PollProbe /></Profiler>,
        ));
        for (let n = 0; n < 8; n++) await act(async () => poll?.());
        return commits;
      } finally {
        await act(async () => instance.unmount());
        node.remove();
      }
    }
    expect(await measure(true)).toBe(9);
    expect(await measure(false)).toBe(1);
  });
});
