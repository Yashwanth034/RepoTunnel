// @vitest-environment jsdom

import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import AppHeader from "./AppHeader";
import AppSidebar from "./AppSidebar";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

describe("Guide navigation label", () => {
  let root: Root | null = null;
  let host: HTMLDivElement | null = null;

  afterEach(async () => {
    if (root) await act(async () => root?.unmount());
    root = null;
    host?.remove();
    host = null;
  });

  it("uses Guide for the existing setup view in both sidebar and page header", async () => {
    host = document.createElement("div");
    document.body.append(host);
    root = createRoot(host);

    await act(async () => {
      root?.render(
        <>
          <AppSidebar activeView="httpsSetup" pendingCount={0} onNavigate={vi.fn()} />
          <AppHeader
            view="httpsSetup"
            gatewayRunning={false}
            connectionReady={false}
            aiAccessPaused={false}
            focusMode={false}
            onToggleFocusMode={vi.fn()}
          />
        </>,
      );
    });

    const activeNavigation = host.querySelector(".sidebar-nav-item.active");
    expect(activeNavigation?.textContent).toContain("Guide");
    expect(host.querySelector(".page-header h1")?.textContent).toBe("Guide");
    expect(activeNavigation?.textContent).not.toContain("HTTPS Setup");
  });
});
