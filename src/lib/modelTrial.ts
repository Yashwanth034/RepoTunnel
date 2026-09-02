import type { ModelSelection } from "../types";

export function modelTrialKey(selection: Pick<ModelSelection, "provider" | "modelId" | "endpoint">): string {
  return `${selection.provider}::${selection.endpoint}::${selection.modelId}`;
}

export function sameModelIdentity(
  selection: Pick<ModelSelection, "provider" | "modelId" | "endpoint">,
  identity: Pick<ModelSelection, "provider" | "modelId" | "endpoint"> | null,
): boolean {
  return Boolean(identity)
    && selection.provider === identity!.provider
    && selection.modelId === identity!.modelId
    && selection.endpoint === identity!.endpoint;
}
