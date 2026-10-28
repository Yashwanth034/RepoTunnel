// @vitest-environment jsdom
import React, { act, Profiler, useState } from "react";
import { createRoot } from "react-dom/client";
import { describe, expect, it } from "vitest";
import { markMissingEditorFile, reconcileEditorFileChecks } from "./editorSync";

type Tab = { key: string; content: string; modifiedAt: number };
const one: Tab = { key: "a", content: "original", modifiedAt: 1 };
const two: Tab = { key: "b", content: "other", modifiedAt: 2 };
(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

describe("editor background synchronization", () => {
  it("preserves the array and tab identities when all metadata is unchanged", () => {
    const current = [one, two];
    expect(reconcileEditorFileChecks(current, current, [one, two])).toBe(current);
    expect(reconcileEditorFileChecks(current, current, current)[0]).toBe(one);
  });

  it("updates only an actually changed tab", () => {
    const current = [one, two];
    const updated = { ...two, modifiedAt: 3, content: "external" };
    const next = reconcileEditorFileChecks(current, current, [one, updated]);
    expect(next).not.toBe(current);
    expect(next[0]).toBe(one);
    expect(next[1]).toBe(updated);
  });

  it("does not overwrite a newer unsaved edit with stale async disk results", () => {
    const oldSnapshot = [one, two];
    const newerEdit = { ...one, content: "my unsaved typing" };
    const current = [newerEdit, two];
    const staleResult = { ...one, content: "disk updated", modifiedAt: 4 };
    expect(reconcileEditorFileChecks(current, oldSnapshot, [staleResult, two])).toBe(current);
    expect(current[0].content).toBe("my unsaved typing");
  });

  it("does not resurrect closed tabs or mutate replacement tabs", () => {
    expect(reconcileEditorFileChecks([two], [one, two], [{ ...one }, two])).toEqual([two]);
    expect(reconcileEditorFileChecks([one], [one], [{ ...one, key: "different" }])).toEqual([one]);
    expect(reconcileEditorFileChecks([one], [one], [])).toEqual([one]);
  });

  it("stops re-publishing the same file-missing status across subsequent polls", () => {
    const original = { ...one, externalDeleted: false };
    const firstMissing = markMissingEditorFile(original);
    expect(firstMissing).not.toBe(original);
    expect(firstMissing.externalDeleted).toBe(true);
    expect(markMissingEditorFile(firstMissing)).toBe(firstMissing);
    const current = [firstMissing];
    const result = [markMissingEditorFile(firstMissing)];
    expect(reconcileEditorFileChecks(current, current, result)).toBe(current);
  });

  it("does not commit React renders on repeated unchanged background checks", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const root = createRoot(host);
    const original = [one, two];
    let poll: (() => void) | undefined;
    let commits = 0;
    function Probe() {
      const [tabs, setTabs] = useState(original);
      poll = () => setTabs((current) => reconcileEditorFileChecks(current, original, original));
      return <div data-count={tabs.length}>{tabs[0].content}</div>;
    }
    try {
      await act(async () => root.render(
        <Profiler id="editor" onRender={() => commits++}><Probe /></Profiler>,
      ));
      expect(commits).toBe(1);
      for (let n = 0; n < 8; n++) {
        await act(async () => poll?.());
      }
      expect(commits).toBe(1);
    } finally {
      await act(async () => root.unmount());
      host.remove();
    }
  });

  it("compares React commit counts for the former and corrected no-change paths", async () => {
    async function measureCommits(usePreviousMerge: boolean): Promise<number> {
      const host = document.createElement("div");
      document.body.append(host);
      const root = createRoot(host);
      const checked = [one, two];
      let poll: (() => void) | undefined;
      let commits = 0;
      function Probe() {
        const [tabs, setTabs] = useState(checked);
        poll = () => setTabs((current) => usePreviousMerge
          ? current.map((tab) => checked.find((item) => item.key === tab.key) ?? tab)
          : reconcileEditorFileChecks(current, checked, checked));
        return <span>{tabs.length}</span>;
      }
      try {
        await act(async () => root.render(
          <Profiler id="editor-commit-count" onRender={() => commits++}><Probe /></Profiler>,
        ));
        for (let index = 0; index < 8; index++) await act(async () => poll?.());
        return commits;
      } finally {
        await act(async () => root.unmount());
        host.remove();
      }
    }
    // Includes the first mount. The old merge publishes a new array every poll.
    expect(await measureCommits(true)).toBe(9);
    expect(await measureCommits(false)).toBe(1);
  });
});
