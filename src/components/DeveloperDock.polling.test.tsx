// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  listTerminalHistory: vi.fn(),
  listManagedProcesses: vi.fn(),
  inspectProject: vi.fn(),
}));
vi.mock("../lib/backend", () => backend);
vi.mock("../lib/filesystem", () => ({ listDirectory: vi.fn(), readFile: vi.fn() }));
vi.mock("./AppSidebar", () => ({ NavIcon: () => null }));

import DeveloperDock from "./DeveloperDock";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

let host: HTMLDivElement | null = null;
let root: Root | null = null;

beforeEach(() => {
  vi.clearAllMocks();
  backend.listTerminalHistory.mockResolvedValue([]);
  backend.listManagedProcesses.mockResolvedValue([]);
  backend.inspectProject.mockResolvedValue({ overview: { manifests: [] } });
  window.localStorage.setItem("repotunnel.developerDock.synthetic-A",
    JSON.stringify({ collapsed: false, activeTab: "terminal", height: 245 }));
  window.localStorage.setItem("repotunnel.developerDock.synthetic-B",
    JSON.stringify({ collapsed: false, activeTab: "terminal", height: 245 }));
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
  window.localStorage.removeItem("repotunnel.developerDock.synthetic-A");
  window.localStorage.removeItem("repotunnel.developerDock.synthetic-B");
});

describe("Developer Dock project-scoped background polling", () => {
  it("ignores a delayed result from the previous project after switching workspaces", async () => {
    const old = deferred<Array<{ id: string; label: string; status: string }>>();
    backend.listManagedProcesses.mockImplementation((id: string) =>
      id === "synthetic-A" ? old.promise : Promise.resolve([
        { id: "synthetic-b-process", label: "Only Project B", status: "running" },
      ]));
    const onNotice = vi.fn();
    function panel(id: string) {
      return <DeveloperDock
        workspaceId={id}
        workspaceName={id}
        workspacePath={`/synthetic/${id}`}
        onOpenProblem={() => undefined}
        onNotice={onNotice}
      />;
    }
    await act(async () => root?.render(panel("synthetic-A")));
    expect(backend.listManagedProcesses).toHaveBeenCalledWith("synthetic-A", 30);
    await act(async () => root?.render(panel("synthetic-B")));
    expect(backend.listManagedProcesses).toHaveBeenCalledWith("synthetic-B", 30);

    const processes = Array.from(host!.querySelectorAll<HTMLButtonElement>("button"))
      .find((button) => button.textContent?.includes("Processes"))!;
    await act(async () => processes.click());
    expect(host?.textContent).toContain("Only Project B");

    await act(async () => old.resolve([
      { id: "synthetic-a-process", label: "STALE PROJECT A", status: "running" },
    ]));
    expect(host?.textContent).toContain("Only Project B");
    expect(host?.textContent).not.toContain("STALE PROJECT A");
    expect(onNotice).not.toHaveBeenCalled();
  });
});
