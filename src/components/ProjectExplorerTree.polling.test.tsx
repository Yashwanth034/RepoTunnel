// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DirectoryEntry, Workspace } from "../types";

const filesystem = vi.hoisted(() => ({
  listDirectory: vi.fn(),
  listDirectoryPage: vi.fn(),
  getFileInfo: vi.fn(),
  createEditorDirectory: vi.fn(),
  createEditorFile: vi.fn(),
  deleteEditorEntry: vi.fn(),
  openWorkspacePathLocal: vi.fn(),
  renameEditorEntry: vi.fn(),
}));
vi.mock("../lib/filesystem", () => filesystem);
vi.mock("./AppSidebar", () => ({ NavIcon: () => null }));
vi.mock("./ConfirmationDialog", () => ({
  default: ({ title, onConfirm, onCancel }: {
    title: string; onConfirm: () => void; onCancel: () => void;
  }) => React.createElement("div", { role: "dialog", "aria-label": title },
    React.createElement("button", { onClick: onConfirm }, "Confirm deletion"),
    React.createElement("button", { onClick: onCancel }, "Cancel deletion")),
}));

import ProjectExplorerTree from "./ProjectExplorerTree";
(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const folder = (name: string, path = name): DirectoryEntry => ({
  name, path, kind: "directory", size: null, modifiedAt: null,
});
const file = (name: string, path = name): DirectoryEntry => ({
  name, path, kind: "file", size: 4, modifiedAt: 1,
});
const workspace = (id: string): Workspace => ({
  id, name: "Synthetic " + id, path: "/isolated/" + id, addedAt: 1,
  accessMode: "readWrite", changePolicy: "review", commandPolicy: "review",
});
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}
const onNotice = vi.fn();
const onOpenFile = vi.fn();
const onEntryRemoved = vi.fn();
const onEntryRenamed = vi.fn();
let root: Root | null;
let host: HTMLDivElement | null;

beforeEach(() => {
  vi.clearAllMocks();
  filesystem.listDirectory.mockImplementation(async (_id: string, path: string) =>
    path ? [] : [folder("alpha"), folder("beta")]);
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

async function show(id = "A", refreshToken = 1, activePath: string | null = null) {
  await act(async () => root?.render(
    <ProjectExplorerTree workspace={workspace(id)} activePath={activePath}
      refreshToken={refreshToken} gitChanges={[]} onOpenFile={onOpenFile}
      onEntryRemoved={onEntryRemoved} onEntryRenamed={onEntryRenamed}
      onNotice={onNotice} />,
  ));
}
async function expand(name: string) {
  const entry = [...host!.querySelectorAll<HTMLButtonElement>("button.explorer-entry-main")]
    .find(button => button.textContent?.includes(name));
  expect(entry).toBeDefined();
  await act(async () => entry?.click());
}
const counts = () => filesystem.listDirectory.mock.calls.map((call: string[]) => call[1]);

describe("Explorer directory reading and refresh behavior", () => {
  it("opens only newly expanded folders instead of resending all expanded directory reads", async () => {
    await show();
    const baseline = counts().filter(path => path === "").length;
    await expand("alpha");
    expect(counts().filter(path => path === "alpha")).toHaveLength(1);
    expect(counts().filter(path => path === "")).toHaveLength(baseline);
    await expand("beta");
    expect(counts().filter(path => path === "beta")).toHaveLength(1);
    expect(counts().filter(path => path === "alpha")).toHaveLength(1);
    expect(counts().filter(path => path === "")).toHaveLength(baseline);
    await show("A", 2);
    expect(counts().filter(path => path === "alpha")).toHaveLength(2);
    expect(counts().filter(path => path === "beta")).toHaveLength(2);
    expect(counts().filter(path => path === "")).toHaveLength(baseline + 1);
  });

  it("ignores a delayed directory listing from an older selected project", async () => {
    const held = deferred<DirectoryEntry[]>();
    filesystem.listDirectory.mockImplementation((id: string, path: string) => {
      if (id === "A" && path === "") return held.promise;
      return Promise.resolve(path ? [] : [file("new-project-B.txt")]);
    });
    await show("A", 0);
    await show("B", 0);
    expect(host?.textContent).toContain("new-project-B.txt");
    await act(async () => held.resolve([file("STALE PROJECT A.txt")]));
    expect(host?.textContent).toContain("new-project-B.txt");
    expect(host?.textContent).not.toContain("STALE PROJECT A.txt");
    expect(onNotice).not.toHaveBeenCalled();
  });

  it("keeps a later refresh result when an earlier read finishes after it", async () => {
    const held = deferred<DirectoryEntry[]>();
    filesystem.listDirectory.mockImplementationOnce(() => held.promise)
      .mockResolvedValue([file("newest.ts")]);
    await show("A", 0);
    await show("A", 1);
    expect(host?.textContent).toContain("newest.ts");
    await act(async () => held.resolve([file("stale.ts")]));
    expect(host?.textContent).toContain("newest.ts");
    expect(host?.textContent).not.toContain("stale.ts");
  });

  it("bounds a deep editor-path auto-expansion to four directory reads at once", async () => {
    const pending: Array<ReturnType<typeof deferred<DirectoryEntry[]>>> = [];
    let inFlight = 0;
    let peak = 0;
    filesystem.listDirectory.mockImplementation((_id: string, path: string) => {
      if (!path) return Promise.resolve([folder("a")]);
      const wait = deferred<DirectoryEntry[]>();
      pending.push(wait);
      peak = Math.max(peak, ++inFlight);
      return wait.promise.finally(() => { inFlight--; });
    });
    const path = "a/b/c/d/e/f/g/h/source.ts";
    await show("A", 0, path);
    expect(peak).toBe(4);
    expect(pending).toHaveLength(4);
    for (let index = 0; index < 8; index++) {
      pending[index].resolve([]);
      await act(async () => { await Promise.resolve(); await Promise.resolve(); });
    }
    expect(pending).toHaveLength(8);
    expect(peak).toBe(4);
  });

  it("falls back to scoped Rust directory pages when the legacy 1000-item limit is reached", async () => {
    const first = Array.from({ length: 200 }, (_, i) => file(`file-${String(i).padStart(4, "0")}.ts`));
    const second = Array.from({ length: 200 }, (_, i) => file(`file-${String(i + 200).padStart(4, "0")}.ts`));
    filesystem.listDirectory.mockRejectedValueOnce(new Error(
      "This folder contains more than 1000 accessible entries. Narrow the request to a subfolder.",
    ));
    filesystem.listDirectoryPage.mockResolvedValueOnce({
      entries: first, nextCursor: "private-cursor-A", done: false, busy: false,
    }).mockResolvedValueOnce({
      entries: second, nextCursor: null, done: true, busy: false,
    });
    await show("A", 0);
    expect(filesystem.listDirectoryPage).toHaveBeenCalledWith("A", "", null, 200);
    expect(host!.querySelectorAll("button.explorer-entry-main")).toHaveLength(200);
    expect(host!.textContent).toContain("Show 200 more");
    const more = host!.querySelector<HTMLButtonElement>(".explorer-load-more");
    await act(async () => more?.click());
    expect(filesystem.listDirectoryPage).toHaveBeenLastCalledWith("A", "", "private-cursor-A", 200);
    expect(host!.querySelectorAll("button.explorer-entry-main")).toHaveLength(400);
    expect(host!.textContent).toContain("file-0399.ts");
    expect(host!.querySelector(".explorer-load-more")).toBeNull();
  });

  it("rejects an older project page result after switching to another project", async () => {
    const held = deferred<{
      entries: DirectoryEntry[]; nextCursor: string | null; done: boolean; busy: boolean;
    }>();
    filesystem.listDirectory.mockImplementation(async (id: string) => {
      if (id === "A") throw new Error(
        "This folder contains more than 1000 accessible entries. Narrow the request to a subfolder.",
      );
      return [file("project-B.txt")];
    });
    filesystem.listDirectoryPage.mockResolvedValueOnce({
      entries: [file("file-0000.ts")], nextCursor: "cursor-A", done: false, busy: false,
    }).mockImplementationOnce(() => held.promise);
    await show("A", 0);
    await act(async () => host!.querySelector<HTMLButtonElement>(".explorer-load-more")?.click());
    await show("B", 0);
    await act(async () => held.resolve({
      entries: [file("SHOULD_NOT_APPEAR.txt")], nextCursor: null, done: true, busy: false,
    }));
    expect(host!.textContent).toContain("project-B.txt");
    expect(host!.textContent).not.toContain("SHOULD_NOT_APPEAR.txt");
    expect(host!.querySelectorAll("button.explorer-entry-main")).toHaveLength(1);
  });

  it("keeps the same cursor available when the bounded reader is busy", async () => {
    filesystem.listDirectory.mockRejectedValueOnce(new Error(
      "This folder contains more than 1000 accessible entries. Narrow the request to a subfolder.",
    ));
    filesystem.listDirectoryPage.mockResolvedValueOnce({
      entries: [file("first.ts")], nextCursor: "cursor-A", done: false, busy: false,
    }).mockResolvedValueOnce({
      entries: [], nextCursor: "cursor-A", done: false, busy: true, retryAfterMs: 100,
    });
    await show("A", 0);
    await act(async () => host!.querySelector<HTMLButtonElement>(".explorer-load-more")?.click());
    expect(host!.querySelectorAll("button.explorer-entry-main")).toHaveLength(1);
    expect(host!.querySelector(".explorer-load-more")).not.toBeNull();
    expect(onNotice).toHaveBeenCalledWith(expect.stringContaining("busy"));
  });

  it("opens a newly created file even when a paginated directory does not include it on its first page", async () => {
    filesystem.listDirectory.mockRejectedValue(new Error(
      "This folder contains more than 1000 accessible entries. Narrow the request to a subfolder.",
    ));
    filesystem.listDirectoryPage.mockResolvedValue({
      entries: [file("first.ts")], nextCursor: "cursor-A", done: false, busy: false,
    });
    filesystem.getFileInfo.mockResolvedValue({
      path: "created.ts", kind: "file", size: 0, modifiedAt: 7,
    });
    await show("A", 0);
    const newFile = host!.querySelector<HTMLButtonElement>(".explorer-toolbar button");
    await act(async () => newFile?.click());
    const input = host!.querySelector<HTMLInputElement>(".explorer-inline-prompt input");
    expect(input).not.toBeNull();
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      setter.call(input, "created.ts");
      input!.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => host!.querySelector<HTMLButtonElement>(".explorer-inline-prompt button")?.click());
    expect(filesystem.createEditorFile).toHaveBeenCalledWith("A", "created.ts", "");
    expect(filesystem.getFileInfo).toHaveBeenCalledWith("A", "created.ts");
    expect(onOpenFile).toHaveBeenCalledWith(
      expect.objectContaining({ id: "A" }),
      expect.objectContaining({ path: "created.ts", kind: "file" }),
    );
  });

  it("recovers a stale directory cursor by restarting the private page read instead of retrying a dead cursor", async () => {
    filesystem.listDirectory.mockRejectedValue(new Error(
      "This folder contains more than 1000 accessible entries. Narrow the request to a subfolder.",
    ));
    filesystem.listDirectoryPage
      .mockResolvedValueOnce({ entries: [file("old-first.ts")], nextCursor: "old-cursor", done: false, busy: false })
      .mockRejectedValueOnce(new Error("Directory-page cursor is unknown or expired; start a new directory page."))
      .mockResolvedValueOnce({ entries: [file("fresh-first.ts")], nextCursor: "new-cursor", done: false, busy: false });
    await show("A", 0);
    await act(async () => host!.querySelector<HTMLButtonElement>(".explorer-load-more")?.click());
    expect(filesystem.listDirectoryPage).toHaveBeenNthCalledWith(3, "A", "", null, 200);
    expect(host!.textContent).toContain("fresh-first.ts");
    expect(host!.textContent).not.toContain("old-first.ts");
    expect(onNotice).toHaveBeenCalledWith(expect.stringContaining("refreshed"));
    await act(async () => host!.querySelector<HTMLButtonElement>(".explorer-load-more")?.click());
    expect(filesystem.listDirectoryPage).toHaveBeenLastCalledWith("A", "", "new-cursor", 200);
  });

  it("never opens project A's newly created file when metadata returns after switching to B", async () => {
    const metadata = deferred<{
      path: string; kind: "file"; size: number; modifiedAt: number;
    }>();
    filesystem.listDirectory.mockImplementation(async (id: string) => {
      if (id === "A") throw new Error(
        "This folder contains more than 1000 accessible entries. Narrow the request to a subfolder.",
      );
      return [file("B-file.ts")];
    });
    filesystem.listDirectoryPage.mockResolvedValue({
      entries: [file("A-first.ts")], nextCursor: "cursor-A", done: false, busy: false,
    });
    filesystem.getFileInfo.mockImplementation(() => metadata.promise);
    await show("A", 0);
    await act(async () => host!.querySelector<HTMLButtonElement>(".explorer-toolbar button")?.click());
    const input = host!.querySelector<HTMLInputElement>(".explorer-inline-prompt input");
    expect(input).not.toBeNull();
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      setter.call(input, "A-created.ts");
      input!.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => host!.querySelector<HTMLButtonElement>(".explorer-inline-prompt button")?.click());
    expect(filesystem.getFileInfo).toHaveBeenCalledWith("A", "A-created.ts");
    await show("B", 0);
    await act(async () => metadata.resolve({
      path: "A-created.ts", kind: "file", size: 0, modifiedAt: 7,
    }));
    expect(onOpenFile).not.toHaveBeenCalled();
    expect(host!.textContent).toContain("B-file.ts");
    expect(host!.textContent).not.toContain("A-created.ts");
  });

  it("ignores an expired-cursor failure returned after the user changes projects", async () => {
    const old = deferred<never>();
    filesystem.listDirectory.mockImplementation(async (id: string) => {
      if (id === "A") throw new Error(
        "This folder contains more than 1000 accessible entries. Narrow the request to a subfolder.",
      );
      return [file("B-file.ts")];
    });
    filesystem.listDirectoryPage
      .mockResolvedValueOnce({ entries: [file("A-first.ts")], nextCursor: "A-cursor", done: false, busy: false })
      .mockImplementationOnce(() => old.promise);
    await show("A", 0);
    await act(async () => host!.querySelector<HTMLButtonElement>(".explorer-load-more")?.click());
    await show("B", 0);
    await act(async () => old.reject(new Error(
      "Directory-page cursor is unknown or expired; start a new directory page.",
    )));
    expect(host!.textContent).toContain("B-file.ts");
    expect(host!.textContent).not.toContain("A-first.ts");
    expect(filesystem.listDirectoryPage).toHaveBeenCalledTimes(2);
    expect(onNotice).not.toHaveBeenCalled();
  });

  it("bounds rendering of a large directory and progressively reveals further entries", async () => {
    const large = Array.from({ length: 2400 }, (_, i) => file(`file-${String(i).padStart(4, "0")}.ts`));
    filesystem.listDirectory.mockResolvedValue(large);
    await show("A", 0);
    // The backend may return thousands of entries; the DOM must stay bounded.
    expect(host!.querySelectorAll("button.explorer-entry-main").length).toBeLessThanOrEqual(250);
    const more = [...host!.querySelectorAll<HTMLButtonElement>("button")]
      .find((button) => button.textContent?.includes("Show 200 more"));
    expect(more).toBeDefined();
    expect(host!.textContent).not.toContain("file-2399.ts");
    await act(async () => more?.click());
    expect(host!.querySelectorAll("button.explorer-entry-main")).toHaveLength(400);
    expect(host!.textContent).toContain("file-0399.ts");
    expect(host!.textContent).not.toContain("file-2399.ts");
  });

  it("keeps a deep selected file visible without mounting its entire large folder", async () => {
    filesystem.listDirectory.mockResolvedValue(
      Array.from({ length: 1200 }, (_, i) => file(`file-${String(i).padStart(4, "0")}.ts`)),
    );
    await show("A", 0, "file-0999.ts");
    expect(host!.textContent).toContain("file-0999.ts");
    expect(host!.querySelectorAll("button.explorer-entry-main").length).toBeLessThanOrEqual(200);
    expect(host!.textContent).not.toContain("file-1199.ts");
    const earlier = host!.querySelector<HTMLButtonElement>(".explorer-show-earlier");
    expect(earlier?.textContent).toContain("Show 200 earlier");
    await act(async () => earlier?.click());
    expect(host!.querySelectorAll("button.explorer-entry-main")).toHaveLength(400);
    expect(host!.textContent).toContain("file-0600.ts");
    expect(host!.textContent).toContain("file-0999.ts");
  });

  it("resets the expanded entry budget when selecting another project", async () => {
    filesystem.listDirectory.mockImplementation(async (id: string) =>
      id === "A" ? Array.from({ length: 500 }, (_, i) => file(`A-${String(i).padStart(3, "0")}.ts`))
                 : Array.from({ length: 500 }, (_, i) => file(`B-${String(i).padStart(3, "0")}.ts`)));
    await show("A", 0);
    const more = [...host!.querySelectorAll<HTMLButtonElement>("button")]
      .find((button) => button.textContent?.includes("Show 200 more"));
    await act(async () => more?.click());
    expect(host!.textContent).toContain("A-399.ts");
    await show("B", 0);
    expect(host!.querySelectorAll("button.explorer-entry-main")).toHaveLength(200);
    expect(host!.textContent).toContain("B-000.ts");
    expect(host!.textContent).not.toContain("A-0399.ts");
  });

  it("does not display or open project A's newly created file after switching to B", async () => {
    const creation = deferred<void>();
    filesystem.createEditorFile.mockImplementationOnce(() => creation.promise);
    filesystem.listDirectory.mockImplementation(async (id: string, path: string) =>
      path ? [] : id === "A" ? [file("from-A.ts")] : [file("from-B.ts")]);
    await show("A", 0);
    const newFile = host!.querySelector<HTMLButtonElement>(".explorer-toolbar button");
    expect(newFile).not.toBeNull();
    await act(async () => newFile!.click());
    const input = host!.querySelector<HTMLInputElement>(".explorer-inline-prompt input");
    expect(input).not.toBeNull();
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      setter.call(input, "created-in-A.ts");
      input!.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const submit = host!.querySelector<HTMLButtonElement>(".explorer-inline-prompt button");
    expect(submit?.disabled).toBe(false);
    await act(async () => submit?.click());
    expect(filesystem.createEditorFile).toHaveBeenCalledWith("A", "created-in-A.ts", "");
    await show("B", 0);
    expect(host!.textContent).toContain("from-B.ts");
    await act(async () => creation.resolve());
    expect(host!.textContent).toContain("from-B.ts");
    expect(host!.textContent).not.toContain("from-A.ts");
    expect(onOpenFile).not.toHaveBeenCalled();
  });

  it("dismisses project A's destructive confirmation before selecting project B", async () => {
    await show("A", 0);
    const row = host!.querySelector(".explorer-row");
    expect(row).not.toBeNull();
    await act(async () => row!.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 30, clientY: 30 })));
    const deleteAction = host!.querySelector<HTMLButtonElement>(".explorer-context-menu button.danger");
    expect(deleteAction).not.toBeNull();
    await act(async () => deleteAction!.click());
    expect(host!.querySelector('[role="dialog"]')?.getAttribute("aria-label")).toContain("Delete");
    await show("B", 0);
    expect(host!.querySelector('[role="dialog"]')).toBeNull();
    expect(filesystem.deleteEditorEntry).not.toHaveBeenCalled();
  });

  it("dismisses an open project A file action menu when selecting project B", async () => {
    await show("A", 0);
    const row = host!.querySelector(".explorer-row");
    await act(async () => row!.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 30, clientY: 30 })));
    expect(host!.querySelector(".explorer-context-menu")).not.toBeNull();
    await show("B", 0);
    expect(host!.querySelector(".explorer-context-menu")).toBeNull();
  });
});
