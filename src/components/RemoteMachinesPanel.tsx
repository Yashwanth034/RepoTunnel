import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { NavIcon } from "./AppSidebar";
import "./RemoteMachinesPanel.css";

type RemoteProfile = {
  id:string; label:string; endpoint:string; remoteWorkspaceId:string; identityFingerprint:string; savedAt:number;
};
type RemoteNode = {
  id: string; label: string; endpoint: string; connectedAt: number;
  connectionNonce: string;
  availableTools: string[]; ownerClaimed: boolean; remoteWorkspaceId: string;
  identityFingerprint:string;
};
type Credential = { secret: string; expiresAt: number; workspaceId: string; warning: string };
type ApprovedProject = { id: string; name: string };
type ActiveGrant = {
  fingerprint: string; expiresAt: number; workspaceId: string; scopes: string[];
};
type NodeProbe = {
  nodeId: string; checkedAt: number; advertisedToolCount: number;
  capabilitiesChanged: boolean; note: string;
};
type BrowserProbe = {
  nodeId:string; checkedAt:number; available:boolean; runningForProbeSession:boolean;
  browserName:string|null; note:string;
};
type CapacityProbe = {
  nodeId: string; checkedAt: number; platform: string; architecture: string;
  memoryAvailableBytes: number | null; workspaceDiskAvailableBytes: number | null;
  gpuAvailable: boolean; gpuDeviceCount: number; note: string;
};
type RemoteJobPreview = {
  nodeId: string; checkedAt: number; remoteWorkspaceId: string;
  runningProcesses: number; note: string;
  jobs: Array<{processId:string; label:string; status:string; createdAt:number; exitCode:number|null}>;
};
type SourceWatch = {
  localWorkspaceId:string; remoteWorkspaceId:string; fingerprint:string; totalSourceFiles:number;
  machineIdentityFingerprint:string;
  connectedAt:number;
  connectionNonce:string;
};
type SourceAudit = {
  nodeId:string; localWorkspaceId:string; remoteWorkspaceId:string;
  sourceFingerprint:string; totalSourceFiles:number;
  cursorIndex:number; nextCursorIndex:number; complete:boolean; sourceVerified:boolean;
  rows:Array<{path:string;status:"identical"|"missing"|"conflict"|"unverified";localSha256:string;localBytes:number;remoteSha256:string|null;remoteBytes:number|null}>;
  note:string;
};
const displayBytes = (value: number | null) =>
  value == null ? "unknown" : (value / (1024 ** 3)).toFixed(1) + " GiB";
const scopeOptions = [
  { id: "files", label: "Project files and inspection" },
  { id: "browser", label: "Managed browsers" },
  { id: "desktop", label: "AI Workspace and desktop apps" },
  { id: "process", label: "Builds, tests and processes" },
  { id: "source", label: "Project source transfer and artifacts" },
  { id: "media", label: "Media inspection" },
] as const;
type NativeMatrixJob = {
  platform: string; architecture?: string | null;
  nodeId: string; remoteWorkspaceId: string;
  processId?: string | null; state: string; evidence: string;
  cancellationObservation?: string | null; cancellationObservedAt?: number | null;
};
type NativeMatrixRun = {
  id: string; createdAt: number; localWorkspaceId: string;
  dispatchComplete: boolean; jobs: NativeMatrixJob[]; limitations: string;
};
type DownloadedArtifact = {
  transferId: string; relativePath: string; receivedBytes: number;
  totalBytes: number; completed: boolean; sha256: string;
};
type CreateOnlyPreflight = {
  nodeId:string; localWorkspaceId:string; remoteWorkspaceId:string;
  machineIdentityFingerprint:string; connectedAt:number; connectionNonce:string; sourceFingerprint:string;
  inspected:number; proposed:Array<{path:string;sha256:string;bytes:number}>;
  note:string;
};
type HumanSourceIntent = {
  id:string; createdAt:number; state:"awaitingReceiverApproval" | "withdrawn";
  nodeId:string; localWorkspaceId:string; remoteWorkspaceId:string;
  machineIdentityFingerprint:string; connectedAt:number; connectionNonce:string;
  sourceFingerprint:string;
  proposed:Array<{path:string;sha256:string;bytes:number}>;
  importAttempt?: {
    approvalId:string;taskId:string;
    state:"staging"|"failedBeforeInstall"|"dispatchUnknown"|"imported";
    importedFiles:number;
  } | null;
};
type SourceIntentObservation = {
  intentId:string;localWorkspaceId:string;remoteWorkspaceId:string;nodeId:string;
  machineIdentityFingerprint:string;connectedAt:number;connectionNonce:string;
  sourceFingerprint:string;rows:SourceAudit["rows"];note:string;
};
type CreateOnlyProgress = {
  nodeId:string; localWorkspaceId:string; remoteWorkspaceId:string;
  machineIdentityFingerprint:string; connectedAt:number; connectionNonce:string; sourceFingerprint:string;
  checkedPaths:string[];
};
type SourceAuditProgress = {
  inspected:number; identical:number; missing:number; conflicts:number; unverified:number;
  issues:SourceAudit["rows"];
  // All source paths, including identical rows, must be unique across pages.
  // Case folding and Unicode normalization guard portable filesystem aliases.
  seenPaths:string[];
  pairedMachineFingerprint:string;
  pairedConnectedAt:number;
  pairedConnectionNonce:string;
};
type ReceiverSourceApproval = {
  id:string; createdAt:number; state:"approved"|"withdrawn"|"legacyUnsigned"|"dispatchUnknown"|"imported";
  workspaceId:string; credentialFingerprint:string;
  receiverMachineFingerprint:string; identityPublicKey:string; identitySignature:string;
  coordinatorMachineFingerprint:string; coordinatorWorkspaceId:string;
  coordinatorConnectionNonce:string; coordinatorSourceFingerprint:string;
  sourceReviewId:string;
  files:Array<{path:string;sha256:string;bytes:number}>;
};
type ReceiverReviewRemoteInspection = ReceiverApprovalInspection & {
  reviewBinding:{
    remoteWorkspaceId:string;receiverMachineFingerprint:string;
    coordinatorMachineFingerprint:string;coordinatorWorkspaceId:string;
    coordinatorConnectionNonce:string;coordinatorSourceFingerprint:string;
    sourceReviewId:string;
  };
};
type ReceiverApprovalInspection = {
  approvalId:string;state:string;importTaskId?:string;credentialActive:boolean;
  humanImportSupported:boolean;
  destinationProjectApprovedWritable:boolean;canStillProposeCreateOnly:boolean;
  files:Array<{path:string;expectedSha256:string;expectedBytes:number;status:"missing"|"matchingBytes"|"conflict"|"unverified"}>;
  note:string;
};
type InboundReviewDraft = {
  schemaVersion:1; kind:"RepoTunnelIncomingSourceReviewV1";
  remoteWorkspaceId:string; receiverMachineFingerprint:string;
  coordinatorMachineFingerprint:string;
  coordinatorWorkspaceId:string; coordinatorConnectionNonce:string;
  coordinatorSourceFingerprint:string; sourceReviewId:string;
  files:Array<{path:string;sha256:string;bytes:number}>;
  identityPublicKey:string; identitySignature:string;
};
type SourceReviewChoice = "defer" | "proposeCreate" | "manualMerge" | "keepRemote";
const defaultReviewChoice = (status: SourceAudit["rows"][number]["status"]): SourceReviewChoice =>
  status === "conflict" ? "manualMerge" : "defer";
const MAX_VISIBLE_DIFF_ISSUES = 40;
// The native source manifest refuses inventories over 6,000 source files.
// Keep every inspected issue within that same bound, but render only one page.
const MAX_REVIEW_ISSUES = 6000;
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);
const sourceShaPattern = /^[a-f0-9]{64}$/;
// Treat every remote comparison row as untrusted MCP output. Exact IDs and
// cursors alone cannot authenticate an invalid, aliased, or contradictory row.
const validSourceAuditRow = (row: SourceAudit["rows"][number]): boolean => {
  const validPath = typeof row?.path === "string" && row.path.length > 0
    && row.path.length <= 2048 && !row.path.startsWith("/")
    && !row.path.includes("\\") && !/[\x00-\x1f\x7f]/.test(row.path)
    && row.path.split("/").every((segment) => Boolean(segment) && segment !== "." && segment !== "..")
    && !/^[a-zA-Z]:/.test(row.path);
  const validSize = (value: number | null) => value === null
    || (Number.isSafeInteger(value) && value >= 0);
  if (!validPath || !sourceShaPattern.test(row.localSha256)
    || !validSize(row.localBytes) || !validSize(row.remoteBytes)
    || row.remoteSha256 != null && !sourceShaPattern.test(row.remoteSha256)
    || !["identical", "missing", "conflict", "unverified"].includes(row.status)) return false;
  if (row.status === "identical") {
    return row.remoteSha256 === row.localSha256 && row.remoteBytes === row.localBytes;
  }
  if (row.status === "missing") return row.remoteSha256 === null && row.remoteBytes === null;
  if (row.status === "conflict") {
    return row.remoteSha256 !== null && row.remoteBytes !== null
      && (row.remoteSha256 !== row.localSha256 || row.remoteBytes !== row.localBytes);
  }
  return true;
};
export default function RemoteMachinesPanel() {
  const [view, setView] = useState<"overview" | "connect" | "credentials" | "machines" | "activity" | "source">("overview");
  const [nodes, setNodes] = useState<RemoteNode[]>([]);
  const [rememberedProfiles, setRememberedProfiles] = useState<RemoteProfile[]>([]);
  const [localIdentity, setLocalIdentity] = useState("");
  const [artifacts, setArtifacts] = useState<DownloadedArtifact[]>([]);
  const [activeGrants, setActiveGrants] = useState<ActiveGrant[]>([]);
  const [nativeRuns, setNativeRuns] = useState<NativeMatrixRun[]>([]);
  const [nativeJobObservations, setNativeJobObservations] = useState<Record<string, string>>({});
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [credential, setCredential] = useState("");
  const [remoteWorkspaceId, setRemoteWorkspaceId] = useState("");
  const [issued, setIssued] = useState<Credential | null>(null);
  const [projects, setProjects] = useState<ApprovedProject[]>([]);
  const [selectedWorkspace, setSelectedWorkspace] = useState("");
  const [scopes, setScopes] = useState<string[]>(["browser", "desktop"]);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState("");
  const [peerIds, setPeerIds] = useState<Record<string, string>>({});
  const [reconnectSecrets, setReconnectSecrets] = useState<Record<string, string>>({});
  const [connectionChecks, setConnectionChecks] = useState<Record<string, string>>({});
  const [healthMonitorEnabled, setHealthMonitorEnabled] = useState(false);
  const [capacityChecks, setCapacityChecks] = useState<Record<string, string>>({});
  const [browserChecks, setBrowserChecks] = useState<Record<string, string>>({});
  const [remoteJobs, setRemoteJobs] = useState<Record<string, RemoteJobPreview>>({});
  const [diffLocalProjects, setDiffLocalProjects] = useState<Record<string, string>>({});
  const [sourceAudits, setSourceAudits] = useState<Record<string, SourceAudit>>({});
  const [sourceAuditProgress, setSourceAuditProgress] = useState<Record<string, SourceAuditProgress>>({});
  const [sourceIssuePages, setSourceIssuePages] = useState<Record<string, number>>({});
  const [sourceReviewChoices, setSourceReviewChoices] =
    useState<Record<string, Record<string, SourceReviewChoice>>>({});
  const [createOnlyPreflights, setCreateOnlyPreflights] =
    useState<Record<string, CreateOnlyProgress>>({});
  const [humanSourceIntents, setHumanSourceIntents] = useState<HumanSourceIntent[]>([]);
  const [incomingSourceApprovals, setIncomingSourceApprovals] =
    useState<ReceiverSourceApproval[]>([]);
  const [incomingApprovalChecks, setIncomingApprovalChecks] =
    useState<Record<string, ReceiverApprovalInspection>>({});
  const [incomingReviewDraft, setIncomingReviewDraft] = useState("");
  const [incomingCredentialFingerprint, setIncomingCredentialFingerprint] = useState("");
  const [sourceIntentObservations, setSourceIntentObservations] =
    useState<Record<string, SourceIntentObservation>>({});
  const [receiverApprovalIds, setReceiverApprovalIds] = useState<Record<string,string>>({});
  const [remoteReceiverApprovals, setRemoteReceiverApprovals] =
    useState<Record<string, ReceiverReviewRemoteInspection>>({});
  const [humanImportProgress, setHumanImportProgress] = useState<Record<string, {
    phase:string; path?:string;sentBytes:number;totalBytes:number;taskId:string;
  }>>({});
  const [humanImportOutcomes, setHumanImportOutcomes] = useState<Record<string,{
    state:"imported"|"dispatchUnknown"|"failedBeforeInstall";importedFiles:number;note:string;taskId?:string;
  }>>({});
  const [humanImportVerifications, setHumanImportVerifications] = useState<Record<string,{
    state:"verifiedImport"|"changedAfterImport"|"matchingWithoutReceipt"|"notVerified";
    receiverState:string;files:ReceiverReviewRemoteInspection["files"];note:string;
  }>>({});
  useEffect(() => {
    // The native Tauri event bridge does not exist in web previews or React
    // tests. The import command itself remains a separate explicit invoke.
    if (!("__TAURI_INTERNALS__" in window)) return;
    let cancelled = false;
    let unlisten: (()=>void) | undefined;
    void listen<{intentId:string;phase:string;path?:string;sentBytes:number;
      totalBytes:number;taskId:string}>("human-source-import-progress", (event) => {
      if (!cancelled) setHumanImportProgress((current)=>({
        ...current,[event.payload.intentId]:event.payload,
      }));
    }).then((cleanup)=>{if(cancelled) cleanup();else unlisten=cleanup;})
      .catch(()=>{/* Without event bridge, the import result still arrives via invoke. */});
    return ()=>{cancelled=true;unlisten?.();};
  }, []);
  // Even an edit-then-restore of the exact approval ID invalidates an
  // in-flight old receiver response, not just a changed final input value.
  const receiverApprovalEpoch = useRef<Record<string,number>>({});
  const [scanningSourceNode, setScanningSourceNode] = useState<string | null>(null);
  // A review edit, rescan, pairing change, or local-project switch makes
  // prior preflight results stale. Never carry a preflight receipt forward.
  useEffect(() => { setCreateOnlyPreflights({}); },
    [sourceAudits, sourceReviewChoices, nodes, diffLocalProjects]);
  const stopSourceScan = useRef(false);
  // An outstanding read-only preflight must not restore a receipt after any
  // review decision changes (even if changed back before its ACK arrives).
  const sourceReviewEpoch = useRef<Record<string, number>>({});
  const invalidateSourceReview = (nodeId: string) => {
    sourceReviewEpoch.current[nodeId] = (sourceReviewEpoch.current[nodeId] ?? 0) + 1;
  };
  // Leaving Computers must not keep scheduling read-only pages on a detached
  // view. An already sent page may complete, but no further requests follow.
  useEffect(() => {
    computersMounted.current = true;
    return () => {
      stopSourceScan.current = true;
      computersMounted.current = false;
    };
  }, []);
  // Keep the current pairing and project selection available to in-flight
  // read-only requests. A late result must never restore stale review evidence
  // after refresh, re-pair, project switch, or Computers closing.
  const latestNodes = useRef(nodes);
  const latestDiffProjects = useRef(diffLocalProjects);
  const computersMounted = useRef(true);
  latestNodes.current = nodes;
  latestDiffProjects.current = diffLocalProjects;
  const sameSourcePairing = (node: RemoteNode) =>
    computersMounted.current && latestNodes.current.some((current) =>
      current.id === node.id
      && current.remoteWorkspaceId === node.remoteWorkspaceId
      && current.identityFingerprint === node.identityFingerprint
      && current.connectedAt === node.connectedAt
      && current.connectionNonce === node.connectionNonce);
  // Out-of-order concurrent refresh requests cannot replace a newer trusted
  // machine registry with stale pairing records.
  const latestRefresh = useRef(0);
  const [sourceWatches, setSourceWatches] = useState<Record<string, SourceWatch>>({});
  const [sourceWatchReports, setSourceWatchReports] = useState<Record<string, string>>({});
  async function refresh() {
    const requestGeneration = ++latestRefresh.current;
    const isLatest = () => computersMounted.current
      && requestGeneration === latestRefresh.current;
    const received = await invoke<RemoteNode[]>("list_remote_machines");
    if (!isLatest()) return;
    const incoming = Array.isArray(received) ? received : [];
    const old = latestNodes.current;
    const changed = old.filter((prior) => !incoming.some((current) =>
      current.id === prior.id
      && current.remoteWorkspaceId === prior.remoteWorkspaceId
      && current.identityFingerprint === prior.identityFingerprint
      && current.connectedAt === prior.connectedAt
      && current.connectionNonce === prior.connectionNonce)).map((item) => item.id);
    latestNodes.current = incoming;
    setNodes(incoming);
    if (changed.length) {
      changed.forEach(invalidateSourceReview);
      const discard = <T,>(current: Record<string, T>) => {
        const next = {...current};
        changed.forEach((id) => { delete next[id]; });
        return next;
      };
      setSourceAudits(discard);
      setSourceAuditProgress(discard);
      setSourceReviewChoices(discard);
      setSourceIssuePages(discard);
      setCreateOnlyPreflights(discard);
      setSourceIntentObservations((current) =>
        Object.fromEntries(Object.entries(current).filter(([,observation]) =>
          !changed.includes(observation.nodeId))));
      setSourceWatches(discard);
      setSourceWatchReports((current) => ({
        ...discard(current),
        ...Object.fromEntries(changed.map((id) => [id,
          "Source parity monitor stopped: paired machine identity or approved project changed (including connection generation); all source review and preflight evidence was invalidated. Restart read-only comparison."])),
      }));
      stopSourceScan.current = true;
    }
    const profiles = await invoke<RemoteProfile[]>("list_remembered_remote_machines");
    if (!isLatest()) return;
    setRememberedProfiles(Array.isArray(profiles) ? profiles : []);
    const projects = await invoke<ApprovedProject[]>("list_workspaces");
    if (!isLatest()) return;
    setProjects(projects);
    const artifacts = await invoke<DownloadedArtifact[]>("list_downloaded_remote_artifacts");
    if (!isLatest()) return;
    setArtifacts(artifacts);
    const grants = await invoke<ActiveGrant[]>("list_remote_machine_access_grants");
    if (!isLatest()) return;
    setActiveGrants(grants);
    const nativeRuns = await invoke<NativeMatrixRun[]>("list_remote_native_test_runs_desktop");
    if (!isLatest()) return;
    setNativeRuns(nativeRuns);
    // Local read-only recovery: a lost desktop ACK must never cause the UI
    // to issue a second consent-writing command automatically.
    const intentions = await invoke<HumanSourceIntent[]>("list_remote_source_review_intents_desktop");
    if (!isLatest()) return;
    setHumanSourceIntents(Array.isArray(intentions) ? intentions : []);
    setReceiverApprovalIds((current)=>({
      ...current,
      ...Object.fromEntries((Array.isArray(intentions) ? intentions : [])
        .filter((intent)=>intent.importAttempt?.approvalId)
        .map((intent)=>[intent.id,intent.importAttempt!.approvalId])),
    }));
    const savedReceiverApprovals = await invoke<ReceiverSourceApproval[]>("list_incoming_source_approvals_desktop");
    if (!isLatest()) return;
    setIncomingSourceApprovals(Array.isArray(savedReceiverApprovals) ? savedReceiverApprovals : []);
    setIncomingApprovalChecks({});
    setSourceIntentObservations((current) =>
      Object.fromEntries(Object.entries(current).filter(([intentId]) =>
        Array.isArray(intentions) && intentions.some((intent) =>
          intent.id===intentId && intent.state==="awaitingReceiverApproval"))));
  }
  useEffect(() => {
    void refresh().catch((error) => setNotice(errorText(error)));
    void invoke<string>("get_local_remote_machine_identity")
      .then((fingerprint) => setLocalIdentity(fingerprint))
      .catch((error) => setNotice("Local machine identity unavailable: " + errorText(error)));
  }, []);
  // Opt-in while this page is mounted; never persists a bearer, starts an
  // action, steals AI ownership, retries mutations or reconnects silently.
  useEffect(() => {
    if (!healthMonitorEnabled || nodes.length === 0) return;
    let mounted = true;
    let running = false;
    async function observeOnly() {
      if (running || !mounted) return;
      running = true;
      try {
        // Bounded to existing enrolled nodes (at most eight). Each remote
        // request is already timeout-limited in the native coordinator.
        await Promise.all(nodes.map(async (node) => {
          let message: string;
          try {
            const report = await invoke<NodeProbe>("probe_remote_machine", { nodeId: node.id });
            message = `MCP replied at ${new Date(report.checkedAt * 1000).toLocaleTimeString()}. ${report.advertisedToolCount} tools; ${report.capabilitiesChanged ? "capabilities changed — manually re-pair to update" : "capabilities unchanged"}. ${report.note}`;
          } catch (error) {
            message = "Health check failed: " + errorText(error) + ". Manual re-pair may be required; no remote action was repeated.";
          }
          if (mounted) {
            setConnectionChecks((current) => ({ ...current, [node.id]: message }));
          }
        }));
      } finally { running = false; }
    }
    void observeOnly();
    const timer = window.setInterval(() => { void observeOnly(); }, 60_000);
    return () => { mounted = false; window.clearInterval(timer); };
  }, [healthMonitorEnabled, nodes]);

  // Opt-in, page-lifetime source parity watcher. It calls ONLY the same
  // human-scoped aggregate preview endpoint; never stages/imports or writes.
  useEffect(() => {
    const watchers = Object.entries(sourceWatches);
    if (watchers.length === 0) return;
    let mounted = true;
    let running = false;
    async function checkOnly() {
      if (!mounted || running) return;
      running = true;
      try {
        await Promise.all(watchers.map(async ([nodeId, watch]) => {
          if (!nodes.some((node) => node.id === nodeId
            && node.remoteWorkspaceId === watch.remoteWorkspaceId
            && node.identityFingerprint === watch.machineIdentityFingerprint
            && node.connectedAt === watch.connectedAt
            && node.connectionNonce === watch.connectionNonce)) {
            if (mounted) {
              setSourceWatches((current) => { const next={...current}; delete next[nodeId]; return next; });
              setSourceAudits((current) => { const next={...current}; delete next[nodeId]; return next; });
              setSourceAuditProgress((current) => { const next={...current}; delete next[nodeId]; return next; });
              setSourceReviewChoices((current) => { const next={...current}; delete next[nodeId]; return next; });
              setSourceWatchReports((current) => ({...current,[nodeId]:
                "Source parity monitor stopped: paired machine identity or approved project changed. Restart the read-only comparison."}));
            }
            return;
          }
          try {
            const report = await invoke<SourceAudit>("preview_remote_source_diff_desktop", {
              nodeId,localWorkspaceId:watch.localWorkspaceId,
              expectedSourceSha256:watch.fingerprint,cursorIndex:watch.totalSourceFiles,
              expectedMachineIdentity:watch.machineIdentityFingerprint,
              expectedConnectionTime:watch.connectedAt,
              expectedConnectionNonce:watch.connectionNonce,
            });
            if (report.nodeId !== nodeId || report.localWorkspaceId !== watch.localWorkspaceId
              || report.remoteWorkspaceId !== watch.remoteWorkspaceId
              || report.sourceFingerprint !== watch.fingerprint
              || report.totalSourceFiles !== watch.totalSourceFiles
              || report.cursorIndex !== watch.totalSourceFiles
              || report.nextCursorIndex !== watch.totalSourceFiles
              || !report.complete || report.rows.length !== 0) {
              throw new Error("Current source inventory or approved remote project changed.");
            }
            if (mounted) {
              if (report.sourceVerified) {
                setSourceWatchReports((current) => ({...current,[nodeId]:
                  "Read-only source parity confirmed at " + new Date().toLocaleTimeString() +
                  ". No remote files were modified."}));
              } else {
                setSourceWatchReports((current) => ({...current,[nodeId]:
                  "Source parity LOST. Remote aggregate no longer matches the approved local project. Restart the full difference preview."}));
                setSourceWatches((current) => { const next={...current}; delete next[nodeId]; return next; });
                setSourceAudits((current) => { const next={...current}; delete next[nodeId]; return next; });
                setSourceAuditProgress((current) => { const next={...current}; delete next[nodeId]; return next; });
              }
            }
          } catch(error) {
            if (mounted) {
              setSourceWatchReports((current) => ({...current,[nodeId]:
                "Source parity is UNVERIFIED: " + errorText(error) + ". Monitoring stopped; restart the comparison."}));
              setSourceWatches((current) => { const next={...current}; delete next[nodeId]; return next; });
              setSourceAudits((current) => { const next={...current}; delete next[nodeId]; return next; });
              setSourceAuditProgress((current) => { const next={...current}; delete next[nodeId]; return next; });
            }
          }
        }));
      } finally { running=false; }
    }
    void checkOnly();
    const timer=window.setInterval(() => { void checkOnly(); },120_000);
    return () => { mounted=false; window.clearInterval(timer); };
  }, [sourceWatches,nodes]);

  async function issue() {
    setBusy(true); setNotice("");
    try {
      setIssued(await invoke<Credential>("issue_remote_machine_access", {
        workspaceId: selectedWorkspace, scopes,
      }));
      setActiveGrants(await invoke<ActiveGrant[]>("list_remote_machine_access_grants"));
    }
    catch (error) { setNotice(errorText(error)); }
    finally { setBusy(false); }
  }
  async function revoke() {
    if (!window.confirm("Revoke all remote access credentials issued by this computer?")) return;
    setBusy(true);
    try {
      await invoke("revoke_remote_machine_access");
      setIssued(null); setActiveGrants([]);
      setNotice("All issued remote credentials revoked.");
    } catch (error) { setNotice(errorText(error)); }
    finally { setBusy(false); }
  }
  async function revokeOne(grant: ActiveGrant) {
    if (!window.confirm("Revoke this project's selected remote credential? Existing remote processes may keep running.")) return;
    setBusy(true);
    try {
      const removed = await invoke<boolean>("revoke_remote_machine_access_grant", { fingerprint: grant.fingerprint });
      setIssued(null);
      setActiveGrants(await invoke<ActiveGrant[]>("list_remote_machine_access_grants"));
      setNotice(removed ? "Selected remote credential revoked; already running remote processes may continue." : "Credential no longer exists or already expired.");
    } catch (error) { setNotice(errorText(error)); }
    finally { setBusy(false); }
  }
  async function rotateOne(grant: ActiveGrant) {
    if (!window.confirm("Rotate this remote credential now? The OLD credential immediately becomes invalid, existing MCP sessions will fail, and you MUST copy the replacement secret to the trusted coordinating computer and reconnect manually. Already running remote processes may continue.")) return;
    setBusy(true); setNotice("");
    try {
      // Only local native UI receives the replacement secret.
      const replacement = await invoke<Credential>("rotate_remote_machine_access_grant", {
        fingerprint: grant.fingerprint,
      });
      setIssued(replacement);
      // A failure to refresh grant metadata must not hide the one-time secret.
      try {
        setActiveGrants(await invoke<ActiveGrant[]>("list_remote_machine_access_grants"));
        setNotice("Credential rotated. Old sessions cannot issue new requests. Copy the replacement secret and reconnect the coordinator manually.");
      } catch (error) {
        setNotice("Credential rotated; replacement secret is shown below, but grant list refresh failed: " + errorText(error));
      }
    } catch (error) {
      setNotice("Credential rotation did not finish: " + errorText(error) + ". If the connection is uncertain, inspect the grant list before retrying.");
    } finally { setBusy(false); }
  }
  async function connect(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault(); setBusy(true); setNotice("");
    try {
      await invoke("connect_remote_machine", { name, url, credential, remoteWorkspaceId });
      setCredential(""); setName(""); setUrl(""); setRemoteWorkspaceId("");
      await refresh();
      setNotice("Remote MCP connected and approved project binding verified. Browser, desktop and build-tool readiness still require separate testing.");
    } catch (error) { setNotice("Connection failed: " + errorText(error)); }
    finally {
      // Never keep a failed bearer in the desktop form after any handshake.
      setCredential("");
      setBusy(false);
    }
  }
  async function rememberMachine(node: RemoteNode) {
    setBusy(true); setNotice("");
    try {
      await invoke<RemoteProfile>("remember_remote_machine", {nodeId:node.id});
      const profiles = await invoke<RemoteProfile[]>("list_remembered_remote_machines");
      setRememberedProfiles(Array.isArray(profiles) ? profiles : []);
      setNotice("Saved only the HTTPS address, machine name and approved project. No credential, AI session or process is saved.");
    } catch (error) { setNotice("Could not remember remote computer: " + errorText(error)); }
    finally { setBusy(false); }
  }
  async function forgetMachine(profile: RemoteProfile) {
    if (!window.confirm(`Forget saved connection details for ${profile.label}? The live connection, if any, is not disconnected.`)) return;
    setBusy(true); setNotice("");
    try {
      await invoke<boolean>("forget_remembered_remote_machine", {nodeId:profile.id});
      const profiles = await invoke<RemoteProfile[]>("list_remembered_remote_machines");
      setRememberedProfiles(Array.isArray(profiles) ? profiles : []);
      setNotice("Saved connection details forgotten. Active remote sessions and credentials were not changed.");
    } catch (error) { setNotice("Could not forget saved connection details: " + errorText(error)); }
    finally { setBusy(false); }
  }
  function useRememberedMachine(profile: RemoteProfile) {
    setCredential("");
    setName(profile.label); setUrl(profile.endpoint); setRemoteWorkspaceId(profile.remoteWorkspaceId);
    setNotice("Connection details filled. Obtain a fresh credential from the remote computer and click Connect; no session was reopened automatically.");
  }
  async function checkConnection(node: RemoteNode) {
    setBusy(true);
    setConnectionChecks((current) => ({ ...current, [node.id]: "Checking authenticated MCP connection…" }));
    try {
      const probe = await invoke<NodeProbe>("probe_remote_machine", { nodeId: node.id });
      setConnectionChecks((current) => ({
        ...current, [node.id]: `MCP replied at ${new Date(probe.checkedAt * 1000).toLocaleTimeString()}. ${probe.advertisedToolCount} delegated tools; ${probe.capabilitiesChanged ? "capability list changed — reconnect to update." : "capability list unchanged."} ${probe.note}`,
      }));
    } catch (error) {
      setConnectionChecks((current) => ({ ...current, [node.id]: "MCP health check failed: " + errorText(error) }));
    } finally { setBusy(false); }
  }
  async function rePair(node: RemoteNode) {
    const secret = reconnectSecrets[node.id] ?? "";
    if (secret.length !== 64) {
      setNotice("Enter the exact 64-character replacement credential from the remote computer.");
      return;
    }
    setBusy(true); setNotice("");
    try {
      await invoke("reconnect_remote_machine", { nodeId: node.id, credential: secret });
      setNotice("Remote machine successfully re-paired to the SAME approved project. No remote commands were replayed.");
      await refresh();
    } catch (error) {
      setNotice("Re-pair refused: " + errorText(error) + ". The existing connection was not replaced.");
    } finally {
      // Do not retain a bearer after a failed, timed-out or successful attempt.
      setReconnectSecrets((current) => { const next = { ...current }; delete next[node.id]; return next; });
      setBusy(false);
    }
  }
  async function checkBrowser(node: RemoteNode) {
    setBusy(true);
    setBrowserChecks((current) => ({...current, [node.id]:"Inspecting scoped remote browser metadata…"}));
    try {
      const report = await invoke<BrowserProbe>("inspect_remote_machine_browser", {nodeId:node.id});
      if (report.nodeId !== node.id) throw new Error("Browser probe returned another machine.");
      setBrowserChecks((current) => ({...current, [node.id]:
        `Browser installed: ${report.available ? "reported yes" : "not detected"}; running in diagnostic session: ${report.runningForProbeSession ? "yes" : "no"}. ${report.browserName ? "Browser: " + report.browserName + ". " : ""}${report.note}` }));
    } catch (error) {
      setBrowserChecks((current) => ({...current, [node.id]:"Browser probe could not be verified: " + errorText(error)}));
    } finally { setBusy(false); }
  }
  async function checkCapacity(node: RemoteNode) {
    setBusy(true);
    setCapacityChecks((current) => ({...current, [node.id]: "Inspecting remote native capacity…" }));
    try {
      const report = await invoke<CapacityProbe>("inspect_remote_machine_capacity", { nodeId: node.id });
      setCapacityChecks((current) => ({...current, [node.id]:
        `Observed ${report.platform} / ${report.architecture} at ${new Date(report.checkedAt * 1000).toLocaleTimeString()}. Available RAM: ${displayBytes(report.memoryAvailableBytes)}; workspace disk: ${displayBytes(report.workspaceDiskAvailableBytes)}; detected GPU: ${report.gpuAvailable ? "yes" : "not verified"} (${report.gpuDeviceCount} devices). ${report.note}` }));
    } catch (error) {
      setCapacityChecks((current) => ({...current, [node.id]:
        "Remote native capacity could not be verified: " + errorText(error) }));
    } finally { setBusy(false); }
  }
  async function checkRemoteJobs(node: RemoteNode) {
    setBusy(true); setNotice("");
    try {
      const result = await invoke<RemoteJobPreview>("inspect_remote_machine_jobs", {nodeId: node.id});
      setRemoteJobs((current) => ({...current, [node.id]: result}));
      setNotice("Read-only remote process snapshot received. No jobs were started or stopped.");
    } catch (error) {
      setRemoteJobs((current) => { const next = {...current}; delete next[node.id]; return next; });
      setNotice("Could not inspect remote processes: " + errorText(error));
    } finally { setBusy(false); }
  }
  async function inspectSavedNativeJob(run: NativeMatrixRun, job: NativeMatrixJob, index: number) {
    const key = `${run.id}:${index}`;
    if (!job.processId || !run.dispatchComplete) {
      setNativeJobObservations((current) => ({...current,[key]:
        "This job has no settled remote process receipt; inspect the original dispatch outcome before taking further action."}));
      return;
    }
    setBusy(true);
    try {
      const updated = await invoke<NativeMatrixRun>("reconcile_remote_native_test_job_desktop", {
        runId:run.id, jobIndex:index, expectedProcessId:job.processId,
      });
      const observed = updated.jobs[index];
      if (updated.id !== run.id || updated.localWorkspaceId !== run.localWorkspaceId
          || !observed || observed.processId !== job.processId
          || observed.nodeId !== job.nodeId || observed.remoteWorkspaceId !== job.remoteWorkspaceId) {
        throw new Error("The recorded native test identity changed during inspection.");
      }
      setNativeRuns((current) => current.map((item) => item.id === run.id ? updated : item));
      setNativeJobObservations((current) => ({...current,[key]:
        observed.state.startsWith("cancel")
          ? `Independent read-only cancellation observation saved: ${observed.cancellationObservation ?? "unverified"}`
          : `Verified remote process receipt saved: ${observed.state}. Native GUI acceptance remains unverified.`}));
    } catch (error) {
      setNativeJobObservations((current) => ({...current,[key]:
        "Cannot reconcile the saved native process: " + errorText(error) + ". No remote job was started or stopped."}));
    } finally { setBusy(false); }
  }
  async function cancelSavedNativeJob(run: NativeMatrixRun, job: NativeMatrixJob, index: number) {
    if (!job.processId || !run.dispatchComplete ||
        !["submitted","running","unverified"].includes(job.state)) return;
    if (!window.confirm(
      `Send one graceful stop to process ${job.processId} on ${job.nodeId} for ${run.id}?\n\nRepoTunnel will verify the paired remote project and exact native test process, save cancellation intent BEFORE sending, and NEVER automatically retry a lost acknowledgement. This does not guarantee the remote process will stop.`
    )) return;
    setBusy(true);
    const key = `${run.id}:${index}`;
    try {
      const updated = await invoke<NativeMatrixRun>("cancel_remote_native_test_job_desktop",{
        runId:run.id,jobIndex:index,expectedProcessId:job.processId,
      });
      if (updated.id !== run.id || updated.localWorkspaceId !== run.localWorkspaceId
          || updated.jobs[index]?.processId !== job.processId
          || updated.jobs[index]?.nodeId !== job.nodeId
          || updated.jobs[index]?.remoteWorkspaceId !== job.remoteWorkspaceId) {
        throw new Error("Cancellation returned a different native process identity; inspect the saved run.");
      }
      setNativeRuns((current) => current.map((item) => item.id === run.id ? updated : item));
      setNativeJobObservations((current) => ({...current,[key]:
        `Graceful stop attempt recorded as ${updated.jobs[index].state}. ${updated.jobs[index].evidence}`}));
    } catch (error) {
      // The intent may already be durable, including if network or UI receipt
      // failed. Refresh local records rather than offering another blind stop.
      try { setNativeRuns(await invoke<NativeMatrixRun[]>("list_remote_native_test_runs_desktop")); } catch { /* Do not overwrite failure explanation. */ }
      setNativeJobObservations((current) => ({...current,[key]:
        "Stop attempt failed or its outcome is uncertain: " + errorText(error) + ". Inspect the saved status; do not retry blindly."}));
    } finally { setBusy(false); }
  }
  async function previewSourceDiff(node: RemoteNode, continuation: boolean, requestedPages = 1) {
    const project = diffLocalProjects[node.id];
    const prior = continuation ? sourceAudits[node.id] : undefined;
    if (!project || (continuation && (!prior || prior.complete || prior.localWorkspaceId !== project))) {
      setNotice("Choose an approved local project and start a fresh source comparison.");
      return;
    }
    setBusy(true); setNotice("");
    stopSourceScan.current = false;
    if (requestedPages > 1) setScanningSourceNode(node.id);
    if (!continuation) {
      invalidateSourceReview(node.id);
      setSourceWatches((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceWatchReports((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceAudits((current) => { const next = {...current}; delete next[node.id]; return next; });
      setSourceAuditProgress((current) => { const next = {...current}; delete next[node.id]; return next; });
      setSourceIssuePages((current) => { const next = {...current}; delete next[node.id]; return next; });
      setSourceReviewChoices((current) => { const next = {...current}; delete next[node.id]; return next; });
    }
    try {
      // Multiple pages in one human-triggered scan, never unbounded polling.
      // Every response is independently checked before its cursor can advance.
      let previous = prior;
      let previousProgress = continuation ? sourceAuditProgress[node.id] : undefined;
      for (let page = 0; page < Math.min(requestedPages, 8); page++) {
        if (page > 0 && stopSourceScan.current) break;
        const result = await invoke<SourceAudit>("preview_remote_source_diff_desktop", {
          localWorkspaceId: project,
          nodeId: node.id,
          expectedSourceSha256: previous?.sourceFingerprint ?? null,
          cursorIndex: previous?.nextCursorIndex ?? 0,
          expectedMachineIdentity:node.identityFingerprint,
          expectedConnectionTime:node.connectedAt,
          expectedConnectionNonce:node.connectionNonce,
        });
      if (!sameSourcePairing(node) || latestDiffProjects.current[node.id] !== project) {
        throw new Error("Paired machine connection or approved local project changed while a source page was in flight.");
      }
      if (!result || !Array.isArray(result.rows)
          || !sourceShaPattern.test(result.sourceFingerprint)
          || !Number.isSafeInteger(result.totalSourceFiles)
          || result.totalSourceFiles < 0
          || !Number.isSafeInteger(result.cursorIndex)
          || !Number.isSafeInteger(result.nextCursorIndex)
          || !result.rows.every(validSourceAuditRow)
          || typeof result.complete !== "boolean"
          || typeof result.sourceVerified !== "boolean"
          || result.localWorkspaceId !== project || result.nodeId !== node.id
          || result.remoteWorkspaceId !== node.remoteWorkspaceId
          || result.cursorIndex !== (previous?.nextCursorIndex ?? 0)
          || (previous && (result.sourceFingerprint !== previous.sourceFingerprint
            || result.totalSourceFiles !== previous.totalSourceFiles))
          || result.nextCursorIndex < result.cursorIndex
          || result.nextCursorIndex > result.totalSourceFiles
          || result.nextCursorIndex - result.cursorIndex !== result.rows.length
          || (!result.complete && result.nextCursorIndex === result.cursorIndex)
          || (!result.complete && result.sourceVerified)
          || result.rows.length > 8
          || result.totalSourceFiles > MAX_REVIEW_ISSUES
          || result.complete !== (result.nextCursorIndex === result.totalSourceFiles)) {
        throw new Error("Remote source comparison response did not match the selected project, file inventory or cursor.");
      }
      if (previous && (!previousProgress
          || previousProgress.inspected !== result.cursorIndex
          || previousProgress.pairedMachineFingerprint !== node.identityFingerprint
          || previousProgress.pairedConnectedAt !== node.connectedAt
          || previousProgress.pairedConnectionNonce !== node.connectionNonce)) {
        throw new Error("Source scan history was reset; restart the comparison from the first page.");
      }
      const seenPaths = new Set(previousProgress?.seenPaths ?? []);
      for (const row of result.rows) {
        const canonicalPath = row.path.normalize("NFC").toLowerCase();
        if (seenPaths.has(canonicalPath)) {
          throw new Error("Source comparison repeated or case-aliased a path across pages.");
        }
        seenPaths.add(canonicalPath);
      }
      if (seenPaths.size !== result.nextCursorIndex) {
        throw new Error("Source comparison path count differs from its verified cursor.");
      }
      const inspected = (previousProgress?.inspected ?? 0) + result.rows.length;
      const issues = result.rows.filter((row) => row.status !== "identical");
      if ((previousProgress?.issues.length ?? 0) + issues.length > MAX_REVIEW_ISSUES) {
        throw new Error("Conflict review exceeded the bounded source inventory; restart the comparison.");
      }
      const nextProgress: SourceAuditProgress = {
        inspected,
        identical: (previousProgress?.identical ?? 0) + result.rows.filter((row) => row.status === "identical").length,
        missing: (previousProgress?.missing ?? 0) + result.rows.filter((row) => row.status === "missing").length,
        conflicts: (previousProgress?.conflicts ?? 0) + result.rows.filter((row) => row.status === "conflict").length,
        unverified: (previousProgress?.unverified ?? 0) + result.rows.filter((row) => row.status === "unverified").length,
        issues: [...(previousProgress?.issues ?? []), ...issues],
        seenPaths: Array.from(seenPaths),
        pairedMachineFingerprint: node.identityFingerprint,
        pairedConnectedAt: node.connectedAt,
        pairedConnectionNonce: node.connectionNonce,
      };
      setSourceAudits((current) => ({...current, [node.id]: result}));
      setSourceAuditProgress((current) => ({...current, [node.id]: nextProgress}));
      previous = result;
      previousProgress = nextProgress;
      if (result.complete) break;
      }
      setNotice(stopSourceScan.current
        ? "Read-only source scan stopped after its latest verified page. Resume with the next cursor; no files were changed."
        : "Read-only source SHA-256 comparison completed for the verified pages. No files were transferred or changed.");
    } catch (error) {
      // A failed continuation may indicate source drift; do not leave the
      // prior partial result on screen as though it were still current.
      setSourceAudits((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceAuditProgress((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceIssuePages((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceReviewChoices((current) => { const next={...current}; delete next[node.id]; return next; });
      setNotice("Source comparison failed: " + errorText(error) + ". No files were modified. Restart the read-only comparison.");
    } finally { setScanningSourceNode(null); setBusy(false); }
  }
  async function preflightNextCreateOnly(node: RemoteNode) {
    const audit=sourceAudits[node.id];
    const progress=sourceAuditProgress[node.id];
    const project=diffLocalProjects[node.id];
    if (!audit || !audit.complete || !progress
        || progress.inspected !== audit.totalSourceFiles
        || progress.pairedMachineFingerprint !== node.identityFingerprint
        || progress.pairedConnectedAt !== node.connectedAt
        || progress.pairedConnectionNonce !== node.connectionNonce
        || audit.localWorkspaceId !== project
        || audit.remoteWorkspaceId !== node.remoteWorkspaceId) {
      setNotice("Complete a current fully verified inventory scan before create-only preflight.");
      return;
    }
    const proposed=progress.issues.filter((row)=>
      row.status==="missing" && sourceReviewChoices[node.id]?.[row.path]==="proposeCreate");
    if (!proposed.length) {
      setNotice("Mark a missing file as proposed create-only before read-only preflight.");
      return;
    }
    const prior=createOnlyPreflights[node.id];
    if (prior && (prior.localWorkspaceId !== project
      || prior.remoteWorkspaceId !== node.remoteWorkspaceId
      || prior.machineIdentityFingerprint !== node.identityFingerprint
      || prior.connectedAt !== node.connectedAt
      || prior.connectionNonce !== node.connectionNonce
      || prior.sourceFingerprint !== audit.sourceFingerprint
      || prior.checkedPaths.some((path,index)=>proposed[index]?.path!==path))) {
      setCreateOnlyPreflights((current)=>{const next={...current};delete next[node.id];return next;});
      setNotice("Selected source proposals or pairing changed; restart the preflight.");
      return;
    }
    const reviewEpoch = sourceReviewEpoch.current[node.id] ?? 0;
    const cursor=prior?.checkedPaths.length ?? 0;
    const next=proposed.slice(cursor,cursor+8);
    if (!next.length) return;
    setBusy(true);
    try {
      const receipt=await invoke<CreateOnlyPreflight>("preflight_remote_create_only_desktop",{
        localWorkspaceId:project,nodeId:node.id,
        expectedSourceSha256:audit.sourceFingerprint,
        expectedMachineIdentity:node.identityFingerprint,
        expectedConnectionTime:node.connectedAt,
        expectedConnectionNonce:node.connectionNonce,
        proposed:next.map((row)=>({path:row.path,sha256:row.localSha256,bytes:row.localBytes})),
      });
      if (!sameSourcePairing(node) || latestDiffProjects.current[node.id] !== project
          || sourceReviewEpoch.current[node.id] !== reviewEpoch) {
        throw new Error("Paired source/project or human review choices changed during create-only preflight. Discard the stale response.");
      }
      if (!receipt || receipt.nodeId!==node.id
          || receipt.localWorkspaceId!==project
          || receipt.remoteWorkspaceId!==node.remoteWorkspaceId
          || receipt.machineIdentityFingerprint!==node.identityFingerprint
          || receipt.connectedAt!==node.connectedAt
          || receipt.connectionNonce!==node.connectionNonce
          || receipt.sourceFingerprint!==audit.sourceFingerprint
          || receipt.inspected!==next.length
          || !Array.isArray(receipt.proposed) || receipt.proposed.length!==next.length
          || receipt.proposed.some((item,index)=>
            item.path!==next[index].path || item.sha256!==next[index].localSha256
            || item.bytes!==next[index].localBytes)) {
        throw new Error("Preflight result did not match the exact selected source and pairing.");
      }
      const checkedPaths=[...(prior?.checkedPaths??[]),...next.map((row)=>row.path)];
      setCreateOnlyPreflights((current)=>({...current,[node.id]:{
        nodeId:node.id,localWorkspaceId:project,
        remoteWorkspaceId:node.remoteWorkspaceId,
        machineIdentityFingerprint:node.identityFingerprint,
        connectedAt:node.connectedAt,
        connectionNonce:node.connectionNonce,
        sourceFingerprint:audit.sourceFingerprint,
        checkedPaths,
      }}));
      setNotice(`Rechecked ${checkedPaths.length}/${proposed.length} proposed create-only source files read-only. No transfer approval or write was granted.`);
    } catch(error) {
      setCreateOnlyPreflights((current)=>{const next={...current};delete next[node.id];return next;});
      setNotice("Create-only preflight failed: "+errorText(error)+
        ". Previous receipts invalidated. Restart the read-only comparison if source or pairing changed.");
    } finally {setBusy(false);}
  }
  async function recordHumanSourceReview(node: RemoteNode) {
    const audit = sourceAudits[node.id];
    const progress = sourceAuditProgress[node.id];
    const checked = createOnlyPreflights[node.id];
    const project = diffLocalProjects[node.id];
    if (!audit?.complete || !progress || progress.inspected !== audit.totalSourceFiles
      || !checked || checked.localWorkspaceId !== project
      || checked.remoteWorkspaceId !== node.remoteWorkspaceId
      || checked.machineIdentityFingerprint !== node.identityFingerprint
      || checked.connectedAt !== node.connectedAt
      || checked.connectionNonce !== node.connectionNonce
      || checked.sourceFingerprint !== audit.sourceFingerprint) {
      setNotice("An exact, complete source scan and fresh create-only preflight are required.");
      return;
    }
    const chosen = progress.issues.filter((row) =>
      row.status === "missing" && sourceReviewChoices[node.id]?.[row.path] === "proposeCreate");
    const batch = chosen.slice(0, 8);
    if (!batch.length || batch.some((row, i) => checked.checkedPaths[i] !== row.path)) {
      setNotice("Preflight the exact first reviewed create-only batch before recording intent.");
      return;
    }
    const proposed = batch.map((row) => ({
      path: row.path, sha256: row.localSha256, bytes: row.localBytes,
    }));
    const summary = [
      "Record this exact SOURCE REVIEW INTENT (not transfer authorization)?",
      "Destination machine SHA-256: " + node.identityFingerprint,
      "Connection generation: " + node.connectionNonce,
      "Local project: " + project,
      "Destination project: " + node.remoteWorkspaceId,
      "Source inventory SHA-256: " + audit.sourceFingerprint,
      "",
      ...proposed.map((file) => file.path + " · " + file.bytes + " bytes · SHA-256 " + file.sha256),
      "",
      "This only saves local evidence. No bytes are transferred. Receiver-side approval and a separate one-shot mutation workflow are still required.",
    ].join("\n");
    if (!window.confirm(summary)) return;
    const revision = sourceReviewEpoch.current[node.id] ?? 0;
    setBusy(true);
    try {
      // The native command independently recomputes the source inventory
      // and checks every receiver path's missing status on the pinned pairing.
      const receipt = await invoke<HumanSourceIntent>("record_remote_source_review_intent_desktop", {
        localWorkspaceId:project, nodeId:node.id,
        expectedSourceSha256:audit.sourceFingerprint,
        expectedMachineIdentity:node.identityFingerprint,
        expectedConnectionTime:node.connectedAt,
        expectedConnectionNonce:node.connectionNonce,
        proposed,
      });
      if (!receipt || receipt.state !== "awaitingReceiverApproval"
        || receipt.localWorkspaceId !== project || receipt.remoteWorkspaceId !== node.remoteWorkspaceId
        || receipt.nodeId !== node.id || receipt.machineIdentityFingerprint !== node.identityFingerprint
        || receipt.connectedAt !== node.connectedAt || receipt.connectionNonce !== node.connectionNonce
        || receipt.sourceFingerprint !== audit.sourceFingerprint
        || !Array.isArray(receipt.proposed) || receipt.proposed.length !== proposed.length
        || receipt.proposed.some((item, i) => item.path !== proposed[i].path
          || item.sha256 !== proposed[i].sha256 || item.bytes !== proposed[i].bytes)) {
        throw new Error("Persisted source review receipt did not match the confirmed identities.");
      }
      // The durable evidence still exists if pairing or choices changed while
      // the request was in flight, but must not be presented as current consent.
      const stillCurrent = sameSourcePairing(node)
        && latestDiffProjects.current[node.id] === project
        && sourceReviewEpoch.current[node.id] === revision;
      setHumanSourceIntents((current) =>
        current.some((entry) => entry.id === receipt.id) ? current : [...current, receipt]);
      setNotice(stillCurrent
        ? "Reviewed create-only file intent saved locally. Awaiting receiver authorization; no file transfer started."
        : "Source review intent was saved, but the pairing or choices changed. Treat it as historical evidence only; rescan before any future transfer.");
    } catch (error) {
      // An acknowledgement can be lost after an on-disk save. READ the local
      // journal instead of blindly issuing a second intent creation request.
      try {
        const saved = await invoke<HumanSourceIntent[]>("list_remote_source_review_intents_desktop");
        if (Array.isArray(saved)) setHumanSourceIntents(saved);
      } catch { /* Keep the uncertainty visible; never retry the mutation. */ }
      setNotice("Saving source review intent failed or returned an uncertain receipt: "
        + errorText(error) + ". Inspect the local intent journal; do not retry blindly. No remote import was sent.");
    } finally { setBusy(false); }
  }
  async function inspectSavedHumanSourceReview(intent: HumanSourceIntent) {
    const node=nodes.find((item)=>item.id===intent.nodeId
      && item.remoteWorkspaceId===intent.remoteWorkspaceId
      && item.identityFingerprint===intent.machineIdentityFingerprint
      && item.connectedAt===intent.connectedAt
      && item.connectionNonce===intent.connectionNonce);
    if (!node || intent.state !== "awaitingReceiverApproval") {
      setNotice("This saved review belongs to a disconnected, withdrawn, or re-paired generation. Read-only reconciliation must not migrate to another pairing.");
      return;
    }
    setBusy(true);
    setSourceIntentObservations((current) => {
      const next={...current}; delete next[intent.id]; return next;
    });
    try {
      const receipt=await invoke<SourceIntentObservation>(
        "reconcile_remote_source_review_intent_desktop",{intentId:intent.id});
      if (!sameSourcePairing(node)
        || !receipt || receipt.intentId!==intent.id
        || receipt.nodeId!==intent.nodeId
        || receipt.localWorkspaceId!==intent.localWorkspaceId
        || receipt.remoteWorkspaceId!==intent.remoteWorkspaceId
        || receipt.machineIdentityFingerprint!==intent.machineIdentityFingerprint
        || receipt.connectedAt!==intent.connectedAt
        || receipt.connectionNonce!==intent.connectionNonce
        || receipt.sourceFingerprint!==intent.sourceFingerprint
        || !Array.isArray(receipt.rows) || receipt.rows.length!==intent.proposed.length
        || receipt.rows.some((row,index)=>!validSourceAuditRow(row)
          || row.path!==intent.proposed[index].path
          || row.localSha256!==intent.proposed[index].sha256
          || row.localBytes!==intent.proposed[index].bytes)) {
        throw new Error("Read-only saved proposal status changed its source, pairing or verified per-file identities.");
      }
      setSourceIntentObservations((current)=>({...current,[intent.id]:receipt}));
      setNotice("Reconciled saved source paths read-only on the ORIGINAL pairing. Matching bytes do not prove an approved or completed import.");
    } catch(error) {
      setSourceIntentObservations((current)=>{const next={...current};delete next[intent.id];return next;});
      setNotice("Saved source proposal read-only reconciliation failed: "+errorText(error)
        + ". No remote write was requested. Inspect pairing, source changes and preserved intent history.");
    } finally {setBusy(false);}
  }
  async function copyIncomingReviewProposal(intent: HumanSourceIntent) {
    if(intent.state!=="awaitingReceiverApproval" || !sourceShaPattern.test(localIdentity)
      || !nodes.some((node)=>node.id===intent.nodeId
        && node.remoteWorkspaceId===intent.remoteWorkspaceId
        && node.identityFingerprint===intent.machineIdentityFingerprint
        && node.connectionNonce===intent.connectionNonce
        && node.connectedAt===intent.connectedAt)){
      setNotice("Only the exact still-connected original source pairing may export a new signed review.");
      return;
    }
    const originalNode=nodes.find((node)=>node.id===intent.nodeId);
    setBusy(true);
    try {
      // The native command independently rechecks the original source bytes,
      // pinned receiver, complete inventory and every positive-missing file,
      // then signs the exact manifest with the installation's protected key.
      const draft=await invoke<InboundReviewDraft>(
        "export_signed_incoming_source_review_desktop",{intentId:intent.id});
      if(!draft || draft.schemaVersion!==1 || draft.kind!=="RepoTunnelIncomingSourceReviewV1"
        || draft.remoteWorkspaceId!==intent.remoteWorkspaceId
        || draft.receiverMachineFingerprint!==intent.machineIdentityFingerprint
        || draft.coordinatorMachineFingerprint!==localIdentity
        || draft.coordinatorWorkspaceId!==intent.localWorkspaceId
        || draft.coordinatorConnectionNonce!==intent.connectionNonce
        || draft.coordinatorSourceFingerprint!==intent.sourceFingerprint
        || draft.sourceReviewId!==intent.id
        || !Array.isArray(draft.files) || draft.files.length!==intent.proposed.length
        || draft.files.some((file,i)=>file.path!==intent.proposed[i].path
          || file.sha256!==intent.proposed[i].sha256
          || file.bytes!==intent.proposed[i].bytes)
        || typeof draft.identityPublicKey!=="string" || draft.identityPublicKey.length!==44
        || typeof draft.identitySignature!=="string" || draft.identitySignature.length!==88
        || !originalNode || !sameSourcePairing(originalNode)){
        throw new Error("Signed source export returned a stale pairing, changed selection or invalid proof envelope.");
      }
      await navigator.clipboard.writeText(JSON.stringify(draft,null,2));
      setNotice("Copied installation-SIGNED exact source review. Verify both installation key fingerprints on the receiver before independently authorizing. The signature proves the coordinator key signed these bytes, not permission to import.");
    }catch(error){
      setNotice("Could not export current signed source review: "+errorText(error)
        + ". Restart the read-only source review if the pairing or inventory changed.");
    }finally{setBusy(false);}
  }

  async function approveIncomingSourceReview() {
    if (incomingReviewDraft.length > 32_768) {
      setNotice("Incoming source review JSON exceeds the bounded 32 KiB human review limit.");
      return;
    }
    let draft:InboundReviewDraft;
    try {
      draft=JSON.parse(incomingReviewDraft) as InboundReviewDraft;
      if (!draft || draft.schemaVersion!==1 || draft.kind!=="RepoTunnelIncomingSourceReviewV1"
        || typeof draft.remoteWorkspaceId!=="string"
        || !sourceShaPattern.test(draft.receiverMachineFingerprint)
        || draft.receiverMachineFingerprint!==localIdentity
        || !sourceShaPattern.test(draft.coordinatorMachineFingerprint)
        || typeof draft.identityPublicKey!=="string" || draft.identityPublicKey.length!==44
        || typeof draft.identitySignature!=="string" || draft.identitySignature.length!==88
        || !sourceShaPattern.test(draft.coordinatorSourceFingerprint)
        || typeof draft.coordinatorWorkspaceId!=="string"
        || !/^node-[a-f0-9]{24}$/.test(draft.coordinatorConnectionNonce)
        || !/^source-review-[a-f0-9]{32}$/.test(draft.sourceReviewId)
        || !Array.isArray(draft.files) || draft.files.length<1 || draft.files.length>8
        || draft.files.some(file=>typeof file?.path!=="string" || file.path.length>512
          || file.path.length===0 || file.path.startsWith("/")
          || file.path.includes("\\") || /[\x00-\x1f\x7f]/.test(file.path)
          || file.path.split("/").some(seg=>!seg||seg==="."||seg==="..")
          || !sourceShaPattern.test(file.sha256)
          || !Number.isSafeInteger(file.bytes) || file.bytes<0 || file.bytes>64*1024*1024)
        || new Set(draft.files.map(file=>file.path.toLowerCase())).size!==draft.files.length) {
        throw new Error("Invalid or aliased incoming source review JSON.");
      }
    } catch(error) {
      setNotice("Incoming source review proposal invalid: "+errorText(error));
      return;
    }
    const grant=activeGrants.find(item=>item.fingerprint===incomingCredentialFingerprint
      && item.workspaceId===draft.remoteWorkspaceId
      && item.expiresAt>Date.now()/1000 && item.scopes.includes("source"));
    if(!grant){
      setNotice("Select an unexpired receiver-issued credential with source scope for the proposal's exact destination project.");
      return;
    }
    const proposed=draft.files.map(file=>({path:file.path,sha256:file.sha256,bytes:file.bytes}));
    const summary=[
      "Approve ONLY this incoming create-only SOURCE REQUEST on this receiving installation?",
      "Receiver workspace: "+grant.workspaceId,
      "Receiver installation SHA-256: "+draft.receiverMachineFingerprint,
      "Credential SHA-256: "+grant.fingerprint,
      "Coordinator machine signing SHA-256: "+draft.coordinatorMachineFingerprint,
      "Coordinator source project: "+draft.coordinatorWorkspaceId,
      "Original connection: "+draft.coordinatorConnectionNonce,
      "Human review ID: "+draft.sourceReviewId,
      "Source fingerprint: "+draft.coordinatorSourceFingerprint,
      "Coordinator review Ed25519 public key: "+draft.identityPublicKey,
      "Signed proof: "+draft.identitySignature,
      ...proposed.map(file=>file.path+" · "+file.bytes+" bytes · SHA-256 "+file.sha256),
      "This independently stores a receiver authorization receipt; it DOES NOT transfer, stage, install, or overwrite files. Manual cross-check of original machine identity is required."
    ].join("\n");
    if(!window.confirm(summary)) return;
    const request={
      workspaceId:grant.workspaceId,
      credentialFingerprint:grant.fingerprint,
      coordinatorMachineFingerprint:draft.coordinatorMachineFingerprint,
      coordinatorWorkspaceId:draft.coordinatorWorkspaceId,
      coordinatorConnectionNonce:draft.coordinatorConnectionNonce,
      coordinatorSourceFingerprint:draft.coordinatorSourceFingerprint,
      sourceReviewId:draft.sourceReviewId,
      files:proposed,
      receiverMachineFingerprint:draft.receiverMachineFingerprint,
      identityPublicKey:draft.identityPublicKey,
      identitySignature:draft.identitySignature,
    };
    setBusy(true);
    try {
      const receipt=await invoke<ReceiverSourceApproval>("approve_incoming_source_review_desktop",{request});
      if(!receipt || receipt.state!=="approved" || receipt.workspaceId!==request.workspaceId
        || receipt.credentialFingerprint!==request.credentialFingerprint
        || receipt.coordinatorMachineFingerprint!==request.coordinatorMachineFingerprint
        || receipt.coordinatorWorkspaceId!==request.coordinatorWorkspaceId
        || receipt.coordinatorConnectionNonce!==request.coordinatorConnectionNonce
        || receipt.coordinatorSourceFingerprint!==request.coordinatorSourceFingerprint
        || receipt.sourceReviewId!==request.sourceReviewId
        || receipt.receiverMachineFingerprint!==request.receiverMachineFingerprint
        || receipt.identityPublicKey!==request.identityPublicKey
        || receipt.identitySignature!==request.identitySignature
        || !/^receiver-source-[a-f0-9]{32}$/.test(receipt.id)
        || !Array.isArray(receipt.files) || receipt.files.length!==proposed.length
        || receipt.files.some((file,i)=>file.path!==proposed[i].path
          || file.sha256!==proposed[i].sha256 || file.bytes!==proposed[i].bytes)) {
        throw new Error("Receiver saved approval receipt does not match confirmed files or pairing.");
      }
      setIncomingSourceApprovals((current)=>current.some(value=>value.id===receipt.id)
        ?current:[...current,receipt]);
      setNotice("Receiver created an independent exact-file approval receipt. No remote file was transferred or installed; a separately authorized one-shot import dispatcher remains required.");
    }catch(error){
      try{
        const saved=await invoke<ReceiverSourceApproval[]>("list_incoming_source_approvals_desktop");
        if(Array.isArray(saved)) setIncomingSourceApprovals(saved);
      }catch{/* Do not replay an uncertain approval. */}
      setNotice("Receiver authorization save failed or ACK was lost: "+errorText(error)
        + ". Inspect saved receiver approvals; no automatic replay or remote write was attempted.");
    }finally{setBusy(false);}
  }

  const incomingApprovalEpoch = useRef<Record<string,number>>({});
  async function inspectIncomingApproval(approval:ReceiverSourceApproval) {
    const currentEpoch=(incomingApprovalEpoch.current[approval.id]??0)+1;
    incomingApprovalEpoch.current[approval.id]=currentEpoch;
    setIncomingApprovalChecks(current=>{const next={...current};delete next[approval.id];return next;});
    setBusy(true);
    try{
      const result=await invoke<ReceiverApprovalInspection>("inspect_incoming_source_approval_desktop",
        {approvalId:approval.id});
      if(incomingApprovalEpoch.current[approval.id]!==currentEpoch)return;
      if(!result || result.approvalId!==approval.id || result.state!==approval.state
        || !Array.isArray(result.files)||result.files.length!==approval.files.length
        || result.files.some((file,index)=>file.path!==approval.files[index].path
          || file.expectedSha256!==approval.files[index].sha256
          || file.expectedBytes!==approval.files[index].bytes
          || !["missing","matchingBytes","conflict","unverified"].includes(file.status))) {
        throw new Error("Receiver freshness result does not match the saved exact source approval.");
      }
      setIncomingApprovalChecks(current=>({...current,[approval.id]:result}));
      setNotice("Receiver approval checked read-only. Missing destinations do not authorize a transfer; an atomic create-only import is still required.");
    }catch(error){
      setNotice("Receiver approval inspection failed: "+errorText(error));
    }finally{setBusy(false);}
  }

  async function withdrawIncomingApproval(approval:ReceiverSourceApproval) {
    if(approval.state!=="approved") return;
    if(!window.confirm("Permanently withdraw receiver-side source approval?\n"
      +approval.id+"\nNo source import is started or replayed.")) return;
    incomingApprovalEpoch.current[approval.id]=(incomingApprovalEpoch.current[approval.id]??0)+1;
    setIncomingApprovalChecks(current=>{const next={...current};delete next[approval.id];return next;});
    setBusy(true);
    try {
      const receipt=await invoke<ReceiverSourceApproval>("withdraw_incoming_source_approval_desktop",
        {approvalId:approval.id});
      if(!receipt || receipt.id!==approval.id || receipt.state!=="withdrawn"){
        throw new Error("Receiver withdrawal acknowledgement did not match.");
      }
      setIncomingSourceApprovals(current=>current.map(item=>item.id===receipt.id?receipt:item));
      setNotice("Independent receiver approval withdrawn. No remote import was executed.");
    }catch(error){
      try{
        const list=await invoke<ReceiverSourceApproval[]>("list_incoming_source_approvals_desktop");
        if(Array.isArray(list)) setIncomingSourceApprovals(list);
      }catch{/* No uncertain local writes are replayed. */}
      setNotice("Receiver withdrawal outcome may be uncertain: "+errorText(error)
        + ". Inspect the saved local receiver journal, never automatically replay.");
    }finally{setBusy(false);}
  }

  async function inspectReceiverApprovalFromOriginalReview(intent: HumanSourceIntent) {
    const approvalId=(receiverApprovalIds[intent.id] ?? "").trim();
    const node=nodes.find((item)=>item.id===intent.nodeId
      && item.remoteWorkspaceId===intent.remoteWorkspaceId
      && item.identityFingerprint===intent.machineIdentityFingerprint
      && item.connectedAt===intent.connectedAt
      && item.connectionNonce===intent.connectionNonce);
    if (!node || intent.state!=="awaitingReceiverApproval"
      || !/^receiver-source-[a-f0-9]{32}$/.test(approvalId)) {
      setNotice("The original machine generation and complete receiver approval ID are required for read-only inspection.");
      return;
    }
    const revision=sourceReviewEpoch.current[intent.nodeId] ?? 0;
    const receiptEpoch=receiverApprovalEpoch.current[intent.id] ?? 0;
    setBusy(true);
    setRemoteReceiverApprovals((current)=>{
      const next={...current};delete next[intent.id];return next;
    });
    try {
      const result=await invoke<ReceiverReviewRemoteInspection>(
        "inspect_remote_receiver_source_approval_desktop",{
          intentId:intent.id,approvalId,
        });
      const binding=result?.reviewBinding;
      if(!result || result.approvalId!==approvalId
        || !["approved","withdrawn","legacyUnsigned","dispatchUnknown","imported"].includes(result.state)
        || !binding || binding.sourceReviewId!==intent.id
        || binding.remoteWorkspaceId!==intent.remoteWorkspaceId
        || binding.receiverMachineFingerprint!==intent.machineIdentityFingerprint
        || binding.coordinatorMachineFingerprint!==localIdentity
        || binding.coordinatorWorkspaceId!==intent.localWorkspaceId
        || binding.coordinatorConnectionNonce!==intent.connectionNonce
        || binding.coordinatorSourceFingerprint!==intent.sourceFingerprint
        || typeof result.humanImportSupported!=="boolean"
        || !Array.isArray(result.files) || result.files.length!==intent.proposed.length
        || result.files.some((file,index)=>file.path!==intent.proposed[index].path
          || file.expectedSha256!==intent.proposed[index].sha256
          || file.expectedBytes!==intent.proposed[index].bytes
          || !["missing","matchingBytes","conflict","unverified"].includes(file.status))
        || !sameSourcePairing(node) || !computersMounted.current
        || (sourceReviewEpoch.current[intent.nodeId]??0)!==revision
        || (receiverApprovalEpoch.current[intent.id]??0)!==receiptEpoch
        || (receiverApprovalIds[intent.id]??"").trim()!==approvalId
        || (result.state!=="approved" && result.canStillProposeCreateOnly)) {
        throw new Error("Receiver approval lookup was stale, substituted or invalid; discard its evidence.");
      }
      setRemoteReceiverApprovals((current)=>({...current,[intent.id]:result}));
      setNotice("Original receiver approval inspected read-only on the pinned machine session. This does not authorize or start any file import.");
    } catch(error){
      setRemoteReceiverApprovals((current)=>{
        const next={...current};delete next[intent.id];return next;
      });
      setNotice("Receiver approval status could not be verified: "+errorText(error)
        + ". No source transfer or mutation was sent.");
    } finally {setBusy(false);}
  }

  async function verifyHumanImportOnReceiver(intent: HumanSourceIntent) {
    const approvalId=intent.importAttempt?.approvalId
      ?? (receiverApprovalIds[intent.id]??"").trim();
    const node=nodes.find((item)=>item.id===intent.nodeId
      && item.remoteWorkspaceId===intent.remoteWorkspaceId
      && item.identityFingerprint===intent.machineIdentityFingerprint
      && item.connectedAt===intent.connectedAt
      && item.connectionNonce===intent.connectionNonce);
    if (!node || !sameSourcePairing(node) || intent.state!=="awaitingReceiverApproval"
      || !/^receiver-source-[a-f0-9]{32}$/.test(approvalId)) {
      setNotice("The original paired receiver and exact approval are needed for read-only import reconciliation.");
      return;
    }
    const epoch=(receiverApprovalEpoch.current[intent.id]??0);
    setBusy(true);
    setHumanImportVerifications((current)=>{const next={...current};delete next[intent.id];return next;});
    try {
      // This is ONLY the existing machine-authenticated receiver inspection.
      // Never call an import, create, stage, retry or reservation operation.
      const result=await invoke<ReceiverReviewRemoteInspection>(
        "inspect_remote_receiver_source_approval_desktop", {
          intentId:intent.id,approvalId,
        });
      const binding=result?.reviewBinding;
      if (!result || result.approvalId!==approvalId
        || !["approved","withdrawn","legacyUnsigned","dispatchUnknown","imported"].includes(result.state)
        || !binding || binding.sourceReviewId!==intent.id
        || binding.remoteWorkspaceId!==intent.remoteWorkspaceId
        || binding.receiverMachineFingerprint!==intent.machineIdentityFingerprint
        || binding.coordinatorMachineFingerprint!==localIdentity
        || binding.coordinatorWorkspaceId!==intent.localWorkspaceId
        || binding.coordinatorConnectionNonce!==intent.connectionNonce
        || binding.coordinatorSourceFingerprint!==intent.sourceFingerprint
        || typeof result.humanImportSupported!=="boolean"
        || !Array.isArray(result.files) || result.files.length!==intent.proposed.length
        || result.files.some((file,index)=>file.path!==intent.proposed[index].path
          || file.expectedSha256!==intent.proposed[index].sha256
          || file.expectedBytes!==intent.proposed[index].bytes
          || !["missing","matchingBytes","conflict","unverified"].includes(file.status))
        || (["imported","dispatchUnknown"].includes(result.state)
          && result.importTaskId !== (intent.importAttempt?.taskId
            ?? humanImportOutcomes[intent.id]?.taskId))
        || !sameSourcePairing(node) || !computersMounted.current
        || (receiverApprovalEpoch.current[intent.id]??0)!==epoch
        || (intent.importAttempt
          ? intent.importAttempt.approvalId!==approvalId
          : (receiverApprovalIds[intent.id]??"").trim()!==approvalId)) {
        throw new Error("Receiver read-only import evidence has changed machine, source, file identity or connection.");
      }
      const allMatch=result.files.every((file)=>file.status==="matchingBytes");
      const state = result.state==="imported"
        ? (allMatch ? "verifiedImport" : "changedAfterImport")
        : (allMatch ? "matchingWithoutReceipt" : "notVerified");
      const note = state==="verifiedImport"
        ? "Receiver has its durable imported completion receipt AND every current file matches the human-approved bytes and SHA-256."
        : state==="changedAfterImport"
          ? "Receiver recorded the completed import, but at least one approved destination is now missing, different, or unverified."
          : state==="matchingWithoutReceipt"
            ? "Current bytes match, but the receiver has NO durable imported completion receipt. This is not proof that the one-shot import succeeded."
            : "Not all approved source files match. This is read-only evidence, not permission to retry the original import.";
      setHumanImportVerifications((current)=>({...current,[intent.id]:{
        state,receiverState:result.state,files:result.files,note,
      }}));
      setNotice(note);
    } catch(error) {
      setHumanImportVerifications((current)=>{const next={...current};delete next[intent.id];return next;});
      setNotice("Receiver import reconciliation unavailable: "+errorText(error)
        + ". Preserve the one-shot attempt and inspect the original receiver locally; no automatic retry.");
    } finally {setBusy(false);}
  }

  async function reconcileProvenHumanImportLocally(intent: HumanSourceIntent) {
    const attempt=intent.importAttempt;
    if (!attempt || attempt.state!=="dispatchUnknown"
      || humanImportVerifications[intent.id]?.state!=="verifiedImport") {
      setNotice("First verify the original receiver's durable imported receipt and every file hash.");
      return;
    }
    if (!window.confirm("Record a proven imported receipt in this computer's local journal?\n"
      +"Exact original receiver approval: "+attempt.approvalId+"\n"
      +"Original one-shot transfer task: "+attempt.taskId+"\n\n"
      +"RepoTunnel will READ the receiver's durable receipt and every approved destination hash again. "
      +"It will NOT resend source bytes, execute an import or overwrite any files. "
      +"If the proof is unavailable, the local outcome stays UNKNOWN.")) return;
    setBusy(true);
    try {
      const receipt=await invoke<NonNullable<HumanSourceIntent["importAttempt"]>>(
        "reconcile_verified_remote_source_import_desktop",{intentId:intent.id});
      if (!receipt || receipt.approvalId!==attempt.approvalId
        || receipt.taskId!==attempt.taskId || receipt.state!=="imported"
        || receipt.importedFiles!==intent.proposed.length) {
        throw new Error("Local reconciliation receipt did not match the original one-shot import.");
      }
      setHumanImportOutcomes((current)=>({...current,[intent.id]:{
        approvalId:receipt.approvalId,taskId:receipt.taskId,
        state:"imported",importedFiles:receipt.importedFiles,
        note:"Original receiver completion task and every approved file hash verified again. Local sender receipt durably reconciled; no installation RPC sent.",
      }}));
      setNotice("Verified receiver import recorded in the sender journal without replaying the import.");
    } catch(error) {
      setNotice("Local import reconciliation remains uncertain: "+errorText(error)
        + ". No receiver write was sent; do not replay the one-shot import.");
    } finally {
      try {
        const saved=await invoke<HumanSourceIntent[]>("list_remote_source_review_intents_desktop");
        if(Array.isArray(saved)) setHumanSourceIntents(saved);
      } catch { /* Preserve uncertainty; do not infer local journal success. */ }
      setBusy(false);
    }
  }

  async function importHumanApprovedSource(intent: HumanSourceIntent) {
    const approvalId=(receiverApprovalIds[intent.id]??"").trim();
    const approval=remoteReceiverApprovals[intent.id];
    const node=nodes.find((n)=>n.id===intent.nodeId && n.remoteWorkspaceId===intent.remoteWorkspaceId
      && n.identityFingerprint===intent.machineIdentityFingerprint
      && n.connectedAt===intent.connectedAt && n.connectionNonce===intent.connectionNonce);
    if (!node || !sameSourcePairing(node) || intent.state!=="awaitingReceiverApproval"
      || approval?.approvalId!==approvalId || approval.state!=="approved"
      || !approval.credentialActive || !approval.humanImportSupported
      || !approval.canStillProposeCreateOnly
      || approval.files.length!==intent.proposed.length
      || approval.files.some((file,index)=>file.status!=="missing"
        || file.path!==intent.proposed[index].path
        || file.expectedSha256!==intent.proposed[index].sha256
        || file.expectedBytes!==intent.proposed[index].bytes)) {
      setNotice("Recheck the ORIGINAL receiver approval and all missing destinations before an import.");
      return;
    }
    if (!window.confirm("IMPORT exactly "+intent.proposed.length+" human-reviewed source files?\n"
      +"Receiver: "+node.label+"\nProject: "+intent.remoteWorkspaceId+"\n"
      +"Receiver approval: "+approvalId+"\n\n"
      +"EXACT approved files:\n"+intent.proposed.map((file)=>
        file.path+" · "+file.bytes+" bytes · SHA-256 "+file.sha256).join("\n")+"\n\n"
      +"This copies verified bytes to protected staging, then makes ONE irreversible create-only import. "
      +"Existing files will NEVER be overwritten. If the result is uncertain, it must NEVER be replayed.")) return;
    setBusy(true);
    setHumanImportOutcomes((current)=>{const next={...current};delete next[intent.id];return next;});
    try {
      const receipt=await invoke<{approvalId:string;state:"imported"|"dispatchUnknown";
        importedFiles:number;note:string;taskId:string}>("import_approved_remote_source_desktop", {
          intentId:intent.id,approvalId,
        });
      if (receipt.approvalId!==approvalId
        || !/^remote-sync-[a-f0-9]{32}$/.test(receipt.taskId)
        || !["imported","dispatchUnknown"].includes(receipt.state))
        throw new Error("Import result did not match an exact durable sender task or receiver one-shot receipt.");
      setHumanImportOutcomes((current)=>({...current,[intent.id]:receipt}));
      setRemoteReceiverApprovals((current)=>{const next={...current};delete next[intent.id];return next;});
      setNotice(receipt.state==="imported"
        ? "Receiver confirmed durable create-only import of "+receipt.importedFiles+" approved files. Verify receiver status read-only."
        : "Import outcome UNKNOWN. Do not retry. Inspect actual receiver source files and the saved one-shot approval.");
    } catch(error) {
      setHumanImportOutcomes((current)=>({...current,[intent.id]:{
        state:"failedBeforeInstall",importedFiles:0,
        note:"Source review or staging failed before the one-shot import request: "+errorText(error)
          +". No installation request was dispatched by this action. Inspect the original receiver stage before making any new decision.",
      }}));
      setRemoteReceiverApprovals((current)=>{const next={...current};delete next[intent.id];return next;});
      setNotice("Import preparation or staging failed before installation. Review the receiver's staged task and approval; no install was requested.");
    } finally {
      // Read the authoritative sender journal even after an ACK loss or
      // transfer failure. The persisted stage/dispatch state, not a transient
      // React flag, decides whether another import attempt is forbidden.
      try {
        const saved=await invoke<HumanSourceIntent[]>("list_remote_source_review_intents_desktop");
        if (Array.isArray(saved)) setHumanSourceIntents(saved);
      } catch {/* Preserve the visible uncertain result; never replay. */}
      setBusy(false);
    }
  }

  async function withdrawHumanSourceReview(intent: HumanSourceIntent) {
    if (intent.state !== "awaitingReceiverApproval") return;
    if (!window.confirm("Permanently withdraw this local source review proposal?\n"
      + intent.id + "\nDestination project: " + intent.remoteWorkspaceId
      + "\nThis cannot be re-created on the same pairing and does not change remote files.")) return;
    setBusy(true);
    // Invalidate pending read-only preflights before dispatching a withdrawal.
    invalidateSourceReview(intent.nodeId);
    setSourceIntentObservations((current) => {
      const next={...current}; delete next[intent.id]; return next;
    });
    setCreateOnlyPreflights((current) => {
      const next={...current};
      delete next[intent.nodeId];
      return next;
    });
    try {
      const receipt=await invoke<HumanSourceIntent>("withdraw_remote_source_review_intent_desktop",
        {intentId:intent.id});
      if (!receipt || receipt.id !== intent.id || receipt.state !== "withdrawn"
        || receipt.nodeId !== intent.nodeId
        || receipt.localWorkspaceId !== intent.localWorkspaceId
        || receipt.remoteWorkspaceId !== intent.remoteWorkspaceId
        || receipt.connectionNonce !== intent.connectionNonce
        || receipt.machineIdentityFingerprint !== intent.machineIdentityFingerprint
        || receipt.sourceFingerprint !== intent.sourceFingerprint
        || receipt.proposed.length !== intent.proposed.length
        || receipt.proposed.some((item,index) =>
          item.path !== intent.proposed[index].path ||
          item.sha256 !== intent.proposed[index].sha256 ||
          item.bytes !== intent.proposed[index].bytes)) {
        throw new Error("Withdrawal acknowledgement did not match the immutable proposal.");
      }
      setHumanSourceIntents((current)=>current.map((entry)=>entry.id===receipt.id?receipt:entry));
      setNotice("Local source review proposal withdrawn permanently; no remote file was changed.");
    } catch(error) {
      try {
        const saved=await invoke<HumanSourceIntent[]>("list_remote_source_review_intents_desktop");
        if(Array.isArray(saved)) setHumanSourceIntents(saved);
      } catch { /* Preserve outcome uncertainty; never replay the write. */ }
      setNotice("Source proposal withdrawal may already be saved: " + errorText(error)
        + ". Inspect the local journal; do not retry blindly. No remote mutation was sent.");
    } finally {setBusy(false);}
  }
  async function copySourceReviewDraft(node: RemoteNode) {
    const audit = sourceAudits[node.id];
    const progress = sourceAuditProgress[node.id];
    if (!audit || !progress || progress.inspected !== audit.nextCursorIndex
      || progress.seenPaths.length !== progress.inspected
      || progress.pairedMachineFingerprint !== node.identityFingerprint
      || progress.pairedConnectedAt !== node.connectedAt
      || progress.pairedConnectionNonce !== node.connectionNonce
      || audit.nodeId !== node.id || audit.remoteWorkspaceId !== node.remoteWorkspaceId
      || audit.localWorkspaceId !== diffLocalProjects[node.id]
      || progress.issues.length > MAX_REVIEW_ISSUES) {
      setNotice("Source review is missing or no longer bound to this exact pairing and inventory; restart the comparison.");
      return;
    }
    // A *review draft*, not a write grant or executable sync instruction.
    // Preserve all observed issues and explicit human preferences without
    // copying source bytes, tokens, or any remote file contents.
    const draft = {
      schemaVersion: 1,
      kind: "readOnlySourceConflictReview",
      nodeId: audit.nodeId,
      localWorkspaceId: audit.localWorkspaceId,
      remoteWorkspaceId: audit.remoteWorkspaceId,
      machineIdentityFingerprint: node.identityFingerprint,
      connectionNonce: node.connectionNonce,
      connectedAt: node.connectedAt,
      sourceFingerprint: audit.sourceFingerprint,
      inspectedFiles: progress.inspected,
      totalSourceFiles: audit.totalSourceFiles,
      complete: audit.complete,
      sourceVerified: audit.sourceVerified,
      requiresFreshPreflightAndSeparateApproval: true,
      canOverwriteRemoteSource: false,
      // This evidence expires whenever the source review/pairing changes and
      // never grants permission to stage, import, or overwrite.
      createOnlyPreflightCheckedPaths: createOnlyPreflights[node.id]?.checkedPaths ?? [],
      issues: progress.issues.map((row) => ({
        path: row.path,
        status: row.status,
        localSha256: row.localSha256,
        localBytes: row.localBytes,
        remoteSha256: row.remoteSha256,
        remoteBytes: row.remoteBytes,
        reviewChoice: sourceReviewChoices[node.id]?.[row.path] ?? defaultReviewChoice(row.status),
      })),
    };
    try {
      await navigator.clipboard.writeText(JSON.stringify(draft, null, 2));
      setNotice("Copied read-only review draft for " + progress.issues.length +
        " issues. This is not a transfer authorization; source changes require fresh verification and separate remote approval.");
    } catch (error) {
      setNotice("Unable to copy source review draft: " + errorText(error));
    }
  }
  async function recheckSourceParity(node: RemoteNode) {
    invalidateSourceReview(node.id);
    const prior = sourceAudits[node.id];
    const project = diffLocalProjects[node.id];
    if (!prior?.complete || !project || prior.localWorkspaceId !== project) {
      setNotice("Complete the source comparison for this approved project before checking aggregate parity.");
      return;
    }
    setBusy(true);
    try {
      // Cursor == total intentionally skips per-file requests; the backend
      // still reads a fresh local manifest and the remote aggregate metadata,
      // and refuses a changed local source fingerprint.
      const result = await invoke<SourceAudit>("preview_remote_source_diff_desktop", {
        localWorkspaceId:project,nodeId:node.id,
        expectedSourceSha256:prior.sourceFingerprint,
        cursorIndex:prior.totalSourceFiles,
        expectedMachineIdentity:node.identityFingerprint,
        expectedConnectionTime:node.connectedAt,
        expectedConnectionNonce:node.connectionNonce,
      });
      if (!sameSourcePairing(node) || latestDiffProjects.current[node.id] !== project) {
        throw new Error("Paired machine connection or approved project changed during aggregate source recheck.");
      }
      if (result.nodeId !== node.id || result.localWorkspaceId !== project
          || result.remoteWorkspaceId !== node.remoteWorkspaceId
          || result.sourceFingerprint !== prior.sourceFingerprint
          || result.cursorIndex !== prior.totalSourceFiles
          || result.nextCursorIndex !== prior.totalSourceFiles
          || result.totalSourceFiles !== prior.totalSourceFiles
          || !result.complete || result.rows.length !== 0) {
        throw new Error("Aggregate source parity response changed the approved project or source cursor.");
      }
      // The old per-file breakdown was a previous snapshot, not a new scan.
      setSourceAuditProgress((current) => { const next = {...current}; delete next[node.id]; return next; });
      setSourceReviewChoices((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceAudits((current) => ({...current,[node.id]:result}));
      if (!result.sourceVerified) {
        setSourceWatches((current) => { const next={...current}; delete next[node.id]; return next; });
      }
      setNotice(result.sourceVerified
        ? "Fresh source hash, file count and byte count all match. This is not native GUI acceptance."
        : "Remote aggregate source parity is NOT verified. Restart the full read-only difference preview to see current conflicts.");
    } catch (error) {
      setSourceAudits((current) => { const next = {...current}; delete next[node.id]; return next; });
      setSourceAuditProgress((current) => { const next = {...current}; delete next[node.id]; return next; });
      setSourceReviewChoices((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceWatches((current) => { const next={...current}; delete next[node.id]; return next; });
      setNotice("Aggregate recheck could not verify current source: " + errorText(error) + ". Restart the read-only comparison.");
    } finally {setBusy(false);}
  }
  async function disconnect(node: RemoteNode) {
    setBusy(true);
    try {
      await invoke("disconnect_remote_machine", { nodeId: node.id });
      setConnectionChecks((current) => { const next = { ...current }; delete next[node.id]; return next; });
      setCapacityChecks((current) => { const next = { ...current }; delete next[node.id]; return next; });
      setBrowserChecks((current) => { const next = { ...current }; delete next[node.id]; return next; });
      setRemoteJobs((current) => { const next = {...current}; delete next[node.id]; return next; });
      setSourceAudits((current) => { const next = {...current}; delete next[node.id]; return next; });
      setSourceAuditProgress((current) => { const next = {...current}; delete next[node.id]; return next; });
      setSourceIssuePages((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceReviewChoices((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceWatches((current) => { const next={...current}; delete next[node.id]; return next; });
      setSourceWatchReports((current) => { const next={...current}; delete next[node.id]; return next; });
      setDiffLocalProjects((current) => { const next = {...current}; delete next[node.id]; return next; });
      setReconnectSecrets((current) => { const next = { ...current }; delete next[node.id]; return next; });
      await refresh(); setNotice(node.label + " disconnected.");
    } catch (error) { setNotice(errorText(error)); }
    finally { setBusy(false); }
  }
  return <div className="remote-fleet-page" data-view={view}>
    <header className="remote-fleet-intro">
      <div className="remote-fleet-title-icon" aria-hidden="true"><NavIcon name="remote" size={28}/></div>
      <div><h2>Remote Computers</h2>
        <p>Connect other RepoTunnel computers and manage secure remote access.</p></div>
    </header>
    <nav className="remote-fleet-tabs" aria-label="Remote Computers pages">
      {([
        ["overview", "Overview"], ["connect", "Connect"], ["credentials", "Credentials"],
        ["machines", "Connected machines"], ["activity", "Activity & artifacts"],
        ["source", "Source approvals"],
      ] as const).map(([id, label]) =>
        <button key={id} type="button" aria-current={view === id ? "page" : undefined}
          className={view === id ? "remote-fleet-tab active" : "remote-fleet-tab"}
          onClick={() => setView(id)}>{label}</button>)}
    </nav>
    {view === "overview" && <div className="remote-fleet-stats" aria-label="Remote computers summary">
      <button type="button" className="remote-fleet-stat" onClick={() => setView("machines")}>
        <span className="remote-fleet-stat-icon green" aria-hidden="true"><NavIcon name="remote" size={24}/></span>
        <span><small>Connected computers</small><strong>{nodes.length}</strong><small>Active connections</small></span>
      </button>
      <button type="button" className="remote-fleet-stat" onClick={() => setView("credentials")}>
        <span className="remote-fleet-stat-icon blue" aria-hidden="true"><NavIcon name="shield" size={24}/></span>
        <span><small>Issued credentials</small><strong>{activeGrants.length}</strong><small>Active grants</small></span>
      </button>
      <button type="button" className="remote-fleet-stat" onClick={() => setView("activity")}>
        <span className="remote-fleet-stat-icon purple" aria-hidden="true"><NavIcon name="checks" size={24}/></span>
        <span><small>Native test runs</small><strong>{nativeRuns.length}</strong><small>Saved results</small></span>
      </button>
      <button type="button" className="remote-fleet-stat" onClick={() => setView("activity")}>
        <span className="remote-fleet-stat-icon amber" aria-hidden="true"><NavIcon name="folder" size={24}/></span>
        <span><small>Remote artifacts</small><strong>{artifacts.length}</strong><small>Downloaded files</small></span>
      </button>
    </div>}
    {view === "overview" && humanSourceIntents.some((intent) =>
      intent.importAttempt?.state === "dispatchUnknown") && <div className="remote-fleet-caution" role="alert">
        <strong>Unresolved source import</strong>
        <span>A previous import has an unknown outcome. Do not repeat it until verified.</span>
        <button type="button" onClick={() => setView("machines")}>Review receipt</button>
      </div>}
    {view === "overview" && incomingSourceApprovals.some((approval) =>
      approval.state === "dispatchUnknown") && <div className="remote-fleet-caution" role="alert">
        <strong>Receiver approval needs review</strong>
        <span>An incoming source approval has an uncertain outcome.</span>
        <button type="button" onClick={() => setView("source")}>View approvals</button>
      </div>}
    {notice && <p role="status" className="remote-fleet-notice">{notice}</p>}
    <div className="remote-fleet-layout">
    <section className="remote-fleet-card remote-fleet-grants">
      <h3>Issue credential on this computer</h3>
      <p className="remote-fleet-summary">Allow another RepoTunnel computer to connect securely.</p>
      <div className="remote-fleet-identity">
        <span>This computer's public identity</span>
        <code>{localIdentity || "Unavailable — check local identity key"}</code>
        {localIdentity && <button type="button" className="remote-fleet-copy"
          aria-label="Copy public computer identity" onClick={() => {
            if (!navigator.clipboard?.writeText) {
              setNotice("Clipboard access is unavailable. Select and copy the identity manually.");
              return;
            }
            void navigator.clipboard.writeText(localIdentity)
              .then(() => setNotice("Computer identity copied."))
              .catch(() => setNotice("Clipboard access failed. Copy the public identity manually."));
          }}>Copy</button>}
      </div>
      <details className="remote-fleet-details"><summary>How credential sharing works</summary>
        <p>Start HTTPS MCP on this computer, issue a time-limited credential, then transfer it directly to your other computer. Do not paste credentials into AI chats.</p>
      </details>
      <label>Approved remote project
        <select value={selectedWorkspace} onChange={(event) => { setSelectedWorkspace(event.target.value); setIssued(null); }}>
          <option value="">Choose a project (required)</option>
          {projects.map((project) => <option key={project.id} value={project.id}>{project.name}</option>)}
        </select>
      </label>
      <fieldset>
        <legend>Allow only these remote tool categories</legend>
        {scopeOptions.map((option) => <label key={option.id}>
          <input type="checkbox" checked={scopes.includes(option.id)} onChange={(event) => {
            setIssued(null);
            setScopes((current) => event.target.checked
              ? [...current, option.id]
              : current.filter((scope) => scope !== option.id));
          }} /> {option.label}
        </label>)}
      </fieldset>
      <div className="remote-fleet-actions">
        <button type="button" disabled={busy || !selectedWorkspace || scopes.length === 0} onClick={() => void issue()}>Issue 24-hour credential</button>
        <button type="button" className="remote-fleet-danger" disabled={busy} onClick={() => void revoke()}>Revoke all issued credentials</button>
      </div>
      <div className="remote-fleet-records">
      <div className="remote-fleet-list-heading">
        <h4>Active issued credentials ({activeGrants.length})</h4>
        <button type="button" disabled={busy} onClick={() => void refresh().catch((error) => setNotice(errorText(error)))}>Refresh</button>
      </div>
      <details className="remote-fleet-details"><summary>Credential rotation and revocation</summary>
        <p>Rotate or revoke one credential without affecting other grants. Rotation invalidates the previous secret, and the remote computer must reconnect manually. Running processes may continue. Old secrets cannot be recovered.</p>
      </details>
      {activeGrants.length === 0 ? <p>No active credentials issued by this computer.</p> : activeGrants.map((grant) =>
        <article key={grant.fingerprint} className="remote-fleet-node">
          <div className="remote-fleet-node-header">
            <div>
              <strong>Project: {grant.workspaceId}</strong>
              <small>Credential fingerprint: {grant.fingerprint.slice(0, 12)}… · Expires {new Date(grant.expiresAt * 1000).toLocaleString()}</small>
              <small>Permissions: {grant.scopes.join(", ")}</small>
            </div>
            <div className="remote-fleet-actions">
              <button type="button" disabled={busy}
                onClick={() => void rotateOne(grant)}>Rotate credential</button>
              <button type="button" disabled={busy} className="remote-fleet-danger"
                onClick={() => void revokeOne(grant)}>Revoke this credential</button>
            </div>
          </div>
        </article>)}
      </div>
      {issued && <div className="remote-fleet-credential">
        <label htmlFor="remote-issued-key">Credential (shown only here)</label>
        <textarea id="remote-issued-key" rows={2} readOnly value={issued.secret} onFocus={(event) => event.currentTarget.select()} />
        <p><strong>Approved remote project ID:</strong> <code>{issued.workspaceId}</code> (copy this ID to the coordinator together with the credential)</p>
        <p>Expires: {new Date(issued.expiresAt * 1000).toLocaleString()}</p>
        <p>{issued.warning}</p>
        <button type="button" onClick={() => setIssued(null)}>Hide credential</button>
      </div>}
    </section>
    <section className="remote-fleet-card remote-fleet-source">
        <h3>Incoming source approvals</h3>
        <p className="remote-fleet-summary">Review incoming source changes before independently approving them. Nothing is imported automatically.</p>
      <details className="remote-fleet-details"><summary>Security and verification requirements</summary>
        <p>Use exact review JSON shared out-of-band, independently compare the sender signing fingerprint and this receiver's installation identity, select a current source-scope credential, and confirm each missing path and SHA-256. Approval records evidence only; it cannot transfer or install files.</p>
      </details>
        <label htmlFor="receiver-review-draft">Incoming human-reviewed source JSON</label>
        <textarea id="receiver-review-draft" rows={7} maxLength={32768} value={incomingReviewDraft}
          onChange={(e)=>setIncomingReviewDraft(e.target.value)} placeholder="Paste RepoTunnelIncomingSourceReviewV1 JSON" />
        <label htmlFor="receiver-grant-choice">Receiver-issued source credential</label>
        <select id="receiver-grant-choice" value={incomingCredentialFingerprint}
          onChange={(e)=>setIncomingCredentialFingerprint(e.target.value)}>
          <option value="">Select exact active credential and approved project</option>
          {activeGrants.filter(grant=>grant.expiresAt>Date.now()/1000 && grant.scopes.includes("source")).map(grant=>
            <option key={grant.fingerprint} value={grant.fingerprint}>
              {grant.workspaceId} · credential SHA-256 {grant.fingerprint.slice(0,16)}…
            </option>)}
        </select>
        <button type="button" disabled={busy || !incomingReviewDraft.trim() || !incomingCredentialFingerprint}
          onClick={()=>void approveIncomingSourceReview()}>
          Confirm independent receiver create-only approval (no import)
        </button>
        <strong>Saved receiver approvals ({incomingSourceApprovals.length})</strong>
        {incomingSourceApprovals.slice(-8).reverse().map(approval=><div key={approval.id}>
          <small><code>{approval.id}</code> · {approval.state} · Destination: {approval.workspaceId}</small>
          <small>Credential SHA-256: {approval.credentialFingerprint} · Source machine SHA-256: {approval.coordinatorMachineFingerprint}</small>
          <small>Original coordinator project: {approval.coordinatorWorkspaceId} · Review: {approval.sourceReviewId}</small>
          {approval.files.map(file=><small key={file.path}><code>{file.path}</code> · {file.bytes} bytes · SHA-256 {file.sha256}</small>)}
          <button type="button" disabled={busy}
            onClick={()=>void inspectIncomingApproval(approval)}>Recheck receiver source approval (read-only)</button>
          {incomingApprovalChecks[approval.id] && <div role="status">
            <small>Credential active: {String(incomingApprovalChecks[approval.id].credentialActive)}
              · Receiver project writable: {String(incomingApprovalChecks[approval.id].destinationProjectApprovedWritable)}
              · Human import supported: {String(incomingApprovalChecks[approval.id].humanImportSupported)}
              · Proposed destinations still missing: {String(incomingApprovalChecks[approval.id].canStillProposeCreateOnly)}</small>
            {!incomingApprovalChecks[approval.id].humanImportSupported && <small role="alert">
              This operating system cannot yet safely install approved human source files. The approval is for review only.
            </small>}
            {incomingApprovalChecks[approval.id].files.map(file=><small key={file.path}>
              <code>{file.path}</code> — {file.status}
            </small>)}
            <small>{incomingApprovalChecks[approval.id].note}</small>
          </div>}
          {approval.state==="dispatchUnknown" && <small role="alert">
            Import outcome uncertain: the one-shot receiver intent is already reserved. Do not retry, withdraw, or reauthorize it. Recheck exact destination hashes read-only; matching bytes do not establish import provenance.
          </small>}
          {approval.state==="legacyUnsigned" && <small>
            Historical unsigned approval only — cannot authorize any import. New signed review and independent receiver approval are required.
          </small>}
          {approval.state==="approved" && <button type="button" disabled={busy}
            onClick={()=>void withdrawIncomingApproval(approval)}>Withdraw receiver source approval</button>}
        </div>)}
        {incomingSourceApprovals.length>8 && <small>Showing eight latest receipts; older records remain durably stored.</small>}
    </section>
    <section className="remote-fleet-card remote-fleet-connect">
      <h3>Connect a remote computer</h3>
      <p className="remote-fleet-summary">Pair another RepoTunnel installation through its HTTPS MCP endpoint.</p>
      <details className="remote-fleet-details"><summary>How secure pairing works</summary>
        <p>Enter the other computer's trusted endpoint, one-time credential and approved project ID. Compare its installation fingerprint during first pairing. Credentials stay in app memory; remembered profiles never save secrets.</p>
      </details>
      <form className="remote-fleet-form" autoComplete="off" onSubmit={(event) => void connect(event)}>
        <label>Machine name<input value={name} onChange={(event) => setName(event.target.value)} maxLength={64} placeholder="Windows test machine" required /></label>
        <label>HTTPS MCP endpoint<input type="url" value={url} onChange={(event) => setUrl(event.target.value)} placeholder="https://remote.example.com/mcp" required /></label>
        <label>Remote credential<input type="password" value={credential} onChange={(event) => setCredential(event.target.value)} autoComplete="new-password" minLength={64} maxLength={64} required /></label>
        <label>Approved remote project ID<input value={remoteWorkspaceId} onChange={(event) => setRemoteWorkspaceId(event.target.value)} maxLength={256} placeholder="workspace-..." required /></label>
        <button type="submit" disabled={busy}>Connect remote machine</button>
      </form>
      <div className="remote-fleet-remembered">
      <div className="remote-fleet-list-heading">
        <h4>Remembered remote computers ({rememberedProfiles.length})</h4>
        <small>Optional connection details only. Never saved: bearer, sessions, browser tabs or tool permissions.</small>
      </div>
      {rememberedProfiles.length === 0 ? <p>No saved connection details. Connected computers can be remembered explicitly below.</p> :
        rememberedProfiles.map((profile) => <article className="remote-fleet-node" key={profile.id}>
          <strong>{profile.label}</strong>
          <small>{profile.endpoint} · Approved project: {profile.remoteWorkspaceId}</small>
          <small>{profile.identityFingerprint
            ? <>Pinned installation key SHA-256: <code>{profile.identityFingerprint}</code></>
            : "Legacy saved profile has no cryptographic identity pin. Forget it, compare the remote fingerprint, then enroll again."}</small>
          <div className="remote-fleet-actions">
            <button type="button" disabled={busy || !profile.identityFingerprint || nodes.some((node) => node.id === profile.id)}
              onClick={() => useRememberedMachine(profile)}>Fill connection form</button>
            <button type="button" disabled={busy} onClick={() => void forgetMachine(profile)}>Forget saved details</button>
          </div>
        </article>)}
      </div>
    </section>
    <section className="remote-fleet-card remote-fleet-machines">
      <div className="remote-fleet-list-heading">
        <h3>Connected computers ({nodes.length})</h3>
        <button type="button" disabled={busy} onClick={() => void refresh().catch((e) => setNotice(errorText(e)))}>Refresh</button>
      </div>
      <label className="remote-fleet-footnote">
        <input type="checkbox" checked={healthMonitorEnabled} onChange={(event) => setHealthMonitorEnabled(event.target.checked)} />
        Monitor MCP connection health every minute while Computers is open (read-only; no automatic reconnection)
      </label>
      {nodes.length > 0 && <div className="remote-fleet-machine-preview" aria-label="Connected computer shortcuts">
        {nodes.map((node) => <div key={node.id} className="remote-fleet-machine-preview-row">
          <span aria-hidden="true" className="remote-fleet-machine-online">●</span>
          <div><strong>{node.label}</strong><small>{node.remoteWorkspaceId}</small></div>
          <button type="button" onClick={() => setView("machines")}>Manage</button>
        </div>)}
      </div>}
      {humanSourceIntents.length > 0 && <div className="remote-fleet-screen" role="status">
        <strong>Saved human source-review intents ({humanSourceIntents.length}) — local receipts only</strong>
        <p>These records survive restart but have not been approved by the receiver. They cannot initiate staging or import. Old connection generations are historical evidence, not reusable permissions.</p>
        {humanSourceIntents.slice(-4).reverse().map((intent) => <div key={intent.id}>
          <p><code>{intent.id}</code> · {intent.state} · {new Date(intent.createdAt*1000).toLocaleString()}
            {nodes.some((node) => node.id === intent.nodeId
              && node.identityFingerprint === intent.machineIdentityFingerprint
              && node.connectionNonce === intent.connectionNonce
              && node.connectedAt === intent.connectedAt
              && node.remoteWorkspaceId === intent.remoteWorkspaceId)
              ? " · original pairing still connected" : " · historical/disconnected pairing"}</p>
          <small>Local: {intent.localWorkspaceId} · Destination: {intent.remoteWorkspaceId} · Machine SHA-256: {intent.machineIdentityFingerprint} · Source SHA-256: {intent.sourceFingerprint}</small>
          {intent.proposed.map((file) => <small key={file.path}><code>{file.path}</code> · {file.bytes} bytes · SHA-256: {file.sha256}</small>)}
          {intent.importAttempt && <div role="status">
            <strong>Saved human import: {intent.importAttempt.state==="imported"
              ? "Installed — verify receiver hashes" : intent.importAttempt.state==="dispatchUnknown"
                ? "UNKNOWN — never replay" : intent.importAttempt.state==="staging"
                  ? "Staging interrupted — no automatic continuation"
                  : "Stopped before installation"}</strong>
            <small>Receiver approval: <code>{intent.importAttempt.approvalId}</code></small>
            <small>Transfer task: <code>{intent.importAttempt.taskId}</code>
              {" · "+intent.importAttempt.importedFiles+" recorded imported files"}</small>
            <small>This durable sender receipt survives restart. Inspect the original receiver approval and actual source hashes read-only before further action.</small>
            <button type="button" disabled={busy || !nodes.some((node)=>
              node.id===intent.nodeId && node.remoteWorkspaceId===intent.remoteWorkspaceId
              && node.identityFingerprint===intent.machineIdentityFingerprint
              && node.connectionNonce===intent.connectionNonce && node.connectedAt===intent.connectedAt)}
              onClick={()=>void verifyHumanImportOnReceiver(intent)}>
              Reconcile receiver import receipt and hashes (read-only)
            </button>
          </div>}
          {intent.state==="awaitingReceiverApproval" && !intent.importAttempt && <button type="button" disabled={busy || !sourceShaPattern.test(localIdentity)
            || !nodes.some((node)=>node.id===intent.nodeId
              && node.remoteWorkspaceId===intent.remoteWorkspaceId
              && node.identityFingerprint===intent.machineIdentityFingerprint
              && node.connectionNonce===intent.connectionNonce
              && node.connectedAt===intent.connectedAt)}
            onClick={() => void copyIncomingReviewProposal(intent)}>Copy receiver approval review JSON</button>}
          {intent.state === "awaitingReceiverApproval" && <button type="button" disabled={busy || !nodes.some((node)=>
            node.id===intent.nodeId && node.remoteWorkspaceId===intent.remoteWorkspaceId
              && node.identityFingerprint===intent.machineIdentityFingerprint
              && node.connectionNonce===intent.connectionNonce && node.connectedAt===intent.connectedAt)}
            onClick={() => void inspectSavedHumanSourceReview(intent)}>Recheck saved proposal status (read-only)</button>}
          {sourceIntentObservations[intent.id] && <div role="status">
            <small>Read-only receiver status, not import provenance:</small>
            {sourceIntentObservations[intent.id].rows.map((row)=> <small key={row.path}>
              <code>{row.path}</code> — {row.status}
              {row.remoteSha256 ? " · receiver SHA-256 " + row.remoteSha256 : ""}
            </small>)}
            <small>{sourceIntentObservations[intent.id].note}</small>
          </div>}
          {intent.state === "awaitingReceiverApproval" && <div>
            <label htmlFor={`receiver-approval-${intent.id}`}>Independent receiver approval ID</label>
            <input id={`receiver-approval-${intent.id}`} type="text"
              aria-label={`Receiver approval ID for ${intent.id}`}
              maxLength={48} placeholder="receiver-source-…"
              value={receiverApprovalIds[intent.id]??""}
              onChange={(event)=>{
                const value=event.target.value;
                receiverApprovalEpoch.current[intent.id]=(receiverApprovalEpoch.current[intent.id]??0)+1;
                setReceiverApprovalIds((current)=>({...current,[intent.id]:value}));
                setRemoteReceiverApprovals((current)=>{
                  const next={...current};delete next[intent.id];return next;
                });
              }}/>
            <button type="button" disabled={busy
              || !/^receiver-source-[a-f0-9]{32}$/.test(receiverApprovalIds[intent.id]??"")
              || !nodes.some((node)=>node.id===intent.nodeId
                && node.remoteWorkspaceId===intent.remoteWorkspaceId
                && node.identityFingerprint===intent.machineIdentityFingerprint
                && node.connectionNonce===intent.connectionNonce
                && node.connectedAt===intent.connectedAt)}
              onClick={()=>void inspectReceiverApprovalFromOriginalReview(intent)}>
              Inspect receiver human approval on paired machine (read-only)
            </button>
            {remoteReceiverApprovals[intent.id] && nodes.some((node)=>
              node.id===intent.nodeId
              && node.identityFingerprint===intent.machineIdentityFingerprint
              && node.connectionNonce===intent.connectionNonce
              && node.connectedAt===intent.connectedAt) && <div role="status">
              <small>Receiver approval: {remoteReceiverApprovals[intent.id].state}
                · Credential active: {String(remoteReceiverApprovals[intent.id].credentialActive)}
                · Destination writable: {String(remoteReceiverApprovals[intent.id].destinationProjectApprovedWritable)}
                · Human import supported: {String(remoteReceiverApprovals[intent.id].humanImportSupported)}
                · Still missing: {String(remoteReceiverApprovals[intent.id].canStillProposeCreateOnly)}
              </small>
              {remoteReceiverApprovals[intent.id].files.map((file)=><small key={file.path}>
                <code>{file.path}</code> — {file.status} · {file.expectedBytes} bytes
              </small>)}
              {!remoteReceiverApprovals[intent.id].humanImportSupported && <small role="alert">
                This receiver does not currently support safe human source installation on its platform. No import can be dispatched or staging approval consumed.
              </small>}
              <small>Read-only approval evidence only until you separately confirm Import. Matching bytes do not prove import provenance.</small>
              {remoteReceiverApprovals[intent.id].state==="approved"
                && remoteReceiverApprovals[intent.id].credentialActive
                && remoteReceiverApprovals[intent.id].humanImportSupported
                && remoteReceiverApprovals[intent.id].canStillProposeCreateOnly
                && !intent.importAttempt
                && !humanImportOutcomes[intent.id] && <button type="button" disabled={busy}
                onClick={()=>void importHumanApprovedSource(intent)}>
                Import the approved source files (create only)
              </button>}
            </div>}
            {humanImportProgress[intent.id] && <div role="status">
              <small>Human source import: {humanImportProgress[intent.id].phase}
                {humanImportProgress[intent.id].path ? " · "+humanImportProgress[intent.id].path : ""}
                {" · "+humanImportProgress[intent.id].sentBytes.toLocaleString()
                  +" / "+humanImportProgress[intent.id].totalBytes.toLocaleString()+" bytes"}
              </small>
              <progress max={Math.max(1,humanImportProgress[intent.id].totalBytes)}
                value={humanImportProgress[intent.id].sentBytes}/>
            </div>}
            {humanImportOutcomes[intent.id] && <div role="alert">
              <strong>Import outcome: {humanImportOutcomes[intent.id].state==="imported"
                ? "Installed and verified"
                : humanImportOutcomes[intent.id].state==="failedBeforeInstall"
                  ? "Stopped before installation" : "UNKNOWN — do not retry"}</strong>
              <small>{humanImportOutcomes[intent.id].note}</small>
              <button type="button" disabled={busy}
                onClick={()=>void verifyHumanImportOnReceiver(intent)}>Verify receiver import receipt and files (read-only)</button>
            </div>}
            {humanImportVerifications[intent.id] && <div role="status">
              <strong>{humanImportVerifications[intent.id].state==="verifiedImport"
                ? "Verified: imported receipt and all current file hashes"
                : humanImportVerifications[intent.id].state==="changedAfterImport"
                  ? "WARNING: receiver import completed, but files changed"
                  : humanImportVerifications[intent.id].state==="matchingWithoutReceipt"
                    ? "Matching content, but import outcome is NOT proven"
                    : "Import NOT verified — do not replay"}</strong>
              <small>Receiver journal state: {humanImportVerifications[intent.id].receiverState}</small>
              {humanImportVerifications[intent.id].files.map((file)=><small key={file.path}>
                <code>{file.path}</code> — {file.status} · {file.expectedBytes} bytes
                · expected SHA-256: {file.expectedSha256}
              </small>)}
              <small>{humanImportVerifications[intent.id].note}</small>
              {humanImportVerifications[intent.id].state==="verifiedImport"
                && intent.importAttempt?.state==="dispatchUnknown"
                && <button type="button" disabled={busy}
                  onClick={()=>void reconcileProvenHumanImportLocally(intent)}>
                  Record verified import in local journal (receiver read-only; no replay)
                </button>}
            </div>}
          </div>}
          {intent.state === "awaitingReceiverApproval" && !intent.importAttempt && <button type="button" disabled={busy}
            onClick={() => void withdrawHumanSourceReview(intent)}>Withdraw saved source proposal</button>}
          {intent.state === "withdrawn" && <small>This proposal was withdrawn permanently and cannot be reused as approval evidence.</small>}
        </div>)}
        {humanSourceIntents.length > 4 && <small>Showing the four latest receipts. All older receipts remain in the bounded local journal.</small>}
      </div>}
      {nodes.length === 0 ? <div className="remote-fleet-empty">
        <span aria-hidden="true" className="remote-fleet-empty-icon"><NavIcon name="remote" size={34}/></span>
        <strong>No computers connected yet</strong>
        <small>Connect another RepoTunnel installation to get started.</small>
        <button type="button" onClick={() => setView("connect")}>Connect a computer</button>
      </div> : nodes.map((node) => <article key={node.id} className="remote-fleet-node">
        <div className="remote-fleet-node-header">
          <div><strong>{node.label}</strong><small>{node.endpoint}</small><small>Approved remote project: {node.remoteWorkspaceId}</small><small>Verified machine key SHA-256: <code>{node.identityFingerprint}</code></small></div>
          <div className="remote-fleet-actions">
            <button type="button" disabled={busy} onClick={() => void checkConnection(node)}>Check connection</button>
            <button type="button" disabled={busy} onClick={() => void rememberMachine(node)}>Remember pairing details</button>
            <button type="button" disabled={busy} onClick={() => void checkCapacity(node)}>Inspect native capacity</button>
            <button type="button" disabled={busy} onClick={() => void checkBrowser(node)}>Inspect browser runtime</button>
            <button type="button" disabled={busy} onClick={() => void checkRemoteJobs(node)}>Inspect remote jobs</button>
            <button type="button" disabled={busy} onClick={() => void disconnect(node)}>Disconnect</button>
          </div>
        </div>
        {connectionChecks[node.id] && <p role="status" className="remote-fleet-footnote">{connectionChecks[node.id]}</p>}
        {capacityChecks[node.id] && <p role="status" className="remote-fleet-footnote">{capacityChecks[node.id]}</p>}
        {browserChecks[node.id] && <p role="status" className="remote-fleet-footnote">{browserChecks[node.id]}</p>}
        <div className="remote-fleet-screen">
          <label htmlFor={"remote-diff-" + node.id}>Local approved project for read-only source comparison</label>
          <select id={"remote-diff-" + node.id} disabled={busy} value={diffLocalProjects[node.id] ?? ""}
            onChange={(event) => {
              invalidateSourceReview(node.id);
              latestDiffProjects.current = {...latestDiffProjects.current, [node.id]: event.target.value};
              setDiffLocalProjects((current) => ({...current, [node.id]: event.target.value}));
              setSourceAudits((current) => { const next = {...current}; delete next[node.id]; return next; });
              setSourceAuditProgress((current) => { const next = {...current}; delete next[node.id]; return next; });
              setSourceIssuePages((current) => { const next={...current}; delete next[node.id]; return next; });
              setSourceReviewChoices((current) => { const next={...current}; delete next[node.id]; return next; });
              setSourceWatches((current) => { const next={...current}; delete next[node.id]; return next; });
            }}>
            <option value="">Choose local project</option>
            {projects.map((project) => <option key={project.id} value={project.id}>{project.name}</option>)}
          </select>
          <div className="remote-fleet-actions">
            <button type="button" disabled={busy || !diffLocalProjects[node.id]}
              onClick={() => void previewSourceDiff(node, false)}>Preview source differences</button>
            <button type="button" disabled={busy || !sourceAudits[node.id] || sourceAudits[node.id].complete}
              onClick={() => void previewSourceDiff(node, true)}>Next 8 files</button>
            <button type="button" disabled={busy || !diffLocalProjects[node.id] || Boolean(sourceAudits[node.id]?.complete)}
              onClick={() => void previewSourceDiff(node, Boolean(sourceAudits[node.id]), 8)}>Scan next 64 files (read-only)</button>
            {scanningSourceNode === node.id && <button type="button"
              onClick={() => { stopSourceScan.current = true; setNotice("Stop requested. The active read-only page may finish; no further pages will be requested."); }}>
              Stop after current page</button>}
            <button type="button" disabled={busy || !sourceAudits[node.id]?.complete}
              onClick={() => void recheckSourceParity(node)}>Recheck whole-source parity</button>
          </div>
          <label className="remote-fleet-footnote">
            <input type="checkbox"
              checked={Boolean(sourceWatches[node.id])}
              disabled={!sourceWatches[node.id]
                && (!sourceAudits[node.id]?.complete || !sourceAudits[node.id]?.sourceVerified)}
              onChange={(event) => {
                if (!event.target.checked) {
                  setSourceWatches((current) => { const next={...current}; delete next[node.id]; return next; });
                  setSourceWatchReports((current) => ({...current,[node.id]:"Source parity monitoring stopped."}));
                  return;
                }
                const audit = sourceAudits[node.id];
                if (!audit || !audit.complete || !audit.sourceVerified
                    || audit.remoteWorkspaceId !== node.remoteWorkspaceId
                    || audit.localWorkspaceId !== diffLocalProjects[node.id]) return;
                setSourceWatchReports((current) => ({...current,[node.id]:"Checking read-only aggregate source parity…"}));
                setSourceWatches((current) => ({...current,[node.id]:{
                  localWorkspaceId:audit.localWorkspaceId,
                  remoteWorkspaceId:node.remoteWorkspaceId,
                  fingerprint:audit.sourceFingerprint,totalSourceFiles:audit.totalSourceFiles,
                  machineIdentityFingerprint:node.identityFingerprint,
                  connectedAt:node.connectedAt,
                  connectionNonce:node.connectionNonce,
                }}));
              }} />
            Monitor source parity every two minutes while Computers is open (read-only)
          </label>
          {sourceWatchReports[node.id] && <p role="status" className="remote-fleet-footnote">{sourceWatchReports[node.id]}</p>}
          {sourceAudits[node.id] && <div role="status">
            <strong>{sourceAudits[node.id].totalSourceFiles === 0
              ? "Read-only source inventory: 0 files"
              : <>Read-only source page {sourceAudits[node.id].cursorIndex + 1}–{sourceAudits[node.id].nextCursorIndex} of {sourceAudits[node.id].totalSourceFiles}</>}</strong>
            <p>{sourceAudits[node.id].complete
              ? (sourceAudits[node.id].sourceVerified ? "Complete aggregate source parity verified." : "Page scan complete; full source parity NOT verified.")
              : "Partial preview — further pages remain."}</p>
            {sourceAuditProgress[node.id] && <p>
              Inspected {sourceAuditProgress[node.id].inspected} files across pages:{" "}
              {sourceAuditProgress[node.id].identical} identical,{" "}
              {sourceAuditProgress[node.id].missing} missing,{" "}
              {sourceAuditProgress[node.id].conflicts} conflicting,{" "}
              {sourceAuditProgress[node.id].unverified} unverified.
            </p>}
            {sourceAuditProgress[node.id]?.issues.slice(
              (sourceIssuePages[node.id] ?? 0) * MAX_VISIBLE_DIFF_ISSUES,
              ((sourceIssuePages[node.id] ?? 0) + 1) * MAX_VISIBLE_DIFF_ISSUES,
            ).map((row) => <p key={row.path}>
              <code>{row.path}</code> — <strong>{row.status}</strong>
              {" "}· local: {row.localBytes.toLocaleString()} bytes
              {row.localSha256 && ` · SHA ${row.localSha256.slice(0,16)}…`}
              {row.remoteSha256 && <> · remote: {row.remoteBytes == null ? "unknown size" : row.remoteBytes.toLocaleString() + " bytes"}
                {` · SHA ${row.remoteSha256.slice(0,16)}…`}</>}
              {row.status === "missing" && " · no remote source file found"}
              {(row.status === "missing" || row.status === "conflict") && <label>
                {" · Review only: "}
                <select aria-label={"Review decision for " + row.path}
                  value={sourceReviewChoices[node.id]?.[row.path] ?? defaultReviewChoice(row.status)}
                  onChange={(event) => {
                    const choice = event.target.value as SourceReviewChoice;
                    if (row.status === "missing" && !["defer", "proposeCreate"].includes(choice)) return;
                    if (row.status === "conflict" && !["manualMerge", "keepRemote"].includes(choice)) return;
                    invalidateSourceReview(node.id);
                    setSourceReviewChoices((current) => ({
                      ...current,
                      [node.id]: { ...(current[node.id] ?? {}), [row.path]: choice },
                    }));
                  }}>
                  {row.status === "missing"
                    ? <>
                      <option value="defer">Defer missing file</option>
                      <option value="proposeCreate">Propose create-only transfer (no write)</option>
                    </>
                    : <>
                      <option value="manualMerge">Requires manual conflict resolution</option>
                      <option value="keepRemote">Keep remote source unchanged</option>
                    </>}
                </select>
              </label>}
              {row.status === "unverified" && " · investigate access before any import"}
            </p>)}
            {sourceAuditProgress[node.id]?.issues.length > MAX_VISIBLE_DIFF_ISSUES && <div className="remote-fleet-actions">
              <button type="button" disabled={(sourceIssuePages[node.id] ?? 0) === 0}
                onClick={() => setSourceIssuePages((current) => ({...current,[node.id]: Math.max(0,(current[node.id] ?? 0)-1)}))}>
                Previous issues</button>
              <span>Issues {(sourceIssuePages[node.id] ?? 0)*MAX_VISIBLE_DIFF_ISSUES+1}–{Math.min(((sourceIssuePages[node.id] ?? 0)+1)*MAX_VISIBLE_DIFF_ISSUES,sourceAuditProgress[node.id].issues.length)} of {sourceAuditProgress[node.id].issues.length}</span>
              <button type="button"
                disabled={((sourceIssuePages[node.id] ?? 0)+1)*MAX_VISIBLE_DIFF_ISSUES >= sourceAuditProgress[node.id].issues.length}
                onClick={() => setSourceIssuePages((current) => ({...current,[node.id]:(current[node.id] ?? 0)+1}))}>
                Next issues</button>
            </div>}
            {sourceAuditProgress[node.id]?.issues.length === 0 &&
              <p>No differences found in the inspected files.</p>}
            {Boolean(sourceAuditProgress[node.id]?.issues.length) && <div>
              {sourceAudits[node.id]?.complete && <div className="remote-fleet-actions">
                <button type="button" disabled={busy ||
                  !sourceAuditProgress[node.id]?.issues.some((row)=>
                    row.status==="missing" && sourceReviewChoices[node.id]?.[row.path]==="proposeCreate") ||
                  (createOnlyPreflights[node.id]?.checkedPaths.length ?? 0) >=
                    sourceAuditProgress[node.id]?.issues.filter((row)=>
                      row.status==="missing" && sourceReviewChoices[node.id]?.[row.path]==="proposeCreate").length}
                  onClick={() => void preflightNextCreateOnly(node)}>
                  Preflight next 8 proposed files (read-only)
                </button>
                {createOnlyPreflights[node.id] && <span role="status">
                  Create-only preflight: {createOnlyPreflights[node.id].checkedPaths.length} proposed files confirmed missing on the pinned connection. These results are not an import authorization.
                </span>}
                <button type="button" disabled={busy || !createOnlyPreflights[node.id]
                  || sourceAuditProgress[node.id]?.issues.filter((row) =>
                    row.status === "missing" && sourceReviewChoices[node.id]?.[row.path] === "proposeCreate")
                    .slice(0,8).some((row,i) => createOnlyPreflights[node.id]?.checkedPaths[i] !== row.path)}
                  onClick={() => void recordHumanSourceReview(node)}>
                  Record reviewed create-only intent (no transfer)
                </button>
              </div>}
              <button type="button" disabled={busy}
                onClick={() => void copySourceReviewDraft(node)}>
                Copy source conflict review draft
              </button>
              <p className="remote-fleet-footnote">
                Review choices are advisory only and remain on this page. The copied draft includes
                source SHA-256 evidence, project identity, full inspected issue list, and whether
                the scan is partial. It contains no source bytes or credentials. No remote mutation
                is authorized or executed; fresh preflight and separate remote approval remain required.
              </p>
            </div>}
            <small>{sourceAudits[node.id].note}</small>
          </div>}
        </div>
        {remoteJobs[node.id] && <div className="remote-fleet-screen" role="status">
          <strong>Remote job snapshot — {remoteJobs[node.id].runningProcesses} running</strong>
          <small>Observed {new Date(remoteJobs[node.id].checkedAt * 1000).toLocaleString()} · Project {remoteJobs[node.id].remoteWorkspaceId}</small>
          {remoteJobs[node.id].jobs.length === 0 ? <p>No managed process records returned.</p> :
            remoteJobs[node.id].jobs.map((job) => <p key={job.processId}>
              <strong>{job.label || "Managed process"}</strong> ({job.processId}): {job.status}
              {job.exitCode != null ? ` · exit ${job.exitCode}` : ""}
            </p>)}
          <small>{remoteJobs[node.id].note}</small>
        </div>}
        <div className="remote-fleet-screen">
          <label htmlFor={"remote-repair-" + node.id}>Replacement credential (for this machine's same approved project)</label>
          <input id={"remote-repair-" + node.id} type="password" autoComplete="new-password"
            value={reconnectSecrets[node.id] ?? ""}
            onChange={(event) => setReconnectSecrets((current) => ({ ...current, [node.id]: event.target.value }))}
            minLength={64} maxLength={64} placeholder="Paste rotated 64-character secret"
          />
          <button type="button" disabled={busy || (reconnectSecrets[node.id] ?? "").length !== 64 || node.ownerClaimed}
            onClick={() => void rePair(node)}>Re-pair with new credential</button>
          {node.ownerClaimed && <small>Disconnect explicitly to release the AI session before re-pairing. Existing job outcomes must be inspected, not replayed.</small>}
        </div>
        <div className="remote-fleet-screen">
          <label htmlFor={"remote-rustdesk-" + node.id}>Optional RustDesk peer ID</label>
          <input id={"remote-rustdesk-" + node.id}
            value={peerIds[node.id] ?? ""}
            onChange={(event) => setPeerIds((current) => ({ ...current, [node.id]: event.target.value }))}
            placeholder="RustDesk ID on the remote computer"
            maxLength={80}
          />
          <button type="button" disabled={busy || !(peerIds[node.id] ?? "").trim()}
            onClick={() => void invoke("view_remote_machine_screen", { peerId: peerIds[node.id] })
              .then(() => setNotice("Opening the installed RustDesk client. The other computer's screen-sharing permissions still apply."))
              .catch((error) => setNotice("Could not open RustDesk: " + errorText(error)))}>
            View screen in RustDesk
          </button>
        </div>
        <div className="remote-fleet-chips">
          <span>{node.availableTools.includes("browser_action") ? "Browser tool advertised" : "Browser tool not advertised"}</span>
          <span>{node.availableTools.includes("ai_workspace_session") ? "AI Workspace tool advertised" : "AI Workspace tool not advertised"}</span>
          <span>{node.availableTools.includes("start_process") ? "Build tool advertised" : "Build tool not advertised"}</span>
          <span>{node.ownerClaimed ? "AI session bound" : "Available for binding"}</span>
        </div>
      </article>)}
      <details className="remote-fleet-details remote-fleet-footnote"><summary>What connected status confirms</summary>
        <p>Advertised tools do not prove that the remote browser, native desktop or build runtime is ready. A timed-out action has an unknown outcome until independently verified on that machine.</p>
      </details>
    </section>
    <section className="remote-fleet-card remote-fleet-activity">
      <div className="remote-fleet-list-heading"><h3>Native test history</h3>
        <button type="button" disabled={busy} onClick={() => void refresh().catch((error) => setNotice(errorText(error)))}>Refresh</button>
      </div>
      <p className="remote-fleet-summary">Previously dispatched tests. Reviewing these records does not restart anything.</p>
      <details className="remote-fleet-details"><summary>About test history</summary>
        <p>These are local receipts only. Unknown outcomes need read-only inspection on the original remote machine.</p>
      </details>
      {nativeRuns.length === 0 ? <p>No recorded native test runs yet.</p> : nativeRuns.map((run) => <article className="remote-fleet-node" key={run.id}>
        <div className="remote-fleet-node-header"><div>
          <strong>{run.id}</strong>
          <small>Source project: {run.localWorkspaceId} · {new Date(run.createdAt * 1000).toLocaleString()} · {run.dispatchComplete ? "Dispatch finished" : "Dispatch incomplete"}</small>
        </div></div>
        {run.jobs.map((job, index) => <div key={`${run.id}-${index}`} className="remote-fleet-screen">
          <strong>{job.platform} {job.architecture ? `(${job.architecture})` : ""} — {job.state}</strong>
          <small>Remote project: {job.remoteWorkspaceId} · Node: {job.nodeId} · Process: {job.processId ?? "not confirmed"}</small>
          <p>{job.evidence}</p>
          <button type="button" disabled={busy || !run.dispatchComplete || !job.processId}
            onClick={() => void inspectSavedNativeJob(run, job, index)}>Inspect live status</button>
          <button type="button"
            disabled={busy || !run.dispatchComplete || !job.processId
              || !["submitted","running","unverified"].includes(job.state)}
            onClick={() => void cancelSavedNativeJob(run, job, index)}>Stop native job…</button>
          {nativeJobObservations[`${run.id}:${index}`] &&
            <p role="status">{nativeJobObservations[`${run.id}:${index}`]}</p>}
          {job.cancellationObservation && <p>Independent cancellation inspection{job.cancellationObservedAt ? ` (${new Date(job.cancellationObservedAt * 1000).toLocaleString()})` : ""}: {job.cancellationObservation}</p>}
        </div>)}
        <p className="remote-fleet-footnote">{run.limitations}</p>
      </article>)}
    </section>
    <section className="remote-fleet-card remote-fleet-artifacts">
      <div className="remote-fleet-list-heading"><h3>Remote build artifacts</h3>
        <button type="button" onClick={() => void refresh().catch((e) => setNotice(errorText(e)))} disabled={busy}>Refresh</button>
      </div>
      <p className="remote-fleet-summary">Downloaded build outputs, checked by SHA-256 and never executed automatically.</p>
      <details className="remote-fleet-details"><summary>About downloaded artifacts</summary>
        <p>Artifacts remain in private local storage. Saving a completed artifact requires choosing its destination.</p>
      </details>
      {artifacts.length===0 ? <p>No transferred artifacts yet.</p> : artifacts.map((artifact) => <article className="remote-fleet-node" key={artifact.transferId}>
        <div className="remote-fleet-node-header">
          <div><strong>{artifact.relativePath}</strong>
            <small>{artifact.receivedBytes.toLocaleString()} / {artifact.totalBytes.toLocaleString()} bytes · {artifact.completed ? "Verified" : "Incomplete"}</small>
          </div>
          <button disabled={!artifact.completed || busy} type="button"
            onClick={() => void invoke<string>("export_remote_artifact", { transferId: artifact.transferId })
              .then((result) => setNotice(result)).catch((error) => setNotice(errorText(error)))}>
            Save artifact as…
          </button>
        </div>
      </article>)}
    </section>
    </div>
  </div>;
}