import { describe, expect, it } from "vitest";
import { modelTrialKey, sameModelIdentity } from "./modelTrial";

const selection = {
  provider: "ollama" as const,
  modelId: "qwen-test",
  endpoint: "http://127.0.0.1:11434",
};

describe("Model Trial identity helpers", () => {
  it("uses provider, endpoint and model id as the stable trial identity", () => {
    expect(modelTrialKey(selection)).toBe("ollama::http://127.0.0.1:11434::qwen-test");
  });

  it("matches only the exact selected local model identity", () => {
    expect(sameModelIdentity(selection, selection)).toBe(true);
    expect(sameModelIdentity(selection, { ...selection, modelId: "other" })).toBe(false);
    expect(sameModelIdentity(selection, null)).toBe(false);
  });
});
