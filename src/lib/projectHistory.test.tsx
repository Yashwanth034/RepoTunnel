// @vitest-environment jsdom
import React, { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ActivityTimeline, VersionTimeline } from "../types";
import { useProjectHistory } from "./projectHistory";

(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

function deferred<T>() {
  let resolve!: (result: T) => void;
  let reject!: (error: Error) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const versions = (project: string): VersionTimeline => ({
  records: [], currentVersionId: "version-" + project,
});
const activity = (project: string): ActivityTimeline => ({
  groups: [{ id: "activity-" + project }] as unknown as ActivityTimeline["groups"],
});
let host: HTMLDivElement | null;
let root: Root | null;
let refresh: () => Promise<void>;
let refreshVersions: () => Promise<void>;
let refreshActivity: () => Promise<void>;
const readVersions = vi.fn<(id: string) => Promise<VersionTimeline>>();
const readActivity = vi.fn<(id: string) => Promise<ActivityTimeline>>();

function Fixture({ workspaceId }: { workspaceId: string | null }) {
  const view = useProjectHistory(workspaceId, readVersions, readActivity);
  refresh = view.refresh;
  refreshVersions = view.refreshVersions;
  refreshActivity = view.refreshActivity;
  return (
    <div>
      <output data-testid="version">{view.versionTimeline.currentVersionId ?? "empty"}</output>
      <output data-testid="activity">{view.activityTimeline.groups.length
        ? (view.activityTimeline.groups[0] as { id: string }).id : "empty"}</output>
    </div>
  );
}
async function show(workspaceId: string | null) {
  await act(async () => root?.render(<Fixture workspaceId={workspaceId} />));
}
const observed = () => host?.textContent ?? "";

beforeEach(() => {
  vi.clearAllMocks();
  readVersions.mockImplementation(async id => versions(id));
  readActivity.mockImplementation(async id => activity(id));
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
});
afterEach(async () => {
  if (root) await act(async () => root?.unmount());
  root = null;
  host?.remove();
  host = null;
});

describe("project-scoped History read-only refresh", () => {
  it("hides project A immediately upon switching to B, and ignores A completing last", async () => {
    const oldVersions = deferred<VersionTimeline>();
    const oldActivity = deferred<ActivityTimeline>();
    readVersions.mockImplementation(id => id === "A" ? oldVersions.promise : Promise.resolve(versions(id)));
    readActivity.mockImplementation(id => id === "A" ? oldActivity.promise : Promise.resolve(activity(id)));

    await show("A");
    let first!: Promise<void>;
    await act(async () => { first = refresh(); });
    await show("B");
    expect(observed()).toBe("emptyempty");
    await act(async () => { await refresh(); });
    expect(observed()).toContain("version-B");
    expect(observed()).toContain("activity-B");
    await act(async () => { oldVersions.resolve(versions("A")); oldActivity.resolve(activity("A")); });
    await first;
    expect(observed()).toContain("version-B");
    expect(observed()).not.toContain("version-A");
    expect(observed()).not.toContain("activity-A");
  });

  it("the latest manual history refresh wins over a slow earlier refresh for the same project", async () => {
    const held = deferred<VersionTimeline>();
    const heldActivity = deferred<ActivityTimeline>();
    readVersions.mockImplementationOnce(() => held.promise)
      .mockResolvedValueOnce(versions("new"));
    readActivity.mockImplementationOnce(() => heldActivity.promise)
      .mockResolvedValueOnce(activity("new"));
    await show("A");
    let old!: Promise<void>;
    await act(async () => { old = refresh(); });
    await act(async () => { await refresh(); });
    expect(observed()).toContain("version-new");
    expect(observed()).toContain("activity-new");
    await act(async () => { held.resolve(versions("old")); heldActivity.resolve(activity("old")); });
    await old;
    expect(observed()).not.toContain("version-old");
    expect(observed()).toContain("version-new");
  });

  it("an activity-only event wins over a slower combined version+activity read", async () => {
    const slowVersions = deferred<VersionTimeline>();
    const slowActivity = deferred<ActivityTimeline>();
    readVersions.mockImplementationOnce(() => slowVersions.promise);
    readActivity.mockImplementationOnce(() => slowActivity.promise)
      .mockResolvedValueOnce(activity("event-new"));
    await show("A");
    let older!: Promise<void>;
    await act(async () => { older = refresh(); });
    await act(async () => { await refreshActivity(); });
    expect(observed()).toContain("activity-event-new");
    await act(async () => {
      slowVersions.resolve(versions("A"));
      slowActivity.resolve(activity("old-event"));
    });
    await older;
    expect(observed()).toContain("version-A");
    expect(observed()).toContain("activity-event-new");
    expect(observed()).not.toContain("activity-old-event");
  });

  it("version-only refresh succeeds despite an unavailable unrelated activity reader", async () => {
    await show("A");
    await act(async () => { await refresh(); });
    readActivity.mockRejectedValue(new Error("activity backend temporarily offline"));
    readVersions.mockResolvedValueOnce(versions("restored"));
    await act(async () => { await refreshVersions(); });
    expect(observed()).toContain("version-restored");
    expect(observed()).toContain("activity-A");
    expect(readActivity).toHaveBeenCalledTimes(1);
  });

  it("clears visible history immediately when no project is selected", async () => {
    await show("A");
    await act(async () => { await refresh(); });
    expect(observed()).toContain("version-A");
    await show(null);
    expect(observed()).toBe("emptyempty");
    await act(async () => { await refresh(); await refreshActivity(); });
    expect(observed()).toBe("emptyempty");
    expect(readVersions).toHaveBeenCalledTimes(1);
    expect(readActivity).toHaveBeenCalledTimes(1);
  });

  it("preserves the last successful snapshot when a transient history request fails", async () => {
    await show("A");
    await act(async () => { await refresh(); });
    readVersions.mockRejectedValueOnce(new Error("backend busy"));
    await act(async () => {
      await expect(refresh()).rejects.toThrow("backend busy");
    });
    expect(observed()).toContain("version-A");
    expect(observed()).toContain("activity-A");
  });
});
