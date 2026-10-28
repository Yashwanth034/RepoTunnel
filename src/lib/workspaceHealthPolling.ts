import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { WorkspaceHealth } from "../types";
import { mapBounded } from "./asyncWork";

export type HealthWorkspace = { id: string; path: string };
export type WorkspaceHealthReader = (workspaceId: string) => Promise<WorkspaceHealth>;

/**
 * Read all project-health values without dispatching unbounded concurrent
 * desktop IPC calls. Preserve input order; errors are per project.
 */
export async function collectWorkspaceHealth(
  workspaces: readonly HealthWorkspace[],
  read: WorkspaceHealthReader,
  concurrency = 4,
): Promise<Record<string, WorkspaceHealth>> {
  if (!workspaces.length) return {};
  const output = await mapBounded(workspaces, async ({ id: workspaceId }): Promise<[string, WorkspaceHealth]> => {
    try {
      return [workspaceId, await read(workspaceId)];
    } catch (error) {
      return [workspaceId, {
        workspaceId, available: false,
        message: error instanceof Error ? error.message : String(error),
      }];
    }
  }, concurrency);
  return Object.fromEntries(output);
}

/** Avoid publishing a new map when periodic health values are identical. */
export function reconcileWorkspaceHealth(
  previous: Record<string, WorkspaceHealth>,
  incoming: Record<string, WorkspaceHealth>,
): Record<string, WorkspaceHealth> {
  const ids = Object.keys(incoming);
  if (ids.length !== Object.keys(previous).length) return incoming;
  for (const id of ids) {
    const before = previous[id];
    const after = incoming[id];
    if (!before || before.workspaceId !== after.workspaceId ||
        before.available !== after.available || before.message !== after.message) {
      return incoming;
    }
  }
  return previous;
}

/**
 * Keeps sidebar/project availability fresh without a health-check burst.
 * A manual refresh supersedes an older poll; switching projects invalidates
 * the previous result even if the older native IPC calls finish later.
 */
export function useWorkspaceHealthPolling(
  workspaces: readonly HealthWorkspace[],
  read: WorkspaceHealthReader,
  intervalMs = 30_000,
): {
  health: Record<string, WorkspaceHealth>;
  refresh: () => Promise<void>;
  update: (workspaceId: string, status: WorkspaceHealth) => void;
} {
  const [health, setHealth] = useState<Record<string, WorkspaceHealth>>({});
  const workspacesRef = useRef(workspaces);
  workspacesRef.current = workspaces;
  const latestRequestRef = useRef(0);
  const signature = useMemo(() => JSON.stringify(workspaces.map(({ id, path }) => [id, path])), [workspaces]);

  const refresh = useCallback(async () => {
    const request = ++latestRequestRef.current;
    const next = await collectWorkspaceHealth(workspacesRef.current, read);
    if (request !== latestRequestRef.current) return;
    setHealth((current) => reconcileWorkspaceHealth(current, next));
  }, [read]);

  const update = useCallback((workspaceId: string, status: WorkspaceHealth) => {
    // A user-requested health retry takes priority over an earlier poll.
    latestRequestRef.current++;
    setHealth((current) => reconcileWorkspaceHealth(current, {
      ...current,
      [workspaceId]: status,
    }));
  }, []);

  useEffect(() => {
    let active = true;
    let pending = false;
    const poll = async () => {
      if (!active || pending) return;
      pending = true;
      try {
        await refresh();
      } finally {
        pending = false;
      }
    };
    void poll();
    const timer = window.setInterval(() => void poll(), intervalMs);
    return () => {
      active = false;
      latestRequestRef.current++;
      window.clearInterval(timer);
    };
  }, [signature, refresh, intervalMs]);

  return { health, refresh, update };
}
