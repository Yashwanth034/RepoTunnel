// @ts-expect-error -- Vitest runs in Node while the app tsconfig intentionally omits Node typings.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const productionSource = readFileSync(new URL("./ProductionPanel.tsx", import.meta.url), "utf8");
const executionSource = readFileSync(new URL("./ExecutionPanel.tsx", import.meta.url), "utf8");

describe("Custom AI applications placement", () => {
  it("keeps custom AI applications in Commands instead of Settings", () => {
    expect(executionSource).toContain('import CustomApplicationsPanel from "./custom-applications/CustomApplicationsPanel";');
    expect(executionSource).toContain("<CustomApplicationsPanel");

    expect(productionSource).not.toContain('import CustomApplicationsPanel from "./custom-applications/CustomApplicationsPanel";');
    expect(productionSource).not.toContain("<CustomApplicationsPanel");
  });
});
