// @vitest-environment jsdom
import React, { act, Profiler } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { LaunchActionRecord, Workspace } from "../types";

const backend = vi.hoisted(() => ({
  listLaunchableApplications: vi.fn(),
  listDeepIntegrations: vi.fn(),
  getDesktopControlEnabled: vi.fn(),
  setDesktopControlEnabled: vi.fn(),
  getGmailAccessEnabled: vi.fn(),
  setGmailAccessEnabled: vi.fn(),
  setDeepIntegrationEnabled: vi.fn(),
  getGithubConnectionStatus: vi.fn(),
  listLaunchHistory: vi.fn(),
}));
vi.mock("../lib/backend", () => backend);
vi.mock("./ai-workspace/AIWorkspacePanel", () => ({ default: () => null }));
vi.mock("./ConfirmationDialog", () => ({ default: () => null }));
import ApplicationLauncher from "./ApplicationLauncher";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
const ws = (id: string): Workspace => ({
  id, name: "Synthetic " + id, path: "/test-only/" + id, addedAt: 1,
  accessMode: "readWrite", changePolicy: "automatic", commandPolicy: "automatic",
});
const record = (id: string, label = id): LaunchActionRecord => ({
  id, workspaceId: id, workspaceName: id, kind: "url",
  target: "https://example.test/" + label, applicationId: null, applicationName: null,
  status: "launched", createdAt: 1, updatedAt: 1, pid: null, error: null,
});
const onError = vi.fn();
let mount: HTMLDivElement | null = null;
let root: Root | null = null;
let commits = 0;
beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  commits = 0;
  backend.listLaunchableApplications.mockResolvedValue([]);
  backend.listDeepIntegrations.mockResolvedValue([]);
  backend.getDesktopControlEnabled.mockResolvedValue(false);
  backend.setDesktopControlEnabled.mockResolvedValue(true);
  backend.getGmailAccessEnabled.mockResolvedValue(false);
  backend.setGmailAccessEnabled.mockResolvedValue(true);
  backend.setDeepIntegrationEnabled.mockResolvedValue([]);
  backend.getGithubConnectionStatus.mockResolvedValue({
    available: false, connected: false, connecting: false, username: null,
    deviceCode: null, verificationUrl: null, message: null,
  });
  backend.listLaunchHistory.mockImplementation(async (id: string) => [record(id)]);
  mount = document.createElement("div");
  document.body.append(mount);
  root = createRoot(mount);
});
afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  mount?.remove();
  mount = null;
  vi.useRealTimers();
});
async function show(id: string, gatewayRunning = true) {
  await act(async () => root?.render(
    <Profiler id="launcher" onRender={() => commits++}>
      <ApplicationLauncher workspace={ws(id)} gatewayRunning={gatewayRunning} onError={onError}/>
    </Profiler>,
  ));
}
describe("Commands application launch history observation", () => {
  it("does not show project A's late Desktop permission toggle on selected project B", async () => {
    const held = deferred<boolean>();
    backend.setDesktopControlEnabled.mockImplementation(() => held.promise);
    await show("A");
    const desktop = () => [...mount!.querySelectorAll<HTMLButtonElement>("button")].find(
      (button) => button.textContent?.trim() === "Desktop",
    );
    expect(desktop()?.getAttribute("aria-pressed")).toBe("false");
    await act(async () => desktop()!.click());
    expect(backend.setDesktopControlEnabled).toHaveBeenCalledWith("A", true);
    await show("B");
    expect(desktop()?.getAttribute("aria-pressed")).toBe("false");
    await act(async () => held.resolve(true));
    expect(desktop()?.getAttribute("aria-pressed")).toBe("false");
    expect(desktop()?.disabled).toBe(false);
  });

  it("does not show project A's late Gmail toggle on project B", async () => {
    const held = deferred<boolean>();
    backend.setGmailAccessEnabled.mockImplementation(() => held.promise);
    await show("A");
    const gmail = () => [...mount!.querySelectorAll<HTMLButtonElement>("button")].find(
      (button) => button.textContent?.trim() === "Gmail",
    );
    await act(async () => gmail()!.click());
    expect(backend.setGmailAccessEnabled).toHaveBeenCalledWith("A", true);
    await show("B");
    expect(gmail()?.getAttribute("aria-pressed")).toBe("false");
    await act(async () => held.resolve(true));
    expect(gmail()?.getAttribute("aria-pressed")).toBe("false");
    expect(gmail()?.disabled).toBe(false);
  });

  it("invalidates a delayed Desktop response even when switching A to B and back to A", async () => {
    const held = deferred<boolean>();
    backend.setDesktopControlEnabled.mockImplementationOnce(() => held.promise);
    await show("A");
    const desktop = () => [...mount!.querySelectorAll<HTMLButtonElement>("button")].find(
      (button) => button.textContent?.trim() === "Desktop",
    );
    await act(async () => desktop()!.click());
    await show("B");
    await show("A");
    expect(desktop()?.getAttribute("aria-pressed")).toBe("false");
    await act(async () => held.resolve(true));
    expect(desktop()?.getAttribute("aria-pressed")).toBe("false");
    expect(desktop()?.disabled).toBe(false);
  });

  it("discards a delayed deep-integration result from another selected project", async () => {
    const held = deferred<Array<{
      id: string; name: string; available: boolean; enabled: boolean;
      actions: string[]; message: string | null;
    }>>();
    const integration = (enabled: boolean) => [{
      id: "synthetic-tool", name: "Synthetic Connector",
      available: true, enabled, actions: [], message: null,
    }];
    backend.listDeepIntegrations.mockResolvedValue(integration(false));
    backend.setDeepIntegrationEnabled.mockImplementation(() => held.promise);
    await show("A");
    const toggle = () => [...mount!.querySelectorAll<HTMLButtonElement>("button")].find(
      (button) => button.textContent?.trim() === "Synthetic Connector",
    );
    expect(toggle()?.getAttribute("aria-pressed")).toBe("false");
    await act(async () => toggle()!.click());
    expect(backend.setDeepIntegrationEnabled).toHaveBeenCalledWith("A", "synthetic-tool", true);
    await show("B");
    expect(toggle()?.getAttribute("aria-pressed")).toBe("false");
    await act(async () => held.resolve(integration(true)));
    expect(toggle()?.getAttribute("aria-pressed")).toBe("false");
    expect(toggle()?.disabled).toBe(false);
  });

  it("coalesces slow 3s history polls, and stops polling after unmount", async () => {
    const held = deferred<LaunchActionRecord[]>();
    backend.listLaunchHistory
      .mockResolvedValueOnce([record("A")])
      .mockImplementationOnce(() => held.promise);
    await show("A");
    expect(backend.listLaunchHistory).toHaveBeenCalledTimes(1);
    await act(async () => { await vi.advanceTimersByTimeAsync(3_000); });
    expect(backend.listLaunchHistory).toHaveBeenCalledTimes(2);
    await act(async () => { await vi.advanceTimersByTimeAsync(9_000); });
    expect(backend.listLaunchHistory).toHaveBeenCalledTimes(2);
    await act(async () => held.resolve([record("A")]));
    await act(async () => { await vi.advanceTimersByTimeAsync(3_000); });
    expect(backend.listLaunchHistory).toHaveBeenCalledTimes(3);
    await act(async () => root?.unmount());
    root = null;
    await act(async () => { await vi.advanceTimersByTimeAsync(9_000); });
    expect(backend.listLaunchHistory).toHaveBeenCalledTimes(3);
  });

  it("ignores delayed project A history and application inventory after switching to B", async () => {
    const historyA = deferred<LaunchActionRecord[]>();
    const applicationsA = deferred<Array<{
      id:string; name:string; category:string; executable:string;
      supportsUrls:boolean; supportsPaths:boolean;
    }>>();
    backend.listLaunchHistory.mockImplementation((id: string) =>
      id === "A" ? historyA.promise : Promise.resolve([record("B")]));
    backend.listLaunchableApplications.mockImplementation((id: string) =>
      id === "A" ? applicationsA.promise : Promise.resolve([{
        id: "b-app", name: "ONLY PROJECT B APP", category: "Tools",
        executable: "/test-only/b-app", supportsUrls: false, supportsPaths: false,
      }]));
    await show("A");
    await show("B");
    expect(mount?.textContent).toContain("example.test/B");
    expect(mount?.textContent).toContain("ONLY PROJECT B APP");
    await act(async () => {
      historyA.resolve([record("A", "STALE A HISTORY")]);
      applicationsA.resolve([{
        id: "a-app", name: "STALE PROJECT A APP", category: "Tools",
        executable: "/test-only/a-app", supportsUrls: false, supportsPaths: false,
      }]);
    });
    expect(mount?.textContent).toContain("ONLY PROJECT B APP");
    expect(mount?.textContent).not.toContain("STALE PROJECT A APP");
    expect(mount?.textContent).not.toContain("STALE A HISTORY");
    expect(onError).not.toHaveBeenCalled();
  });

  it("keeps React view identity when repeated history polls return identical records", async () => {
    await show("A");
    await act(async () => { await vi.advanceTimersByTimeAsync(3_000); });
    const settled = commits;
    for (let i = 0; i < 4; i++) {
      await act(async () => { await vi.advanceTimersByTimeAsync(3_000); });
    }
    expect(backend.listLaunchHistory).toHaveBeenCalledTimes(6);
    expect(commits).toBe(settled);
  });

  it("manual app refresh wins against a slow older background launch-history read", async () => {
    const held = deferred<LaunchActionRecord[]>();
    backend.listLaunchHistory.mockResolvedValueOnce([record("A", "INITIAL")])
      .mockImplementationOnce(() => held.promise)
      .mockResolvedValueOnce([record("A", "MANUAL CURRENT")]);
    await show("A");
    await act(async () => { await vi.advanceTimersByTimeAsync(3_000); });
    expect(backend.listLaunchHistory).toHaveBeenCalledTimes(2);
    const refresh = [...mount!.querySelectorAll("button")].find(x => x.textContent === "Refresh apps");
    expect(refresh).toBeTruthy();
    await act(async () => refresh?.click());
    expect(mount?.textContent).toContain("MANUAL CURRENT");
    await act(async () => held.resolve([record("A", "STALE OLD POLL")]));
    expect(mount?.textContent).toContain("MANUAL CURRENT");
    expect(mount?.textContent).not.toContain("STALE OLD POLL");
    expect(onError).not.toHaveBeenCalled();
  });
});
