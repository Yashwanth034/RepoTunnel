// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ChatConnectionStatus } from "../types";
import ChatConnectionPanel from "./ChatConnectionPanel";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let host: HTMLDivElement | null = null;
const onConnect = vi.fn(async (_tunnelId: string, _apiKey: string) => undefined);
const onDisconnect = vi.fn(async () => undefined);
const onPause = vi.fn(async () => undefined);
const onResume = vi.fn(async () => undefined);
const onRemember = vi.fn(async () => undefined);
const onInstallClient = vi.fn(async () => undefined);
const onOpenGuide = vi.fn();

const unavailableStatus: ChatConnectionStatus = {
  paused: false,
  remembered: false,
  rememberSupported: true,
  clientAvailable: false,
  clientVersion: null,
  running: false,
  ready: false,
  tunnelId: null,
  adminUrl: null,
  message: null,
};

async function renderPanel(status: ChatConnectionStatus = unavailableStatus) {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => {
    root?.render(
      <ChatConnectionPanel
        status={status}
        gatewayRunning={false}
        busy={false}
        aiAccessPaused={false}
        onConnect={onConnect}
        onDisconnect={onDisconnect}
        onPause={onPause}
        onResume={onResume}
        onRemember={onRemember}
        onInstallClient={onInstallClient}
        onOpenGuide={onOpenGuide}
      />,
    );
  });
}

beforeEach(() => {
  vi.clearAllMocks();
});

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

describe("ChatConnectionPanel Secure Tunnel entry", () => {
  it("shows official client resources and opens the in-app setup Guide when the client is missing", async () => {
    await renderPanel();

    expect(host?.textContent).toContain("OpenAI tunnel-client was not detected");
    expect(host?.textContent).toContain("Install verified official client");
    expect(onInstallClient).not.toHaveBeenCalled();
    const installButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Install verified official client"));
    expect(installButton).toBeTruthy();
    await act(async () => installButton?.dispatchEvent(new MouseEvent("click", { bubbles: true })));
    expect(onInstallClient).toHaveBeenCalledTimes(1);
    expect(host?.querySelector('a[href="https://platform.openai.com/settings/organization/tunnels"]')).toBeTruthy();
    expect(host?.querySelector('a[href="https://github.com/openai/tunnel-client/releases/latest"]')).toBeTruthy();

    const guideButton = Array.from(host?.querySelectorAll("button") ?? [])
      .find((button) => button.textContent?.includes("Open setup Guide"));
    expect(guideButton).toBeTruthy();

    await act(async () => {
      guideButton?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    expect(onOpenGuide).toHaveBeenCalledTimes(1);
  });

  it("does not offer the unnotarized client ZIP installer on macOS", async () => {
    const previousAgent = navigator.userAgent;
    Object.defineProperty(navigator, "userAgent", {
      configurable: true, value: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)",
    });
    try {
      await renderPanel();
      expect(host?.textContent).toContain("Homebrew");
      expect(host?.textContent).toContain("Gatekeeper");
      expect(host?.textContent).toContain("brew install openai/tools/tunnel-client");
      const installButton = Array.from(host?.querySelectorAll("button") ?? [])
        .find((button) => button.textContent?.includes("Install verified official client"));
      expect(installButton).toBeUndefined();
      expect(onInstallClient).not.toHaveBeenCalled();
    } finally {
      Object.defineProperty(navigator, "userAgent", {
        configurable: true, value: previousAgent,
      });
    }
  });

  it("keeps a failed tunnel startup message visible after the client process exits", async () => {
    await renderPanel({
      ...unavailableStatus,
      clientAvailable: true,
      clientVersion: "tunnel-client version (test)",
      message: "Secure MCP Tunnel is not ready yet (HTTP 503): authentication is required.",
    });

    expect(host?.textContent).toContain("Last connection attempt");
    expect(host?.textContent).toContain("HTTP 503");
    expect(host?.textContent).toContain("authentication is required");
  });

  it("keeps disconnect controls visible if the executable disappears while its process is running", async () => {
    await renderPanel({
      ...unavailableStatus,
      running: true,
      ready: false,
      tunnelId: "tunnel_0123456789abcdef0123456789abcdef",
      message: "Tunnel process is still running.",
    });

    expect(host?.textContent).not.toContain("OpenAI tunnel-client was not detected");
    expect(host?.textContent).toContain("Disconnect OpenAI tunnel");
  });

  it("only pauses the active OpenAI tunnel and still offers full disconnect", async () => {
    await renderPanel({
      ...unavailableStatus,
      clientAvailable: true,
      running: true,
      ready: true,
      tunnelId: "tunnel_0123456789abcdef0123456789abcdef",
    });
    const pause = Array.from(host?.querySelectorAll("button") ?? [])
      .find(button => button.textContent?.includes("Pause OpenAI tunnel"));
    expect(pause).toBeTruthy();
    await act(async () => pause?.click());
    expect(onPause).toHaveBeenCalledTimes(1);
    expect(onResume).not.toHaveBeenCalled();
    expect(onDisconnect).not.toHaveBeenCalled();
  });

  it("resumes a paused tunnel without exposing saved credentials or touching public providers", async () => {
    await renderPanel({
      ...unavailableStatus,
      clientAvailable: true,
      paused: true,
      tunnelId: "tunnel_0123456789abcdef0123456789abcdef",
    });
    expect(host?.textContent).toContain("Direct HTTPS and the public MCP endpoint are unaffected");
    expect(host?.textContent).toContain("No key is written to RepoTunnel settings");
    expect(host?.textContent).not.toContain("Runtime API key");
    const resume = Array.from(host?.querySelectorAll("button") ?? [])
      .find(button => button.textContent?.includes("Resume OpenAI tunnel"));
    await act(async () => resume?.click());
    expect(onResume).toHaveBeenCalledTimes(1);
    expect(onConnect).not.toHaveBeenCalled();
    expect(onDisconnect).not.toHaveBeenCalled();
    const forget = Array.from(host?.querySelectorAll("button") ?? [])
      .find(button => button.textContent?.includes("Disconnect and forget key"));
    await act(async () => forget?.click());
    expect(onDisconnect).toHaveBeenCalledTimes(1);
  });

  it("lets the user explicitly save the connected key in the OS keyring", async () => {
    await renderPanel({
      ...unavailableStatus,
      clientAvailable: true,
      running: true,
      ready: true,
      tunnelId: "tunnel_0123456789abcdef0123456789abcdef",
    });
    const remember = Array.from(host?.querySelectorAll("button") ?? [])
      .find(button => button.textContent?.includes("Remember in system keyring"));
    expect(remember).toBeTruthy();
    await act(async () => remember?.click());
    expect(onRemember).toHaveBeenCalledTimes(1);
    expect(onConnect).not.toHaveBeenCalled();
  });

  it("does not offer OS keyring saving on unsupported platforms", async () => {
    await renderPanel({
      ...unavailableStatus,
      clientAvailable: true,
      running: true,
      ready: true,
      rememberSupported: false,
      tunnelId: "tunnel_0123456789abcdef0123456789abcdef",
    });
    expect(host?.textContent).not.toContain("Remember in system keyring");
    expect(onRemember).not.toHaveBeenCalled();
  });

  it("reopens a saved tunnel paused and offers Resume without a secret input", async () => {
    await renderPanel({
      ...unavailableStatus,
      clientAvailable: true,
      paused: true,
      remembered: true,
      tunnelId: "tunnel_0123456789abcdef0123456789abcdef",
    });
    expect(host?.textContent).toContain("saved only in your Linux system keyring");
    expect(host?.querySelector('input[type="password"]')).toBeNull();
    const resume = Array.from(host?.querySelectorAll("button") ?? [])
      .find(button => button.textContent?.includes("Resume OpenAI tunnel"));
    await act(async () => resume?.click());
    expect(onResume).toHaveBeenCalledTimes(1);
    expect(onConnect).not.toHaveBeenCalled();
  });

  it("explains tunnel-local token authentication without directing users to public HTTPS OAuth", async () => {
    await renderPanel({
      ...unavailableStatus,
      clientAvailable: true,
      clientVersion: "tunnel-client (test)",
      running: true,
      ready: true,
      tunnelId: "tunnel_0123456789abcdef0123456789abcdef",
    });
    expect(host?.textContent).toContain("Connection: Tunnel");
    expect(host?.textContent).toContain("No authentication");
    expect(host?.textContent).toContain("Direct HTTPS OAuth");
    expect(host?.textContent).toContain("Disconnect OpenAI tunnel");
  });
});
