import { useCallback, useRef, useState } from "react";
import type { ActivityTimeline, VersionTimeline } from "../types";

const EMPTY_VERSIONS: VersionTimeline = { records: [], currentVersionId: null };
const EMPTY_ACTIVITY: ActivityTimeline = { groups: [] };

type ProjectHistoryState = {
  workspaceId: string | null;
  versions: VersionTimeline;
  activity: ActivityTimeline;
};

/**
 * Read-only history view snapshot, scoped to the selected approved project.
 *
 * All callers (initial load, changes events, activity events, explicit refresh,
 * restore/clear follow-up) share request revisions. A late project A read must
 * never be published on project B. A late full read cannot overwrite a newer
 * activity-only event, even when both target the same project.
 */
export function useProjectHistory(
  workspaceId: string | null,
  readVersions: (id: string) => Promise<VersionTimeline>,
  readActivity: (id: string) => Promise<ActivityTimeline>,
): {
  versionTimeline: VersionTimeline;
  activityTimeline: ActivityTimeline;
  refresh: () => Promise<void>;
  refreshVersions: () => Promise<void>;
  refreshActivity: () => Promise<void>;
} {
  const [state, setState] = useState<ProjectHistoryState>({
    workspaceId: null, versions: EMPTY_VERSIONS, activity: EMPTY_ACTIVITY,
  });
  const currentWorkspace = useRef(workspaceId);
  currentWorkspace.current = workspaceId;
  const versionEpoch = useRef(0);
  const activityEpoch = useRef(0);

  const refresh = useCallback(async () => {
    const id = workspaceId;
    const versionRequest = ++versionEpoch.current;
    const activityRequest = ++activityEpoch.current;
    if (!id) {
      setState((current) => current.workspaceId === null ? current : {
        workspaceId: null, versions: EMPTY_VERSIONS, activity: EMPTY_ACTIVITY,
      });
      return;
    }
    const [versions, activity] = await Promise.all([
      readVersions(id), readActivity(id),
    ]);
    if (currentWorkspace.current !== id) return;
    setState((current) => {
      if (currentWorkspace.current !== id) return current;
      const takeVersions = versionEpoch.current === versionRequest;
      const takeActivity = activityEpoch.current === activityRequest;
      if (!takeVersions && !takeActivity) return current;
      return {
        workspaceId: id,
        versions: takeVersions ? versions : current.workspaceId === id ? current.versions : EMPTY_VERSIONS,
        activity: takeActivity ? activity : current.workspaceId === id ? current.activity : EMPTY_ACTIVITY,
      };
    });
  }, [workspaceId, readVersions, readActivity]);

  const refreshVersions = useCallback(async () => {
    const id = workspaceId;
    const request = ++versionEpoch.current;
    if (!id) return;
    const versions = await readVersions(id);
    if (currentWorkspace.current !== id || request !== versionEpoch.current) return;
    setState((current) => {
      if (currentWorkspace.current !== id || request !== versionEpoch.current) return current;
      return {
        workspaceId: id,
        versions,
        activity: current.workspaceId === id ? current.activity : EMPTY_ACTIVITY,
      };
    });
  }, [workspaceId, readVersions]);

  const refreshActivity = useCallback(async () => {
    const id = workspaceId;
    const request = ++activityEpoch.current;
    if (!id) return;
    const activity = await readActivity(id);
    if (currentWorkspace.current !== id || request !== activityEpoch.current) return;
    setState((current) => {
      if (currentWorkspace.current !== id || request !== activityEpoch.current) return current;
      return {
        workspaceId: id,
        versions: current.workspaceId === id ? current.versions : EMPTY_VERSIONS,
        activity,
      };
    });
  }, [workspaceId, readActivity]);

  // Do not expose the old project's timeline even for one render after switch,
  // before effects or asynchronous data loads complete.
  const visible = state.workspaceId === workspaceId;
  return {
    versionTimeline: visible ? state.versions : EMPTY_VERSIONS,
    activityTimeline: visible ? state.activity : EMPTY_ACTIVITY,
    refresh,
    refreshVersions,
    refreshActivity,
  };
}
