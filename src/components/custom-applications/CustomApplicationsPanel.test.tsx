// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  listCustomApplications: vi.fn(),
  discoverCustomApplications: vi.fn(),
  listLaunchableApplications: vi.fn(),
  addDiscoveredCustomApplication: vi.fn(),
  pickCustomApplication: vi.fn(),
  removeCustomApplication: vi.fn(),
  openUrl: vi.fn(),
  launchDiscoveredDesktopApplication: vi.fn(),
  startAiWorkspace: vi.fn(),
}));
vi.mock("../../lib/backend", () => backend);
import CustomApplicationsPanel from "./CustomApplicationsPanel";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
let root: Root | null;
let host: HTMLDivElement;
const onError = vi.fn();
const onNotice = vi.fn();

async function flush() {
  await act(async () => { await new Promise((resolve) => window.setTimeout(resolve, 0)); });
}
async function render() {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => root?.render(<CustomApplicationsPanel workspaceId="workspace-qa"
    onError={onError} onNotice={onNotice} />));
  await flush();
}
async function click(button: Element | null | undefined) {
  expect(button).toBeTruthy();
  await act(async () => { button?.dispatchEvent(new MouseEvent("click", { bubbles: true })); });
  await flush();
}
function button(label: string) {
  return [...host.querySelectorAll("button")].find((element) => element.textContent?.trim() === label);
}
function row(name: string) {
  return [...host.querySelectorAll('[role="listitem"]')].find((el) => el.textContent?.includes(name));
}

beforeEach(() => {
  vi.clearAllMocks();
  window.localStorage.clear();
  backend.listCustomApplications.mockResolvedValue([]);
  backend.discoverCustomApplications.mockResolvedValue([
    { id: "custom-inkscape", name: "Inkscape", launchMode: "native" },
    { id: "desktop-flatpak", name: "Signal (Flatpak)", launchMode: "desktop" },
    { id: "desktop-snap", name: "Telegram (Snap)", launchMode: "desktop" },
  ]);
  backend.listLaunchableApplications.mockResolvedValue([
    { id: "vscode", name: "Visual Studio Code", category: "editor", executable: "/usr/bin/code", supportsUrls: false, supportsPaths: true },
    { id: "google-chrome", name: "Google Chrome", category: "browser", executable: "/usr/bin/google-chrome", supportsUrls: true, supportsPaths: true },
    { id: "docker", name: "Docker", category: "development", executable: "/usr/bin/docker", supportsUrls: false, supportsPaths: false },
  ]);
  backend.pickCustomApplication.mockResolvedValue(null);
  backend.removeCustomApplication.mockResolvedValue([]);
  backend.addDiscoveredCustomApplication.mockResolvedValue({ id: "custom-inkscape", name: "Inkscape", executable: "/usr/bin/inkscape" });
  backend.openUrl.mockResolvedValue({ launch: { status: "launched" }, queued: false });
  backend.launchDiscoveredDesktopApplication.mockResolvedValue({ launch: { status: "pending" }, queued: true });
});
afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
});

describe("Simple application picker", () => {
  it("shows only locally installed applications, without browser or website options", async () => {
    await render();
    await click(button("Add application"));
    expect(host.querySelector('[role="dialog"][aria-label="Choose an application"]')).toBeTruthy();
    expect(host.textContent).toContain("Inkscape");
    expect(host.textContent).toContain("Telegram");
    expect(host.textContent).not.toContain("WhatsApp");
    expect(host.textContent).not.toContain("YouTube");
    expect(host.textContent).not.toContain("Add website");
    expect(host.textContent).not.toContain("Web apps");
    expect(host.textContent).not.toContain("Google Chrome");
    expect(host.textContent).not.toContain("Docker");
    expect(backend.pickCustomApplication).not.toHaveBeenCalled();
  });

  it("adds an installed app by clicking Add without opening Files, revalidating in backend", async () => {
    backend.listCustomApplications.mockResolvedValueOnce([])
      .mockResolvedValueOnce([{ id: "custom-inkscape", name: "Inkscape", executable: "/usr/bin/inkscape" }]);
    await render();
    await click(button("Add application"));
    await click(row("Inkscape")?.querySelector("button"));
    expect(backend.addDiscoveredCustomApplication).toHaveBeenCalledWith("custom-inkscape");
    expect(backend.pickCustomApplication).not.toHaveBeenCalled();
    expect(host.textContent).toContain("AI Workspace approved");
  });

  it("ignores old saved website shortcuts without deleting saved installed applications", async () => {
    window.localStorage.setItem("repotunnel.application-shortcuts.v1", JSON.stringify([
      { id: "website:https://x.com/", name: "X Website", kind: "web", url: "https://x.com/" },
      { id: "vscode", name: "Visual Studio Code", kind: "builtin" },
    ]));
    await render();
    await click(button("Add application"));
    expect(host.textContent).not.toContain("X Website");
    expect(host.textContent).toContain("Visual Studio Code");
    expect(backend.openUrl).not.toHaveBeenCalled();
    expect(window.localStorage.getItem("repotunnel.application-shortcuts.v1")).toContain("vscode");
  });

  it("pins a Flatpak app without elevating it to native AI Workspace access", async () => {
    await render();
    await click(button("Add application"));
    await click(row("Signal (Flatpak)")?.querySelector("button"));
    expect(backend.addDiscoveredCustomApplication).not.toHaveBeenCalled();
    expect(backend.startAiWorkspace).not.toHaveBeenCalled();
    expect(host.textContent).toContain("system desktop (not AI Workspace)");
    await click(button("Open"));
    expect(backend.launchDiscoveredDesktopApplication).toHaveBeenCalledWith("workspace-qa", "desktop-flatpak");
    expect(onNotice).toHaveBeenCalledWith(expect.stringContaining("waiting for project approval"));
  });

  it("offers Snap desktop apps in the same searchable chooser", async () => {
    await render();
    await click(button("Add application"));
    expect(row("Telegram (Snap)")).toBeTruthy();
    expect(host.textContent).toContain("system desktop");
  });

  it("searches installed applications without offering any website input", async () => {
    await render();
    await click(button("Add application"));
    const search = host.querySelector<HTMLInputElement>('[aria-label="Search installed applications"]')!;
    expect(search).toBeTruthy();
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      setter.call(search, "Inkscape");
      search.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(row("Inkscape")).toBeTruthy();
    expect(row("Telegram (Snap)")).toBeUndefined();
    expect(host.querySelector('[aria-label="New website URL"]')).toBeNull();
    expect(host.querySelector('[aria-label="New website name"]')).toBeNull();
  });

  it("keeps the executable file picker under a clearly labeled Advanced action", async () => {
    await render();
    expect(backend.pickCustomApplication).not.toHaveBeenCalled();
    await click(button("Add application"));
    await click(button("Choose executable file (advanced)"));
    expect(backend.pickCustomApplication).toHaveBeenCalledTimes(1);
  });

  it("removes a previously approved application without touching any others", async () => {
    backend.listCustomApplications.mockResolvedValue([{ id: "custom-inkscape", name: "Inkscape", executable: "/usr/bin/inkscape" }]);
    await render();
    await click(button("Remove"));
    expect(backend.removeCustomApplication).toHaveBeenCalledWith("custom-inkscape");
    expect(host.textContent).not.toContain("/usr/bin/inkscape");
  });
});
