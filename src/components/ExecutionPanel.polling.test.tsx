// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  getExecutionStatus: vi.fn(),
  listCommandHistory: vi.fn(),
  listTerminalHistory: vi.fn(),
  listManagedProcesses: vi.fn(),
  listCommandPresets: vi.fn(),
  readManagedProcessOutput: vi.fn(),
  stopManagedProcess: vi.fn(),
}));

vi.mock("../lib/backend", () => backend);
vi.mock("./ApplicationLauncher", () => ({
  default: () => <div id="commands-ai-workspace" role="region" aria-label="AI Workspace" tabIndex={-1} />,
}));
vi.mock("./BrowserAutomation", () => ({ default: () => null }));
vi.mock("./MonitoringPanel", () => ({ default: () => null }));
vi.mock("./TaskIntegrityPanel", () => ({ default: () => null }));
vi.mock("./custom-applications/CustomApplicationsPanel", () => ({ default: () => null }));

import ExecutionPanel from "./ExecutionPanel";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

let host: HTMLDivElement | null = null;
let root: Root | null = null;

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  backend.getExecutionStatus.mockResolvedValue({ sandboxAvailable: false, sandboxVersion: null, message: "Synthetic" });
  backend.listCommandHistory.mockResolvedValue([]);
  backend.listTerminalHistory.mockResolvedValue([]);
  backend.listManagedProcesses.mockResolvedValue([]);
  backend.listCommandPresets.mockResolvedValue([]);
  backend.stopManagedProcess.mockResolvedValue(undefined);
  backend.readManagedProcessOutput.mockResolvedValue({
    processId: "unused", stdout: "", stderr: "",
    nextStdoutOffset: 0, nextStderrOffset: 0, outputTruncated: false,
  });
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});

afterEach(async () => {
  if (root) await act(async () => { root?.unmount(); });
  root = null;
  host?.remove();
  host = null;
  vi.useRealTimers();
});

describe("Commands background polling lifecycle", () => {
  it("does not roll an exited process back to running when an older poll finishes after Refresh", async () => {
    const stale = deferred<any[]>();
    const process = (status: "running" | "exited") => ({
      id: "process-A", workspaceId: "synthetic-A", status,
      label: "Synthetic build", command: "printf safe", cwd: "/synthetic",
      pid: status === "running" ? 121 : null,
      restartCount: 0, exitCode: status === "exited" ? 0 : null, error: null,
    });
    backend.listManagedProcesses
      .mockResolvedValueOnce([process("running")])
      .mockImplementationOnce(() => stale.promise)
      .mockResolvedValueOnce([process("exited")]);
    await act(async () => {
      root?.render(<ExecutionPanel workspaces={[]} gatewayRunning={false}
        onError={() => undefined} onNotice={() => undefined} />);
    });
    expect(host!.querySelector(".managed-process-record")?.textContent).toContain("Running");
    await act(async () => vi.advanceTimersByTimeAsync(5000));
    expect(backend.listManagedProcesses).toHaveBeenCalledTimes(2);
    const refresh = [...host!.querySelectorAll("button")].find(
      (button) => button.textContent?.trim() === "Refresh",
    );
    expect(refresh).toBeDefined();
    await act(async () => refresh!.click());
    expect(host!.querySelector(".managed-process-record")?.textContent).toContain("Exited");
    await act(async () => stale.resolve([process("running")]));
    expect(host!.querySelector(".managed-process-record")?.textContent).toContain("Exited");
    expect(host!.querySelector(".managed-process-record")?.textContent).not.toContain("Running");
  });

  it("keeps a successfully stopped process stopped when an older poll resolves later", async () => {
    const oldPoll = deferred<any[]>();
    const record = (status: "running" | "stopped") => ({
      id: "synthetic-process", workspaceId: "synthetic-A", status,
      label: "Private stop QA", command: "sleep 10", cwd: "/synthetic",
      pid: status === "running" ? 234 : null,
      restartCount: 0, exitCode: status === "stopped" ? 143 : null, error: null,
    });
    backend.listManagedProcesses
      .mockResolvedValueOnce([record("running")])
      .mockImplementationOnce(() => oldPoll.promise)
      .mockResolvedValueOnce([record("stopped")]);
    await act(async () => {
      // No selected workspace avoids a second mount-time preset refresh;
      // the process record and Stop operation are mocked and test-only.
      root?.render(<ExecutionPanel workspaces={[]} gatewayRunning={false}
        onError={() => undefined} onNotice={() => undefined} />);
    });
    await act(async () => vi.advanceTimersByTimeAsync(5000));
    const stop = [...host!.querySelectorAll("button")].find(
      (button) => button.textContent?.trim() === "Stop",
    );
    expect(stop).toBeDefined();
    await act(async () => stop!.click());
    expect(backend.stopManagedProcess).toHaveBeenCalledWith("synthetic-process");
    expect(host!.querySelector(".managed-process-record")?.textContent).toContain("Stopped");
    await act(async () => oldPoll.resolve([record("running")]));
    expect(host!.querySelector(".managed-process-record")?.textContent).toContain("Stopped");
    expect(host!.querySelector(".managed-process-record")?.textContent).not.toContain("Running");
  });

  it("never requests duplicate process output while an initial read is pending", async () => {
    const pending = deferred<any>();
    backend.listManagedProcesses.mockResolvedValue([{
      id: "synthetic-process-A", workspaceId: "synthetic-A", status: "running",
      label: "Synthetic process", command: "printf safe", cwd: "/synthetic",
      pid: 123, restartCount: 0, exitCode: null, error: null,
    }]);
    backend.readManagedProcessOutput.mockImplementation(() => pending.promise);
    await act(async () => {
      root?.render(<ExecutionPanel workspaces={[{
        id: "synthetic-A", name: "QA", path: "/synthetic",
        accessMode: "readWrite", changePolicy: "automatic", commandPolicy: "automatic", addedAt: 1,
      }]} gatewayRunning={false} onError={() => undefined} onNotice={() => undefined} />);
    });
    const output = [...host!.querySelectorAll("button")].find((button) => button.textContent?.trim() === "Output");
    expect(output).toBeDefined();
    await act(async () => output!.click());
    expect(backend.readManagedProcessOutput).toHaveBeenCalledTimes(1);
    // Initial on-click read has not returned yet. Polling must coalesce
    // instead of starting duplicate reads from offset zero.
    await act(async () => vi.advanceTimersByTime(4500));
    expect(backend.readManagedProcessOutput).toHaveBeenCalledTimes(1);
    await act(async () => {
      pending.resolve({
        processId: "synthetic-process-A", stdout: "safe", stderr: "",
        nextStdoutOffset: 4, nextStderrOffset: 0, outputTruncated: false,
      });
    });
    expect(host?.textContent).toContain("safe");
    await act(async () => vi.advanceTimersByTime(1500));
    expect(backend.readManagedProcessOutput).toHaveBeenCalledTimes(2);
    expect(backend.readManagedProcessOutput).toHaveBeenLastCalledWith("synthetic-process-A", 4, 0);
  });

  it("does not let a stale process output response unlock or overwrite another process", async () => {
    const oldRead = deferred<any>();
    const newRead = deferred<any>();
    backend.listManagedProcesses.mockResolvedValue(["A", "B"].map((id) => ({
      id: `process-${id}`, workspaceId: "synthetic-A", status: "running",
      label: `Synthetic ${id}`, command: "printf safe", cwd: "/synthetic",
      pid: null, restartCount: 0, exitCode: null, error: null,
    })));
    backend.readManagedProcessOutput.mockImplementation((id: string) =>
      id === "process-A" ? oldRead.promise : newRead.promise);
    await act(async () => {
      root?.render(<ExecutionPanel workspaces={[{
        id: "synthetic-A", name: "QA", path: "/synthetic",
        accessMode: "readWrite", changePolicy: "automatic", commandPolicy: "automatic", addedAt: 1,
      }]} gatewayRunning={false} onError={() => undefined} onNotice={() => undefined} />);
    });
    const toggle = async (processId: string) => {
      const row = [...host!.querySelectorAll(".managed-process-record")].find(
        (item) => item.textContent?.includes(`Synthetic ${processId}`),
      );
      const output = [...(row?.querySelectorAll("button") ?? [])].find(
        (button) => button.textContent?.trim() === "Output",
      );
      expect(output).toBeDefined();
      await act(async () => output!.click());
    };
    await toggle("A");
    await toggle("B");
    expect(backend.readManagedProcessOutput).toHaveBeenCalledTimes(2);

    // A returns after B was selected. It must neither appear nor clear B's
    // in-flight guard, even after several polling intervals.
    await act(async () => {
      oldRead.resolve({
        processId: "process-A", stdout: "OLD SECRET PROCESS A", stderr: "",
        nextStdoutOffset: 20, nextStderrOffset: 0, outputTruncated: false,
      });
    });
    await act(async () => vi.advanceTimersByTime(4500));
    expect(backend.readManagedProcessOutput).toHaveBeenCalledTimes(2);
    expect(host!.textContent).not.toContain("OLD SECRET PROCESS A");

    await act(async () => {
      newRead.resolve({
        processId: "process-B", stdout: "CURRENT PROCESS B", stderr: "",
        nextStdoutOffset: 17, nextStderrOffset: 0, outputTruncated: false,
      });
    });
    expect(host!.textContent).toContain("CURRENT PROCESS B");
    expect(host!.textContent).not.toContain("OLD SECRET PROCESS A");
  });

  it("jumps to the existing AI Workspace section without enabling desktop permission", async () => {
    const syntheticWorkspace = [{
      id: "synthetic-A", name: "Synthetic QA", path: "/synthetic/qa",
      accessMode: "readWrite" as const, changePolicy: "automatic" as const,
      commandPolicy: "automatic" as const, addedAt: 1,
    }];
    await act(async () => {
      root?.render(<ExecutionPanel workspaces={syntheticWorkspace}
        gatewayRunning={false} onError={() => undefined} onNotice={() => undefined} />);
    });
    // selectedWorkspaceId is reconciled on mount by an effect.
    const shortcut = [...host!.querySelectorAll<HTMLButtonElement>("button")].find(
      (button) => button.textContent?.trim() === "AI Workspace",
    );
    const destination = host!.querySelector<HTMLElement>("#commands-ai-workspace");
    expect(shortcut).toBeDefined();
    expect(shortcut?.disabled).toBe(false);
    expect(destination?.getAttribute("aria-label")).toBe("AI Workspace");
    const scroll = vi.fn();
    destination!.scrollIntoView = scroll;
    await act(async () => shortcut!.click());
    expect(scroll).toHaveBeenCalledWith({ block: "center", behavior: "smooth" });
    expect(document.activeElement).toBe(destination);
  });

  it("disables the AI Workspace shortcut when no project is selected", async () => {
    await act(async () => {
      root?.render(<ExecutionPanel workspaces={[]}
        gatewayRunning={false} onError={() => undefined} onNotice={() => undefined} />);
    });
    const shortcut = [...host!.querySelectorAll<HTMLButtonElement>("button")].find(
      (button) => button.textContent?.trim() === "AI Workspace",
    );
    expect(shortcut?.disabled).toBe(true);
  });

  it("does not overlap slow reads or issue further queries after unmount", async () => {
    const held = deferred<unknown[]>();
    backend.listCommandHistory
      .mockResolvedValueOnce([])
      .mockImplementationOnce(() => held.promise);
    const errors = vi.fn();
    await act(async () => {
      root?.render(<ExecutionPanel workspaces={[]} gatewayRunning={false} onError={errors} onNotice={() => undefined} />);
    });
    expect(backend.listCommandHistory).toHaveBeenCalledTimes(1);

    await act(async () => { vi.advanceTimersByTime(5000); });
    expect(backend.listCommandHistory).toHaveBeenCalledTimes(2);

    // Two more scheduled ticks occur while the previous async batch is blocked.
    await act(async () => { vi.advanceTimersByTime(10_000); });
    expect(backend.listCommandHistory).toHaveBeenCalledTimes(2);
    expect(backend.listTerminalHistory).toHaveBeenCalledTimes(2);
    expect(backend.listManagedProcesses).toHaveBeenCalledTimes(2);

    await act(async () => { held.resolve([]); });
    await act(async () => { vi.advanceTimersByTime(5000); });
    expect(backend.listCommandHistory).toHaveBeenCalledTimes(3);

    await act(async () => { root?.unmount(); });
    root = null;
    await act(async () => { vi.advanceTimersByTime(20_000); });
    expect(backend.listCommandHistory).toHaveBeenCalledTimes(3);
    expect(errors).not.toHaveBeenCalled();
  });

  it("does not display stale presets after changing the selected project", async () => {
    const oldPresets = deferred<Array<{id: string; label: string; command: string; timeoutSeconds: number}>>();
    backend.getExecutionStatus.mockResolvedValue({
      sandboxAvailable: true, sandboxVersion: "synthetic", message: "Ready",
    });
    backend.listCommandPresets.mockImplementation((id: string) => id === "synthetic-A"
      ? oldPresets.promise
      : Promise.resolve([{ id: "new", label: "ONLY PROJECT B PRESET", command: "true", timeoutSeconds: 5 }]));
    const workspaces = (id: string) => [{
      id, name: id, path: "/synthetic/" + id,
      accessMode: "readWrite" as const, changePolicy: "automatic" as const,
      commandPolicy: "automatic" as const, addedAt: 1,
    }];
    const onError = vi.fn();
    await act(async () => {
      root?.render(<ExecutionPanel workspaces={workspaces("synthetic-A")}
        gatewayRunning={false} onError={onError} onNotice={() => undefined} />);
    });
    expect(backend.listCommandPresets).toHaveBeenCalledWith("synthetic-A");
    await act(async () => {
      root?.render(<ExecutionPanel workspaces={workspaces("synthetic-B")}
        gatewayRunning={false} onError={onError} onNotice={() => undefined} />);
    });
    expect(backend.listCommandPresets).toHaveBeenCalledWith("synthetic-B");
    expect(host?.textContent).toContain("ONLY PROJECT B PRESET");
    await act(async () => {
      oldPresets.resolve([{ id: "old", label: "STALE PROJECT A PRESET",
        command: "false", timeoutSeconds: 5 }]);
    });
    expect(host?.textContent).toContain("ONLY PROJECT B PRESET");
    expect(host?.textContent).not.toContain("STALE PROJECT A PRESET");
    expect(onError).not.toHaveBeenCalled();
  });

  it("discards an in-flight poll after the Commands view unmounts", async () => {
    const held = deferred<unknown[]>();
    backend.listCommandHistory.mockResolvedValueOnce([]).mockImplementationOnce(() => held.promise);
    await act(async () => {
      root?.render(<ExecutionPanel workspaces={[]} gatewayRunning={false} onError={() => undefined} onNotice={() => undefined} />);
    });
    await act(async () => { vi.advanceTimersByTime(5000); });
    expect(backend.listCommandHistory).toHaveBeenCalledTimes(2);
    await act(async () => { root?.unmount(); });
    root = null;
    await act(async () => { held.resolve([{ id: "stale", status: "running" }]); });
    expect(backend.listCommandHistory).toHaveBeenCalledTimes(2);
    await act(async () => { vi.advanceTimersByTime(10000); });
    expect(backend.listCommandHistory).toHaveBeenCalledTimes(2);
  });
});
