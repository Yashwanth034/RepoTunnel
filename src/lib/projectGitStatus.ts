import { useEffect, useMemo, useRef, useState } from "react";
import type { GitRepositoryStatus } from "../types";
import { coalesceAsync } from "./asyncWork";
import { preserveGitStatus } from "./gitSnapshot";

export type GitStatusReader = (workspaceId: string) => Promise<GitRepositoryStatus>;

/**
 * App shell project-rail Git state, independent of the Git screen's own UI.
 *
 * The shell may poll, refresh after file changes, or switch projects during
 * a slow read. Never publish an old project's status under the new project.
 */
export function useProjectGitStatus(
  workspaceId: string | null,
  read: GitStatusReader,
): { status: GitRepositoryStatus | null; refresh: () => Promise<void> } {
  const [snapshot, setSnapshot] = useState<{
    workspaceId: string | null;
    status: GitRepositoryStatus | null;
  }>({ workspaceId: null, status: null });
  const currentWorkspaceRef = useRef(workspaceId);
  currentWorkspaceRef.current = workspaceId;
  const mountedRef = useRef(false);

  useEffect(() => {
    mountedRef.current = true;
    return () => { mountedRef.current = false; };
  }, []);

  const refresh = useMemo(() => coalesceAsync(async () => {
    if (!workspaceId) {
      if (mountedRef.current) setSnapshot((current) =>
        current.workspaceId === null && current.status === null
          ? current : { workspaceId: null, status: null });
      return;
    }
    try {
      const next = await read(workspaceId);
      if (!mountedRef.current || currentWorkspaceRef.current !== workspaceId) return;
      setSnapshot((current) => {
        const status = preserveGitStatus(
          current.workspaceId === workspaceId ? current.status : null,
          next,
        );
        return current.workspaceId === workspaceId && current.status === status
          ? current : { workspaceId, status };
      });
    } catch {
      if (!mountedRef.current || currentWorkspaceRef.current !== workspaceId) return;
      setSnapshot((current) => current.workspaceId === workspaceId && current.status === null
        ? current : { workspaceId, status: null });
    }
  }), [workspaceId, read]);

  // An old project's data is invisible on the first render after a switch,
  // even before any effects or a new backend request have completed.
  return {
    status: snapshot.workspaceId === workspaceId ? snapshot.status : null,
    refresh,
  };
}
