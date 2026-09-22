// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import tauriConfig from "../../src-tauri/tauri.conf.json";

const backend = vi.hoisted(() => ({
  createVideoProject: vi.fn(),
  deleteVideoProject: vi.fn(),
  importVideoProjectFolder: vi.fn(),
  listVideoProjectFiles: vi.fn(),
  listVideoProjects: vi.fn(),
  prepareVideoProjectFilePreview: vi.fn(),
  prepareVideoProjectPreview: vi.fn(),
  readVideoProjectTextFile: vi.fn(),
  renderVideoProjectTimeline: vi.fn(),
  setVideoProjectPinned: vi.fn(),
}));

const dialog = vi.hoisted(() => ({ open: vi.fn() }));

vi.mock("../lib/backend", () => backend);
vi.mock("@tauri-apps/plugin-dialog", () => dialog);
vi.mock("@tauri-apps/api/core", () => ({
  convertFileSrc: (path: string) => `asset://${path}`,
}));

import VideoProductionPanel from "./VideoProductionPanel";


(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const workspace = {
  id: "workspace-1a05e4cea94",
  name: "RepoTunnel",
  path: "project-root",
  addedAt: 1,
  accessMode: "readWrite",
  changePolicy: "automatic",
  commandPolicy: "automatic",
} as any;

const project = {
  schemaVersion: 1,
  id: "video-1",
  workspaceId: workspace.id,
  name: "Demo Project",
  slug: "demo-project",
  relativePath: "video-projects/demo-project",
  status: "planning",
  pinned: false,
  aspectRatio: "16:9",
  width: 1920,
  height: 1080,
  fps: 30,
  createdAt: 1,
  updatedAt: 2,
  scriptPath: "video-projects/demo-project/script/script.md",
  storyboardPath: "video-projects/demo-project/storyboard/storyboard.json",
  timelinePath: "video-projects/demo-project/timeline/timeline.json",
  currentPreview: "video-projects/demo-project/assets/video/main.mov",
  latestDraft: null,
  finalExport: null,
  currentSubtitle: null,
  assets: [],
  checkpoints: [],
  attentionRequired: false,
  lastError: null,
} as const;

const files = [
  { relativePath: "video-projects/demo-project/assets/video/main.mov", name: "main.mov", kind: "video", sizeBytes: 1024, modifiedAt: 2 },
  { relativePath: "video-projects/demo-project/assets/audio/voice.mp3", name: "voice.mp3", kind: "audio", sizeBytes: 2048, modifiedAt: 2 },
  { relativePath: "video-projects/demo-project/assets/images/poster.png", name: "poster.png", kind: "image", sizeBytes: 3072, modifiedAt: 2 },
  { relativePath: "video-projects/demo-project/script/script.md", name: "script.md", kind: "text", sizeBytes: 30, modifiedAt: 2 },
  { relativePath: "video-projects/demo-project/storyboard/storyboard.json", name: "storyboard.json", kind: "text", sizeBytes: 30, modifiedAt: 2 },
  { relativePath: "video-projects/demo-project/timeline/timeline.json", name: "timeline.json", kind: "text", sizeBytes: 30, modifiedAt: 2 },
] as const;

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function settle() {
  await act(async () => {
    await new Promise((resolve) => window.setTimeout(resolve, 0));
  });
}

function folderButton(container: HTMLElement, name: string): HTMLButtonElement {
  const button = Array.from(container.querySelectorAll(".video-file-tree-row.folder"))
    .find((element) => element.textContent?.trim().endsWith(name)) as HTMLButtonElement | undefined;
  if (!button) throw new Error(`Folder ${name} was not rendered`);
  return button;
}

async function expandFolder(container: HTMLElement, name: string) {
  const button = folderButton(container, name);
  if (!button.querySelector(".video-file-tree-chevron.expanded")) {
    await act(async () => button.click());
  }
}

async function renderPanel() {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => {
    root?.render(
      <VideoProductionPanel workspaces={[workspace]} selectedWorkspaceId={workspace.id} onNotice={() => undefined} />,
    );
  });
  await settle();
  await settle();
  return host;
}

beforeEach(() => {
  vi.clearAllMocks();
  backend.listVideoProjects.mockResolvedValue([project]);
  backend.listVideoProjectFiles.mockResolvedValue(files);
  backend.prepareVideoProjectPreview.mockResolvedValue({
    projectId: project.id,
    videoPath: "preview-cache/preview.mp4",
    playbackUrl: "http://127.0.0.1:43123/media/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    subtitlePath: null,
    subtitleUrl: null,
    subtitleLanguage: null,
    mimeType: "video/mp4",
    sizeBytes: 4,
    createdAt: 10,
  });
  backend.prepareVideoProjectFilePreview.mockResolvedValue({
    projectId: project.id,
    videoPath: "preview-cache/file-preview.mp4",
    playbackUrl: "http://127.0.0.1:43123/media/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    subtitlePath: null,
    subtitleUrl: null,
    subtitleLanguage: null,
    mimeType: "video/mp4",
    sizeBytes: 4,
    createdAt: 11,
  });
  backend.setVideoProjectPinned.mockResolvedValue({ ...project, pinned: true });
  backend.deleteVideoProject.mockResolvedValue(undefined);
  backend.renderVideoProjectTimeline.mockResolvedValue({});
  Object.defineProperty(HTMLMediaElement.prototype, "play", { configurable: true, value: vi.fn().mockResolvedValue(undefined) });
  Object.defineProperty(HTMLMediaElement.prototype, "pause", { configurable: true, value: vi.fn() });
});

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

describe("VideoProductionPanel polish", () => {
  it("allows the private loopback HTTP media transport used by the desktop player", () => {
    expect(tauriConfig.app.security.csp["media-src"]).toContain("http://127.0.0.1:*");
    expect(tauriConfig.app.security.devCsp["media-src"]).toContain("http://127.0.0.1:*");
  });

  it("falls back to the first project video when the manifest has no current preview", async () => {
    backend.listVideoProjects.mockResolvedValue([{ ...project, currentPreview: null, latestDraft: null, finalExport: null }]);
    const container = await renderPanel();
    expect(backend.prepareVideoProjectPreview).not.toHaveBeenCalled();
    expect(backend.prepareVideoProjectFilePreview).toHaveBeenCalledWith(
      workspace.id,
      project.id,
      "video-projects/demo-project/assets/video/main.mov",
    );
    const player = container.querySelector("video");
    expect(player).not.toBeNull();
    expect(player?.autoplay).toBe(true);
  });

  it("loads the project preview automatically and exposes normal player controls", async () => {
    const container = await renderPanel();
    expect(backend.prepareVideoProjectPreview).toHaveBeenCalledWith(workspace.id, project.id);
    const player = container.querySelector("video");
    expect(player).not.toBeNull();
    expect(player?.controls).toBe(true);
    expect(player?.autoplay).toBe(true);
    expect(player?.getAttribute("src")).toBe(
      "http://127.0.0.1:43123/media/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
  });

  it("shows project-owned files without old document tabs or summary cards", async () => {
    const container = await renderPanel();
    const text = container.textContent ?? "";
    expect(text).toContain("main.mov");
    await expandFolder(container, "audio");
    await expandFolder(container, "images");
    await expandFolder(container, "video");
    const expandedText = container.textContent ?? "";
    expect(expandedText).toContain("voice.mp3");
    expect(expandedText).toContain("poster.png");
    expect(expandedText).toContain("main.mov");
    const sidebarTree = container.querySelector(".video-project-sidebar-tree");
    expect(sidebarTree).not.toBeNull();
    expect(sidebarTree?.closest(".video-production-rail")).not.toBeNull();
    expect(container.querySelector(".video-production-main .video-project-sidebar-tree")).toBeNull();
    expect(container.querySelector(".video-production-document-tabs")).toBeNull();
    expect(container.querySelector(".video-production-summary")).toBeNull();
    expect(text).not.toContain("Checkpoints");
    expect(text).not.toContain("Final export");
    expect(container.querySelector(".video-production-list")).not.toBeNull();
  });

  it("collapses and expands project files from the Video Project row", async () => {
    const container = await renderPanel();
    const collapse = container.querySelector('button[aria-label="Collapse files for Demo Project"]') as HTMLButtonElement;
    expect(collapse).not.toBeNull();
    expect(container.querySelector(".video-project-sidebar-tree")).not.toBeNull();

    await act(async () => collapse.click());
    expect(container.querySelector(".video-project-sidebar-tree")).toBeNull();

    const expand = container.querySelector('button[aria-label="Expand files for Demo Project"]') as HTMLButtonElement;
    expect(expand).not.toBeNull();
    await act(async () => expand.click());
    expect(container.querySelector(".video-project-sidebar-tree")).not.toBeNull();
  });

  it("uses neutral creation ratios with no platform-specific wording", async () => {
    const container = await renderPanel();
    const ratio = container.querySelector('select[aria-label="Video aspect ratio"]') as HTMLSelectElement;
    expect(Array.from(ratio.options).map((option) => option.textContent)).toEqual(["16:9", "9:16", "1:1", "4:5"]);
    expect(container.textContent).not.toContain("YouTube");
    expect(container.textContent).not.toContain("Shorts");
  });

  it("uses the RepoTunnel in-app delete confirmation and cancels without touching the project", async () => {
    const container = await renderPanel();
    const remove = container.querySelector('button[aria-label="Delete Demo Project"]') as HTMLButtonElement;
    await act(async () => remove.click());

    const modal = container.querySelector('[role="dialog"][aria-labelledby="video-delete-title"]');
    expect(modal).not.toBeNull();
    expect(modal?.textContent).toContain('Delete "Demo Project"');
    expect(backend.deleteVideoProject).not.toHaveBeenCalled();

    const cancel = Array.from(modal?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.trim() === "Cancel") as HTMLButtonElement;
    await act(async () => cancel.click());

    expect(container.querySelector(".video-confirm-modal")).toBeNull();
    expect(backend.deleteVideoProject).not.toHaveBeenCalled();
  });

  it("pins from the project row and deletes only after confirmation", async () => {
    const container = await renderPanel();
    const pin = container.querySelector('button[aria-label="Pin Demo Project"]') as HTMLButtonElement;
    expect(pin).not.toBeNull();
    await act(async () => pin.click());
    expect(backend.setVideoProjectPinned).toHaveBeenCalledWith(workspace.id, project.id, true);

    const remove = container.querySelector('button[aria-label="Delete Demo Project"]') as HTMLButtonElement;
    expect(remove).not.toBeNull();
    await act(async () => remove.click());

    const modal = container.querySelector(".video-confirm-modal");
    expect(modal).not.toBeNull();
    const confirmDelete = Array.from(modal?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.trim() === "Delete") as HTMLButtonElement;
    await act(async () => confirmDelete.click());
    await settle();

    expect(backend.deleteVideoProject).toHaveBeenCalledWith(workspace.id, project.id);
  });

  it("splits at the playhead, supports undo/redo, and renders muted source audio", async () => {
    const container = await renderPanel();
    await expandFolder(container, "video");
    const fileButton = Array.from(container.querySelectorAll(".video-file-tree-row.file"))
      .find((element) => element.textContent?.includes("main.mov")) as HTMLButtonElement;
    await act(async () => fileButton.click());
    await settle();

    const player = container.querySelector("video") as HTMLVideoElement;
    player.currentTime = 5;
    const split = Array.from(container.querySelectorAll("button"))
      .find((button) => button.textContent?.trim() === "Split at playhead") as HTMLButtonElement;
    await act(async () => split.click());
    expect(container.textContent).toContain("Clip 2");

    const undo = Array.from(container.querySelectorAll("button"))
      .find((button) => button.textContent?.trim() === "Undo") as HTMLButtonElement;
    await act(async () => undo.click());
    expect(container.textContent).not.toContain("Clip 2");

    const redo = Array.from(container.querySelectorAll("button"))
      .find((button) => button.textContent?.trim() === "Redo") as HTMLButtonElement;
    await act(async () => redo.click());
    expect(container.textContent).toContain("Clip 2");

    const audioSelect = Array.from(container.querySelectorAll(".video-audio-controls select"))[0] as HTMLSelectElement;
    await act(async () => {
      audioSelect.value = "mute";
      audioSelect.dispatchEvent(new Event("change", { bubbles: true }));
    });

    const render = Array.from(container.querySelectorAll("button"))
      .find((button) => button.textContent?.trim() === "Render draft") as HTMLButtonElement;
    await act(async () => render.click());
    expect(backend.renderVideoProjectTimeline).toHaveBeenCalledWith(
      workspace.id,
      project.id,
      expect.objectContaining({
        preserveSourceAudio: false,
        narrationPath: null,
        finalRender: false,
      }),
    );
    const request = backend.renderVideoProjectTimeline.mock.calls[0][2];
    expect(request.clips).toHaveLength(2);
    expect(request.clips[0].endSeconds).toBe(5);
    expect(request.clips[1].startSeconds).toBe(5);
  });

  it("opens a video file from the tree and initializes manual editing", async () => {
    const container = await renderPanel();
    await expandFolder(container, "video");
    const fileButton = Array.from(container.querySelectorAll(".video-file-tree-row.file"))
      .find((element) => element.textContent?.includes("main.mov")) as HTMLButtonElement;
    expect(fileButton).not.toBeNull();
    await act(async () => fileButton.click());
    await settle();
    expect(backend.prepareVideoProjectFilePreview).toHaveBeenCalledWith(
      workspace.id,
      project.id,
      "video-projects/demo-project/assets/video/main.mov",
    );
    expect(container.textContent).toContain("Clip 1");
    expect(container.textContent).toContain("Split at playhead");
    expect(container.textContent).toContain("Keep source audio");
  });
});
