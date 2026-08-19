import { useEffect, useMemo, useState } from "react";
import type {
  ActivityGroup,
  ActivityKind,
  ActivityStatus,
  ActivityTimeline,
  VersionRecord,
  VersionTimeline,
} from "../types";
import ConfirmationDialog from "./ConfirmationDialog";

const PAGE_SIZE = 20;

const operationLabels: Record<string, string> = {
  createFile: "New file",
  writeFile: "Updated",
  patchFile: "Updated",
  createDirectory: "New folder",
  renameEntry: "Renamed",
  moveEntry: "Moved",
  deleteEntry: "Deleted",
};

const activityLabels: Record<ActivityKind, string> = {
  files: "Files",
  terminal: "Terminal",
  process: "Process",
  launcher: "App",
  browser: "Browser",
  git: "Git",
  monitoring: "Monitor",
  verification: "Verify",
  team: "Team",
};

type HistoryFilter = "all" | ActivityKind;
type HistorySort = "newest" | "oldest";
type RestoreTarget = { kind: "version"; version: VersionRecord } | { kind: "original" };
type TimelineEntry =
  | { kind: "activity"; group: ActivityGroup; versions: VersionRecord[]; timestamp: number }
  | { kind: "version"; version: VersionRecord; timestamp: number };

function formatTime(timestamp: number): string {
  return new Intl.DateTimeFormat(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

function dateGroupLabel(timestamp: number): string {
  const value = new Date(timestamp);
  const today = new Date();
  const todayStart = new Date(today.getFullYear(), today.getMonth(), today.getDate()).getTime();
  const valueStart = new Date(value.getFullYear(), value.getMonth(), value.getDate()).getTime();
  const dayDifference = Math.round((todayStart - valueStart) / 86_400_000);
  if (dayDifference === 0) return "Today";
  if (dayDifference === 1) return "Yesterday";
  return new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", year: "numeric" }).format(value);
}

function basename(path: string): string {
  const normalized = path.replace(/\\/g, "/").replace(/\/$/, "");
  return normalized.split("/").filter(Boolean).pop() ?? path;
}

function groupStatus(group: ActivityGroup): ActivityStatus {
  const statuses = group.events.map((event) => event.status);
  if (statuses.includes("pending")) return "pending";
  if (statuses.includes("running")) return "running";

  const completed = statuses.filter((status) => status !== "observed");
  if (completed.length === 0) return "observed";
  if (completed.every((status) => status === "failed")) return "failed";
  if (completed.every((status) => status === "rejected")) return "rejected";
  if (completed.every((status) => status === "stopped")) return "stopped";

  // The group badge represents whether the AI request is still active, not whether
  // every individual verification succeeded. Failed/rejected sub-actions remain
  // visible on their own rows while a completed mixed request is shown as Done.
  return "succeeded";
}

function statusLabel(status: ActivityStatus): string {
  if (status === "succeeded") return "Done";
  if (status === "observed") return "Observed";
  return status.charAt(0).toUpperCase() + status.slice(1);
}

function activityTitle(group: ActivityGroup, versions: VersionRecord[]): string {
  const newestVersion = versions.slice().sort((a, b) => b.updatedAt - a.updatedAt)[0];
  if (newestVersion) return newestVersion.summary;
  const significant = group.events
    .slice()
    .reverse()
    .find((event) => event.status !== "observed");
  return significant?.summary ?? group.events.at(-1)?.summary ?? group.summary;
}

function entryMatches(entry: TimelineEntry, filter: HistoryFilter, query: string): boolean {
  if (filter !== "all") {
    if (entry.kind === "version") {
      if (filter !== "files") return false;
    } else if (filter === "files") {
      if (entry.versions.length === 0 && !entry.group.events.some((event) => event.kind === "files")) return false;
    } else if (!entry.group.events.some((event) => event.kind === filter)) {
      return false;
    }
  }

  if (!query) return true;
  const searchable = entry.kind === "version"
    ? [
        entry.version.summary,
        ...entry.version.files.flatMap((file) => [file.summary, file.primaryPath, file.secondaryPath ?? ""]),
      ]
    : [
        entry.group.summary,
        ...entry.group.events.flatMap((event) => [event.summary, event.detail ?? "", event.action, activityLabels[event.kind]]),
        ...entry.versions.flatMap((version) => [
          version.summary,
          ...version.files.flatMap((file) => [file.summary, file.primaryPath, file.secondaryPath ?? ""]),
        ]),
      ];
  return searchable.join(" ").toLowerCase().includes(query);
}

function FileDetails({ version }: { version: VersionRecord }) {
  return (
    <details className="change-details version-files">
      <summary>View {version.files.length === 1 ? "file" : `${version.files.length} files`}</summary>
      <div className="version-file-list">
        {version.files.map((file, index) => (
          <details className="version-file" key={`${version.id}:${file.primaryPath}:${index}`}>
            <summary>
              <span className="version-file-name">
                {file.secondaryPath
                  ? `${basename(file.primaryPath)} → ${basename(file.secondaryPath)}`
                  : basename(file.primaryPath)}
              </span>
              <span className="change-operation">{operationLabels[file.operation] ?? "Changed"}</span>
            </summary>
            <div className="change-details-body">
              <div className="change-detail-row">
                <span>Path</span>
                <code>{file.secondaryPath ? `${file.primaryPath} → ${file.secondaryPath}` : file.primaryPath}</code>
              </div>
              {file.diff ? <pre className="change-diff">{file.diff}</pre> : null}
            </div>
          </details>
        ))}
      </div>
    </details>
  );
}

type ChangeHistoryPanelProps = {
  timeline: VersionTimeline;
  activityTimeline: ActivityTimeline;
  workspaceName: string | null;
  busy: boolean;
  onRefresh: () => void;
  onRestore: (versionId: string | null) => Promise<void>;
  onClear: () => Promise<void>;
};

function ChangeHistoryPanel({
  timeline,
  activityTimeline,
  workspaceName,
  busy,
  onRefresh,
  onRestore,
  onClear,
}: ChangeHistoryPanelProps) {
  const [restoreTarget, setRestoreTarget] = useState<RestoreTarget | null>(null);
  const [clearOpen, setClearOpen] = useState(false);
  const [clearBusy, setClearBusy] = useState(false);
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState<HistoryFilter>("all");
  const [sort, setSort] = useState<HistorySort>("newest");
  const [visibleCount, setVisibleCount] = useState(PAGE_SIZE);
  const records = timeline.records;
  const currentId = timeline.currentVersionId;

  useEffect(() => setVisibleCount(PAGE_SIZE), [search, filter, sort, workspaceName]);

  const byId = useMemo(() => new Map(records.map((record) => [record.id, record])), [records]);
  const current = currentId ? byId.get(currentId) ?? null : null;
  const currentLineage = useMemo(() => {
    const lineage = new Set<string>();
    let cursor = currentId;
    while (cursor) {
      if (lineage.has(cursor)) break;
      lineage.add(cursor);
      cursor = byId.get(cursor)?.parentId ?? null;
    }
    return lineage;
  }, [byId, currentId]);

  const previousId = current?.parentId ?? null;
  const previousAvailable = currentId !== null;
  const forwardTarget = useMemo(() => {
    const candidates = records.filter((record) => record.parentId === currentId).sort((a, b) => a.createdAt - b.createdAt);
    if (currentId === null) return candidates[0] ?? null;
    return candidates.at(-1) ?? null;
  }, [records, currentId]);

  const entries = useMemo(() => {
    const linkedVersionIds = new Set<string>();
    const activityEntries: TimelineEntry[] = activityTimeline.groups.map((group) => {
      const linked = records.filter(
        (version) => group.versionIds.includes(version.id) ||
          (!!group.traceGroupId && version.editGroupId === group.traceGroupId),
      );
      linked.forEach((version) => linkedVersionIds.add(version.id));
      return { kind: "activity", group, versions: linked, timestamp: group.updatedAt };
    });
    const legacyEntries: TimelineEntry[] = records
      .filter((version) => !linkedVersionIds.has(version.id))
      .map((version) => ({ kind: "version", version, timestamp: version.updatedAt }));
    return [...activityEntries, ...legacyEntries];
  }, [activityTimeline.groups, records]);

  const filteredEntries = useMemo(() => {
    const query = search.trim().toLowerCase();
    return entries
      .filter((entry) => entryMatches(entry, filter, query))
      .sort((a, b) => sort === "newest" ? b.timestamp - a.timestamp : a.timestamp - b.timestamp);
  }, [entries, filter, search, sort]);

  const visibleEntries = filteredEntries.slice(0, visibleCount);
  const groupedEntries = useMemo(() => {
    const groups = new Map<string, TimelineEntry[]>();
    for (const entry of visibleEntries) {
      const label = dateGroupLabel(entry.timestamp);
      const values = groups.get(label) ?? [];
      values.push(entry);
      groups.set(label, values);
    }
    return Array.from(groups.entries());
  }, [visibleEntries]);

  async function confirmRestore() {
    if (!restoreTarget) return;
    await onRestore(restoreTarget.kind === "original" ? null : restoreTarget.version.id);
    setRestoreTarget(null);
  }

  async function confirmClear() {
    setClearBusy(true);
    try {
      await onClear();
      setClearOpen(false);
      setSearch("");
      setFilter("all");
      setVisibleCount(PAGE_SIZE);
    } finally {
      setClearBusy(false);
    }
  }

  const currentPosition = current
    ? Math.max(1, records.slice().sort((a, b) => a.createdAt - b.createdAt).findIndex((item) => item.id === current.id) + 1)
    : 0;
  const showOriginal = search.trim() === "" && filter === "all";
  const originalCard = showOriginal ? (
    <article className={`version-card original-card ${currentId === null ? "current" : ""}`}>
      <div className="version-card-head">
        <div className="version-copy">
          <div className="version-meta">
            {currentId === null ? <span className="change-status applied">Current</span> : <span className="version-dot" aria-hidden="true" />}
            <span>Beginning of RepoTunnel history</span>
          </div>
          <h3>Original project state</h3>
        </div>
        {currentId !== null ? (
          <button className="secondary-button" type="button" disabled={busy || clearBusy} onClick={() => setRestoreTarget({ kind: "original" })}>
            Restore original
          </button>
        ) : null}
      </div>
    </article>
  ) : null;

  function renderVersion(version: VersionRecord) {
    const isCurrent = version.id === currentId;
    const branched = !isCurrent && (currentId === null || !currentLineage.has(version.id));
    return (
      <article className={`version-card ${isCurrent ? "current" : ""}`} key={`version:${version.id}`}>
        <div className="version-card-head">
          <div className="version-copy">
            <div className="version-meta">
              {isCurrent ? <span className="change-status applied">Current</span> : <span className="version-dot" aria-hidden="true" />}
              <span>{formatTime(version.updatedAt)}</span><span aria-hidden="true">•</span>
              <span>{version.files.length} {version.files.length === 1 ? "file" : "files"}</span>
              {branched ? <span className="version-branch-label">Saved future</span> : null}
            </div>
            <h3>{version.summary}</h3>
          </div>
          {!isCurrent ? (
            <div className="version-actions">
              <button className="secondary-button" type="button" disabled={busy || clearBusy} onClick={() => setRestoreTarget({ kind: "version", version })}>
                Restore this version
              </button>
            </div>
          ) : null}
        </div>
        <FileDetails version={version} />
      </article>
    );
  }

  function renderActivity(group: ActivityGroup, versions: VersionRecord[]) {
    const linkedVersion = versions.slice().sort((a, b) => b.updatedAt - a.updatedAt)[0] ?? null;
    const isCurrent = versions.some((version) => version.id === currentId);
    const branched = !!linkedVersion && !isCurrent && (currentId === null || !currentLineage.has(linkedVersion.id));
    const status = groupStatus(group);
    const kinds = Array.from(new Set(group.events.map((event) => event.kind)));
    return (
      <article className={`version-card activity-card ${isCurrent ? "current" : ""}`} key={`activity:${group.id}`}>
        <div className="version-card-head">
          <div className="version-copy">
            <div className="version-meta">
              {isCurrent ? <span className="change-status applied">Current</span> : <span className="activity-request-dot" aria-hidden="true" />}
              <span>AI activity</span><span aria-hidden="true">•</span><span>{formatTime(group.updatedAt)}</span>
              <span aria-hidden="true">•</span><span>{group.events.length} {group.events.length === 1 ? "action" : "actions"}</span>
              <span className={`activity-status ${status}`}>{statusLabel(status)}</span>
              {branched ? <span className="version-branch-label">Saved future</span> : null}
            </div>
            <h3>{activityTitle(group, versions)}</h3>
            <div className="activity-kind-row">
              {kinds.map((kind) => <span className="activity-kind-chip" key={kind}>{activityLabels[kind]}</span>)}
              {linkedVersion ? <span className="activity-version-chip">Restore point</span> : null}
            </div>
          </div>
          {linkedVersion && !isCurrent ? (
            <div className="version-actions">
              <button className="secondary-button" type="button" disabled={busy || clearBusy} onClick={() => setRestoreTarget({ kind: "version", version: linkedVersion })}>
                Restore this version
              </button>
            </div>
          ) : null}
        </div>

        <details className="change-details activity-details">
          <summary>View activity</summary>
          <div className="activity-event-list">
            {group.events.map((event) => (
              <div className="activity-event-row" key={event.id}>
                <div className="activity-event-main">
                  <span className="activity-kind-chip">{activityLabels[event.kind]}</span>
                  <strong>{event.summary}</strong>
                  <span className={`activity-status ${event.status}`}>{statusLabel(event.status)}</span>
                </div>
                {event.detail ? <pre>{event.detail}</pre> : null}
              </div>
            ))}
          </div>
        </details>
        {linkedVersion ? <FileDetails version={linkedVersion} /> : null}
      </article>
    );
  }

  const hasHistory = entries.length > 0 || records.length > 0;

  return (
    <section className="change-section version-history" aria-labelledby="change-title">
      <div className="section-heading change-heading">
        <div>
          <span className="section-kicker">AI activity history</span>
          <h2 id="change-title">Changes &amp; history</h2>
          <p>
            One AI request stays grouped across files, terminal, processes, browser, Git and verification. File-changing requests keep their existing local restore point.
          </p>
        </div>
        <div className="change-heading-actions">
          <button className="secondary-button" type="button" onClick={onRefresh} disabled={busy || clearBusy}>Refresh</button>
        </div>
      </div>

      {workspaceName ? (
        <div className="version-toolbar">
          <div className="version-project-copy">
            <strong>{workspaceName}</strong>
            <span>{currentId === null ? "Original version" : `Version ${currentPosition} · ${records.length} saved ${records.length === 1 ? "step" : "steps"}`}</span>
          </div>
          <div className="version-nav-actions">
            <button
              className="secondary-button" type="button" disabled={busy || clearBusy || !previousAvailable}
              onClick={() => {
                if (!current) return;
                if (previousId) {
                  const previous = byId.get(previousId);
                  if (previous) setRestoreTarget({ kind: "version", version: previous });
                } else setRestoreTarget({ kind: "original" });
              }}
            >← Previous</button>
            <span className={`version-current-pill ${currentId === null ? "original" : ""}`}>{currentId === null ? "Original" : "Current"}</span>
            <button className="secondary-button" type="button" disabled={busy || clearBusy || !forwardTarget} onClick={() => forwardTarget && setRestoreTarget({ kind: "version", version: forwardTarget })}>Next →</button>
          </div>
        </div>
      ) : null}

      {workspaceName && hasHistory ? (
        <div className="history-management-toolbar">
          <label className="history-search"><span className="sr-only">Search history</span><input type="search" value={search} onChange={(event) => setSearch(event.target.value)} placeholder="Search history" /></label>
          <select value={filter} onChange={(event) => setFilter(event.target.value as HistoryFilter)} aria-label="Filter history">
            <option value="all">All activity</option><option value="files">Files</option><option value="terminal">Terminal</option>
            <option value="process">Processes</option><option value="launcher">Apps</option><option value="browser">Browser</option>
            <option value="git">Git</option><option value="verification">Verification</option><option value="monitoring">Monitoring</option><option value="team">Team</option>
          </select>
          <select value={sort} onChange={(event) => setSort(event.target.value as HistorySort)} aria-label="Sort history">
            <option value="newest">Newest first</option><option value="oldest">Oldest first</option>
          </select>
          <button type="button" className="secondary-button history-clear-button" disabled={busy || clearBusy} onClick={() => setClearOpen(true)}>Clear All History</button>
        </div>
      ) : null}

      {!workspaceName ? (
        <div className="change-empty"><strong>Select a project</strong><p>Choose a project from the Projects rail to view its AI activity and restore history.</p></div>
      ) : !hasHistory ? (
        <div className="change-empty"><strong>No AI activity yet</strong><p>File changes, commands, processes, browser tests, Git actions, Team Mode activity and verification will appear here.</p></div>
      ) : filteredEntries.length === 0 ? (
        <div className="change-empty history-filter-empty"><strong>No matching history</strong><p>Try another search or change the filter.</p></div>
      ) : (
        <div className="version-list">
          {sort === "oldest" && showOriginal && records.length > 0 ? originalCard : null}
          {groupedEntries.map(([label, values]) => (
            <div className="history-date-group" key={label}>
              <div className="history-date-heading"><span>{label}</span></div>
              {values.map((entry) => entry.kind === "activity" ? renderActivity(entry.group, entry.versions) : renderVersion(entry.version))}
            </div>
          ))}
          {visibleCount < filteredEntries.length ? (
            <div className="history-load-more">
              <button type="button" className="secondary-button" onClick={() => setVisibleCount((count) => count + PAGE_SIZE)}>Load More</button>
              <span>{Math.min(visibleCount, filteredEntries.length)} of {filteredEntries.length}</span>
            </div>
          ) : null}
          {sort === "newest" && showOriginal && records.length > 0 && visibleCount >= filteredEntries.length ? originalCard : null}
        </div>
      )}

      {restoreTarget ? (
        <ConfirmationDialog
          title={restoreTarget.kind === "original" ? "Restore original project state?" : "Restore this saved version?"}
          message={restoreTarget.kind === "original"
            ? "RepoTunnel will move the project back to the state captured before its first AI edit. Your existing later versions remain in history, so you can move forward again."
            : `RepoTunnel will restore “${restoreTarget.version.summary}”. Later versions remain saved, so you can return to them afterward.`}
          confirmLabel="Restore version" busy={busy} busyLabel="Restoring…" onConfirm={() => void confirmRestore()} onCancel={() => !busy && setRestoreTarget(null)}
        />
      ) : null}

      {clearOpen ? (
        <ConfirmationDialog
          title="Clear all history for this project?"
          message={`RepoTunnel will remove saved versions and AI activity history for “${workspaceName ?? "this project"}”. Current project files will not be changed. Pending AI Review requests remain available.`}
          confirmLabel="Clear All History" busy={clearBusy} busyLabel="Clearing…" onConfirm={() => void confirmClear()} onCancel={() => !clearBusy && setClearOpen(false)}
        />
      ) : null}
    </section>
  );
}

export default ChangeHistoryPanel;
