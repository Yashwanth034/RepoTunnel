import type { EditorDocument } from "../components/WorkspaceEditor";
import type { Workspace } from "../types";
import { mapBounded } from "./asyncWork";

export type SavedEditorTab = {
  workspaceId: string;
  path: string;
  dirty: boolean;
  draftContent?: string;
  savedContent?: string;
};

/**
 * Restore the bounded persisted editor tab set without serial IPC per file.
 * Results remain in tab order even if files load in a different order.
 * Each loader call must check the requested file's type/access once.
 */
export async function restoreEditorDocuments(
  savedTabs: readonly SavedEditorTab[],
  workspaces: readonly Workspace[],
  load: (workspace: Workspace, path: string) => Promise<EditorDocument>,
  maxTabs = 16,
): Promise<EditorDocument[]> {
  const approved = new Map(workspaces.map((workspace) => [workspace.id, workspace]));
  const result = await mapBounded(savedTabs.slice(0, maxTabs), async (saved) => {
    const workspace = approved.get(saved.workspaceId);
    if (!workspace || typeof saved.path !== "string" || !saved.path) return null;
    try {
      let document = await load(workspace, saved.path);
      if (document.kind === "text" && saved.dirty
        && typeof saved.draftContent === "string"
        && typeof saved.savedContent === "string") {
        if (saved.savedContent === document.savedContent) {
          document = {
            ...document, content: saved.draftContent,
            dirty: saved.draftContent !== document.savedContent,
          };
        } else if (saved.draftContent === document.content) {
          document = { ...document, dirty: false };
        } else {
          // Both local and disk content changed: keep the draft and surface
          // the external version. Never silently replace unsaved work.
          document = {
            ...document,
            content: saved.draftContent,
            savedContent: saved.savedContent,
            dirty: true,
            externalContent: document.content,
            externalModifiedAt: document.modifiedAt,
            conflict: true,
          };
        }
      }
      return document;
    } catch {
      // Deleted, inaccessible, or changed-type files do not prevent the
      // remaining approved editor tabs from restoring.
      return null;
    }
  }, 4);
  return result.filter((document): document is EditorDocument => document !== null);
}
