// @ts-expect-error -- Vitest runs in Node while the app tsconfig intentionally omits Node typings.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const homeSource = readFileSync(new URL("./HomeWorkspace.tsx", import.meta.url), "utf8");
const workspaceCss = readFileSync(new URL("../workspace.css", import.meta.url), "utf8");

describe("Home workspace branding", () => {
  it("keeps the RepoTunnel brand centered without rendering the local Home chat", () => {
    expect(homeSource).not.toContain('import HomeChat from "./HomeChat";');
    expect(homeSource).not.toContain("<HomeChat");
    expect(homeSource).toContain('className="home-chat-empty-brand home-home-brand"');
    expect(homeSource).toContain("<span>RepoTunnel</span>");
    expect(workspaceCss).toContain(".home-home-brand");
    expect(workspaceCss).toContain("padding: 0;");
  });
});
