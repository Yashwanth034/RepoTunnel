import { describe, expect, it, vi } from "vitest";
import type { Workspace } from "../types";
import type { EditorDocument } from "../components/WorkspaceEditor";
import { restoreEditorDocuments, type SavedEditorTab } from "./editorSessionRestore";

const workspace = (id: string): Workspace => ({
  id, name: "Synthetic " + id, path: "/test-only/" + id,
  addedAt: 1, accessMode: "readWrite", changePolicy: "review",
  commandPolicy: "review",
});
const saved = (path: string, extra: Partial<SavedEditorTab> = {}): SavedEditorTab => ({
  workspaceId: "A", path, dirty: false, ...extra,
});
const document = (path: string, text: string, modifiedAt = 1): EditorDocument => ({
  key: "A:" + path, workspaceId: "A", workspaceName: "Synthetic A",
  path, name: path.split("/").at(-1) ?? path, kind: "text", language: "typescript",
  content: text, savedContent: text, size: text.length, modifiedAt,
  imageDataUrl: null, dirty: false, externalContent: null,
  externalModifiedAt: null, conflict: false, externalDeleted: false,
  updatedExternally: false, readonly: false,
});
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((success, failure) => {
    resolve = success; reject = failure;
  });
  return { promise, resolve, reject };
}

describe("concurrent persisted editor session restoration", () => {
  it("caps at four outstanding tab loads while preserving all 16 original positions", async () => {
    const tabs = Array.from({ length: 19 }, (_, index) => saved("tab-" + index + ".ts"));
    const waits = Array.from({ length: 16 }, () => deferred<EditorDocument>());
    const called: string[] = [];
    let active = 0;
    let peak = 0;
    const load = vi.fn(async (_workspace: Workspace, path: string) => {
      const index = Number(path.match(/\d+/)?.[0]);
      called.push(path);
      peak = Math.max(peak, ++active);
      try { return await waits[index].promise; }
      finally { active--; }
    });
    const restoration = restoreEditorDocuments(tabs, [workspace("A")], load);
    expect(called).toEqual(tabs.slice(0, 4).map(item => item.path));
    expect(peak).toBe(4);

    // Complete the first four in reverse order, then the rest. The UI tab
    // ordering must match what was persisted, never IPC completion order.
    for (const index of [3, 2, 1, 0, ...Array.from({ length: 12 }, (_, i) => i + 4)]) {
      waits[index].resolve(document(tabs[index].path, "disk " + index));
      for (let i = 0; i < 5; i++) await Promise.resolve();
    }
    const result = await restoration;
    expect(load).toHaveBeenCalledTimes(16);
    expect(peak).toBe(4);
    expect(result.map(item => item.path)).toEqual(tabs.slice(0, 16).map(item => item.path));
  });

  it("keeps a dirty draft when disk content did not change", async () => {
    const result = await restoreEditorDocuments([
      saved("local.ts", { dirty: true, savedContent: "original", draftContent: "my unsaved edit" }),
    ], [workspace("A")], async (_workspace, path) => document(path, "original"));
    expect(result).toHaveLength(1);
    expect(result[0].content).toBe("my unsaved edit");
    expect(result[0].savedContent).toBe("original");
    expect(result[0].dirty).toBe(true);
    expect(result[0].conflict).toBe(false);
  });

  it("recognizes a draft already present on disk and does not falsely mark it dirty", async () => {
    const result = await restoreEditorDocuments([
      saved("already-saved.ts", { dirty: true, savedContent: "previous", draftContent: "current disk" }),
    ], [workspace("A")], async (_workspace, path) => document(path, "current disk"));
    expect(result[0].content).toBe("current disk");
    expect(result[0].dirty).toBe(false);
    expect(result[0].conflict).toBe(false);
  });

  it("never drops local edits when both disk and draft changed; preserve conflict evidence", async () => {
    const result = await restoreEditorDocuments([
      saved("conflict.ts", { dirty: true, savedContent: "base", draftContent: "my local edit" }),
    ], [workspace("A")], async (_workspace, path) => document(path, "other process edit", 1234));
    expect(result[0]).toMatchObject({
      dirty: true, conflict: true, content: "my local edit",
      savedContent: "base", externalContent: "other process edit",
      externalModifiedAt: 1234,
    });
  });

  it("skips deleted, inaccessible, disallowed and malformed tabs without losing valid tabs", async () => {
    const restored = await restoreEditorDocuments([
      saved("keep.ts"),
      saved("deleted.ts"),
      saved("other-workspace.ts", { workspaceId: "B" }),
      saved(""),
      saved("readonly-error.ts"),
      saved("other-keep.ts"),
    ], [workspace("A")], async (_workspace, path) => {
      if (path === "deleted.ts" || path === "readonly-error.ts") throw new Error("not readable");
      return document(path, path);
    });
    expect(restored.map(item => item.path)).toEqual(["keep.ts", "other-keep.ts"]);
  });

  it("keeps regular binary or image documents without treating saved drafts as editable text", async () => {
    const result = await restoreEditorDocuments([
      saved("artifact.png", { dirty: true, savedContent: "old", draftContent: "cannot edit binary" }),
    ], [workspace("A")], async (_workspace, path) => ({
      ...document(path, ""), kind: "image", language: "image",
      imageDataUrl: "data:image/png;base64,aGVsbG8=",
    }));
    expect(result[0].dirty).toBe(false);
    expect(result[0].conflict).toBe(false);
    expect(result[0].content).toBe("");
  });
});
