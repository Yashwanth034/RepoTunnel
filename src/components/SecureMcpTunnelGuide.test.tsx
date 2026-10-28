// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it } from "vitest";
import SecureMcpTunnelGuide from "./SecureMcpTunnelGuide";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function renderGuide() {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => {
    root?.render(<SecureMcpTunnelGuide />);
  });
}

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

describe("SecureMcpTunnelGuide", () => {
  it("renders the ordered setup, security notes, and official OpenAI resources", async () => {
    await renderGuide();
    const text = host?.textContent ?? "";

    expect(text).toContain("Connect ChatGPT with OpenAI Secure MCP Tunnel");
    expect(text).toContain("Create or select a tunnel");
    expect(text).toContain("Install the official tunnel-client");
    expect(text).toContain("Create a restricted runtime API key");
    expect(text).toContain("Connect from RepoTunnel");
    expect(text).toContain("Tunnels Read + Manage");
    expect(text).toContain("Tunnels Read + Use");
    expect(text).toContain("The tunnel ID is not a secret");
    expect(text).toContain("Homebrew");
    expect(text).toContain("Gatekeeper");
    expect(text).toContain("run-only");
    expect(text).toContain("No inbound port required");
    expect(text).toContain("No authentication");
    expect(text).toContain("process-bound");
    expect(text).toContain("Review the security warning");
    expect(text).toContain("create the plugin");
    expect(text).toContain("MCP listener stays on this computer");
    expect(text).toContain("still pass through OpenAI's hosted tunnel");
    expect(text).toContain("all MCP traffic must remain entirely local");

    expect(host?.querySelector('a[href="https://platform.openai.com/settings/organization/tunnels"]')).toBeTruthy();
    expect(host?.querySelector('a[href="https://platform.openai.com/settings/organization/api-keys"]')).toBeTruthy();
    expect(host?.querySelector('a[href="https://github.com/openai/tunnel-client/releases/latest"]')).toBeTruthy();
    expect(host?.querySelector('a[href="https://developers.openai.com/api/docs/guides/secure-mcp-tunnels"]')).toBeTruthy();
  });

  it("labels the two UI illustrations as samples rather than real screenshots", async () => {
    await renderGuide();

    expect(host?.querySelectorAll(".secure-tunnel-sample")).toHaveLength(2);
    expect(host?.textContent).toContain("Illustrative sample screens—not live captures");
    expect(host?.textContent).toContain("Connection");
    expect(host?.textContent).toContain("Tunnel");
  });

  it("makes clear the existing public connection options remain optional alternatives", async () => {
    await renderGuide();
    const text = host?.textContent ?? "";

    expect(text).toContain("Direct HTTPS");
    expect(text).toContain("ngrok");
    expect(text).toContain("Cloudflare");
    expect(text).toContain("This guide does not enable, disable, or change those options.");
  });
});
