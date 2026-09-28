// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const backend = vi.hoisted(() => ({
  configurePublicTunnel: vi.fn(),
  getHttpsSetupReadiness: vi.fn(),
  installHttpsSetupWireguardConfig: vi.fn(),
  openHttpsSetupResource: vi.fn(),
  provisionDirectHttpsCertificate: vi.fn(),
  verifyHttpsSetupHostname: vi.fn(),
}));

vi.mock("../lib/backend", () => backend);

import HttpsSetupGuide from "./HttpsSetupGuide";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root | null = null;
let host: HTMLDivElement | null = null;

const readiness = {
  supportedPlatform: true,
  wireguardInstalled: true,
  wgQuickInstalled: true,
  nftablesInstalled: true,
  opensslInstalled: true,
  certbotReady: true,
  pipxInstalled: true,
  systemdAvailable: true,
  nativeGlobalIpv6Available: false,
  wireguardInterfaceActive: false,
  standardWireguardConfigPresent: false,
  standardWireguardServiceActive: false,
  nftablesRulesReadable: false,
  nftablesRulesPresent: false,
  pkexecAvailable: true,
  directHttpsConfigured: false,
  directHttpsLocalReady: false,
  directHttpsTlsTrusted: false,
  directHttpsPublicReachable: false,
};

async function settle() {
  await act(async () => {
    await new Promise((resolve) => window.setTimeout(resolve, 0));
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  backend.getHttpsSetupReadiness.mockResolvedValue(readiness);
  backend.openHttpsSetupResource.mockResolvedValue(undefined);
  backend.verifyHttpsSetupHostname.mockResolvedValue({
    validHostname: false,
    dnsResolves: false,
    ipv4Available: false,
    ipv6Available: false,
    healthReachable: false,
    tlsTrusted: false,
    oauthResourceMetadataReachable: false,
    oauthServerMetadataReachable: false,
  });
});

afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

async function renderGuide() {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () => {
    root?.render(<HttpsSetupGuide onError={() => undefined} />);
  });
  await settle();
}

describe("HTTPS Setup privacy and guided resources", () => {
  it("shows only abstract setup status with a generic hostname example", async () => {
    await renderGuide();

    const text = host?.textContent ?? "";
    const input = host?.querySelector('input[aria-label="Direct HTTPS hostname"]') as HTMLInputElement;

    expect(text).toContain("HTTPS Setup");
    expect(text).toContain("Public IPv6 path");
    expect(input.placeholder).toBe("your-name.duckdns.org");

    expect(text).not.toContain("https://");
    expect(text).not.toContain("/mcp");
    expect(text).not.toContain("43182");
    expect(text).not.toContain("43183");
    expect(text).not.toContain("43184");
    expect(text).not.toContain("203.0.113.42");
    expect(text).not.toContain("private-user-host.example");
  });

  it("opens allowlisted setup resources without passing a raw URL from the UI", async () => {
    await renderGuide();

    const button = Array.from(host?.querySelectorAll("button") ?? [])
      .find((item) => item.textContent?.includes("Open Route64")) as HTMLButtonElement;

    await act(async () => button.click());

    expect(backend.openHttpsSetupResource).toHaveBeenCalledTimes(1);
    expect(backend.openHttpsSetupResource).toHaveBeenCalledWith("route64");
  });
});
