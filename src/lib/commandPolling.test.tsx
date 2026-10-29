// @vitest-environment jsdom
import React, { act, Profiler, useState } from "react";
import { createRoot } from "react-dom/client";
import { describe, expect, it } from "vitest";
import { mergePolledProcessLog, reconcilePolledRecords } from "./commandPolling";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

const command = { id: "one", status: "running", updatedAt: 10, stdout: "first" };
const process = { id: "two", status: "exited", updatedAt: 11, stdout: "done" };

describe("Commands history and process polling", () => {
  it("keeps the whole array and individual object identities when a snapshot is identical", () => {
    const previous = [command, process];
    expect(reconcilePolledRecords(previous, previous)).toBe(previous);
    expect(reconcilePolledRecords(previous, [{ ...command }, { ...process }])).toBe(previous);
  });

  it("preserves unmodified records and propagates actual process status/output changes", () => {
    const previous = [command, process];
    const next = reconcilePolledRecords(previous, [{ ...command }, { ...process, status: "running" }]);
    expect(next).not.toBe(previous);
    expect(next[0]).toBe(command);
    expect(next[1]).toMatchObject({ id: "two", status: "running" });
    expect(reconcilePolledRecords(previous, [command])).toHaveLength(1);
    expect(reconcilePolledRecords(previous, [process, command])[0].id).toBe("two");
    expect(reconcilePolledRecords(previous, [command, process, { id: "new", status: "pending", updatedAt: 2, stdout: "" }])).toHaveLength(3);
    const logChange = reconcilePolledRecords(previous, [{ ...command, stdout: "changed" }, process]);
    expect(logChange[0].stdout).toBe("changed");
    expect(logChange[1]).toBe(process);
  });

  it("prevents repeated no-output chunks from rerendering but applies output and offsets", () => {
    const current = {
      processId: "p1", stdout: "hello", stderr: "", stdoutOffset: 5, stderrOffset: 0,
      outputTruncated: false,
    };
    const idle = {
      processId: "p1", stdout: "", stderr: "", nextStdoutOffset: 5,
      nextStderrOffset: 0, outputTruncated: false,
    };
    expect(mergePolledProcessLog(current, idle)).toBe(current);
    expect(mergePolledProcessLog(current, { ...idle, nextStdoutOffset: 6 }))
      .toMatchObject({ stdoutOffset: 6, stdout: "hello" });
    expect(mergePolledProcessLog(current, { ...idle, stdout: "!", nextStdoutOffset: 6 }))
      .toMatchObject({ stdout: "hello!", stdoutOffset: 6 });
    expect(mergePolledProcessLog(current, { ...idle, stderr: "problem", nextStderrOffset: 7 }))
      .toMatchObject({ stderr: "problem", stderrOffset: 7 });
    expect(mergePolledProcessLog(current, { ...idle, outputTruncated: true }))
      .toMatchObject({ outputTruncated: true });
    expect(mergePolledProcessLog(current, { ...idle, processId: "p2" }))
      .toMatchObject({ processId: "p2", stdout: "", stderr: "" });
  });

  it("compares React commits across eight identical background poll cycles", async () => {
    async function commitsWith(previousMerge: boolean) {
      const mount = document.createElement("div");
      document.body.append(mount);
      const root = createRoot(mount);
      const initial = [command, process];
      let poll: (() => void) | null = null;
      let commits = 0;
      function Probe() {
        const [history, setHistory] = useState(initial);
        poll = () => setHistory((current) => previousMerge
          ? [{ ...command }, { ...process }]
          : reconcilePolledRecords(current, [{ ...command }, { ...process }]));
        return <span>{history.length}</span>;
      }
      try {
        await act(async () => root.render(
          <Profiler id="command-history" onRender={() => commits++}><Probe /></Profiler>,
        ));
        for (let i = 0; i < 8; i++) {
          await act(async () => poll?.());
        }
        return commits;
      } finally {
        await act(async () => root.unmount());
        mount.remove();
      }
    }
    expect(await commitsWith(true)).toBe(9);
    expect(await commitsWith(false)).toBe(1);
  });
});
