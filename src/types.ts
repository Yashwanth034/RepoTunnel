export type WorkspaceAccessMode = "readOnly" | "readWrite";
export type WorkspaceChangePolicy = "review" | "automatic";
export type CommandPolicy = "disabled" | "review" | "automatic";

export type Workspace = {
  id: string;
  name: string;
  path: string;
  addedAt: number;
  accessMode: WorkspaceAccessMode;
  changePolicy: WorkspaceChangePolicy;
  commandPolicy: CommandPolicy;
};

export type WorkspaceHealth = {
  workspaceId: string;
  available: boolean;
  message: string | null;
};


export type AiAccessStatus = {
  paused: boolean;
};

export type CheckpointSummary = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  name: string | null;
  pinned: boolean;
  createdAt: number;
  fileCount: number;
  totalBytes: number;
};

export type CheckpointComparison = {
  checkpoint: CheckpointSummary;
  addedCount: number;
  modifiedCount: number;
  deletedCount: number;
  added: string[];
  modified: string[];
  deleted: string[];
};

export type CheckpointRestoreResult = {
  checkpoint: CheckpointSummary;
  preRestoreCheckpoint: CheckpointSummary;
  restoredFiles: number;
  removedFiles: number;
};

export type HistorySettings = {
  versionHistoryLimit: number | null;
  checkpointLimit: number | null;
};

export type HistoryClearResult = {
  removedVersions: number;
  removedChanges: number;
  removedActivities: number;
  removedOperationalRecords: number;
};

export type CheckpointClearResult = {
  removedCheckpoints: number;
};

export type SafetyScanCheck = {
  key: string;
  title: string;
  status: "pass" | "warning" | "blocked";
  detail: string;
  items: string[];
};

export type SafetyScanResult = {
  workspaceId: string;
  workspaceName: string;
  level: "protected" | "attention" | "blocked";
  fileCount: number;
  ignoredEntryCount: number;
  pendingReviews: number;
  checks: SafetyScanCheck[];
};

export type GatewayStatus = {
  running: boolean;
  port: number | null;
  workspaceCount: number;
};

export type AccessCheck = {
  allowed: boolean;
  reason: string | null;
};

export type DirectoryEntry = {
  name: string;
  path: string;
  kind: "file" | "directory" | "symlink" | "other";
  size: number | null;
  modifiedAt: number | null;
};

export type FileContent = {
  path: string;
  content: string;
  size: number;
  modifiedAt: number | null;
};

export type ImagePreview = {
  path: string;
  mimeType: string;
  size: number;
  dataBase64: string;
};

export type FileInfo = {
  path: string;
  kind: "file" | "directory" | "symlink" | "other";
  size: number | null;
  modifiedAt: number | null;
  readonly: boolean;
};

export type SearchMatch = {
  path: string;
  line: number;
  column: number;
  preview: string;
};

export type PublicTunnelStatus = {
  configured: boolean;
  running: boolean;
  ready: boolean;
  publicUrl: string | null;
  mcpUrl: string | null;
  autoStart: boolean;
  requestCount: number;
  lastRemoteRequestAt: number | null;
  message: string | null;
};

export type ChatConnectionStatus = {
  clientAvailable: boolean;
  clientVersion: string | null;
  running: boolean;
  ready: boolean;
  tunnelId: string | null;
  adminUrl: string | null;
  message: string | null;
};

export type ChangeOperation =
  | "createFile"
  | "writeFile"
  | "patchFile"
  | "createDirectory"
  | "renameEntry"
  | "moveEntry"
  | "deleteEntry";

export type ChangeStatus = "pending" | "applied" | "rejected" | "undone" | "failed";

export type ChangeRecord = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  operation: ChangeOperation;
  primaryPath: string;
  secondaryPath: string | null;
  summary: string;
  diff: string | null;
  status: ChangeStatus;
  createdAt: number;
  updatedAt: number;
  canUndo: boolean;
  error: string | null;
};

export type ChangeOutcome = {
  applied: boolean;
  change: ChangeRecord;
  file: FileInfo | null;
};

export type VersionFileChange = {
  operation: ChangeOperation;
  primaryPath: string;
  secondaryPath: string | null;
  summary: string;
  diff: string | null;
};

export type VersionRecord = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  parentId: string | null;
  editGroupId: string | null;
  beforeSnapshotId: string;
  afterSnapshotId: string;
  summary: string;
  files: VersionFileChange[];
  createdAt: number;
  updatedAt: number;
};

export type VersionTimeline = {
  records: VersionRecord[];
  currentVersionId: string | null;
};

export type ActivityKind =
  | "files"
  | "terminal"
  | "process"
  | "launcher"
  | "browser"
  | "git"
  | "monitoring"
  | "verification"
  | "team";

export type ActivityStatus =
  | "pending"
  | "running"
  | "succeeded"
  | "failed"
  | "rejected"
  | "stopped"
  | "observed";

export type ActivityEvent = {
  id: string;
  kind: ActivityKind;
  action: string;
  summary: string;
  detail: string | null;
  status: ActivityStatus;
  sourceId: string | null;
  createdAt: number;
  updatedAt: number;
};

export type ActivityGroup = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  traceGroupId: string | null;
  summary: string;
  versionIds: string[];
  events: ActivityEvent[];
  createdAt: number;
  updatedAt: number;
};

export type ActivityTimeline = {
  groups: ActivityGroup[];
};

export type VersionRestoreResult = {
  currentVersionId: string | null;
  recoveryCheckpointId: string;
  restoredFiles: number;
  removedFiles: number;
};


export type LanguageStat = {
  name: string;
  files: number;
};

export type ProjectOverview = {
  fileCount: number;
  directoryCount: number;
  textFileCount: number;
  binaryFileCount: number;
  largeFileCount: number;
  ignoredEntryCount: number;
  ignoredEntries: string[];
  totalBytes: number;
  languages: LanguageStat[];
  manifests: string[];
  truncated: boolean;
};

export type ProjectEntry = {
  path: string;
  kind: "file" | "directory";
  size: number | null;
  binary: boolean;
  large: boolean;
  language: string | null;
};

export type ProjectSnapshot = {
  overview: ProjectOverview;
  entries: ProjectEntry[];
};


export type GitFileChange = {
  path: string;
  indexStatus: string;
  worktreeStatus: string;
  staged: boolean;
  unstaged: boolean;
  untracked: boolean;
  conflicted: boolean;
};

export type GitRepositoryStatus = {
  available: boolean;
  message: string | null;
  branch: string | null;
  head: string | null;
  detached: boolean;
  ahead: number;
  behind: number;
  stagedCount: number;
  unstagedCount: number;
  untrackedCount: number;
  conflictedCount: number;
  changes: GitFileChange[];
};

export type GitDiff = {
  staged: boolean;
  content: string;
  truncated: boolean;
};

export type GitCommitSummary = {
  hash: string;
  shortHash: string;
  author: string;
  timestamp: number;
  subject: string;
};

export type GitActionKind = "stage" | "commit";
export type GitActionStatus = "pending" | "applied" | "rejected" | "failed";

export type GitActionRecord = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  kind: GitActionKind;
  summary: string;
  detail: string | null;
  status: GitActionStatus;
  createdAt: number;
  updatedAt: number;
  commitHash: string | null;
  error: string | null;
};

export type CommandStatus =
  | "pending"
  | "running"
  | "completed"
  | "failed"
  | "rejected"
  | "timedOut";

export type CommandPreset = {
  id: string;
  label: string;
  command: string;
  timeoutSeconds: number;
};

export type CommandRecord = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  presetId: string;
  label: string;
  command: string;
  status: CommandStatus;
  createdAt: number;
  updatedAt: number;
  durationMs: number | null;
  exitCode: number | null;
  stdout: string;
  stderr: string;
  outputTruncated: boolean;
  error: string | null;
};

export type CommandOutcome = {
  queued: boolean;
  command: CommandRecord;
};

export type ExecutionStatus = {
  sandboxAvailable: boolean;
  sandboxVersion: string | null;
  message: string | null;
};

export type TerminalCommandStatus =
  | "pending"
  | "running"
  | "completed"
  | "failed"
  | "rejected"
  | "timedOut";

export type TerminalCommandRecord = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  command: string;
  cwd: string;
  status: TerminalCommandStatus;
  createdAt: number;
  updatedAt: number;
  durationMs: number | null;
  exitCode: number | null;
  stdout: string;
  stderr: string;
  outputTruncated: boolean;
  error: string | null;
};

export type TerminalCommandOutcome = {
  queued: boolean;
  command: TerminalCommandRecord;
};

export type ManagedProcessStatus =
  | "pending"
  | "running"
  | "exited"
  | "stopped"
  | "failed"
  | "rejected";

export type ManagedProcessRecord = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  label: string;
  command: string;
  cwd: string;
  status: ManagedProcessStatus;
  pid: number | null;
  createdAt: number;
  startedAt: number | null;
  updatedAt: number;
  exitedAt: number | null;
  exitCode: number | null;
  restartCount: number;
  error: string | null;
};

export type ManagedProcessOutcome = {
  queued: boolean;
  process: ManagedProcessRecord;
};

export type ManagedProcessOutput = {
  processId: string;
  status: ManagedProcessStatus;
  stdout: string;
  stderr: string;
  stdoutOffset: number;
  stderrOffset: number;
  nextStdoutOffset: number;
  nextStderrOffset: number;
  stdoutHasMore: boolean;
  stderrHasMore: boolean;
  outputTruncated: boolean;
};


export type LaunchActionKind = "url" | "workspacePath" | "application";
export type LaunchActionStatus = "pending" | "launched" | "failed" | "rejected";

export type LaunchApplication = {
  id: string;
  name: string;
  category: string;
  executable: string;
  supportsUrls: boolean;
  supportsPaths: boolean;
};

export type LaunchActionRecord = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  kind: LaunchActionKind;
  target: string;
  applicationId: string | null;
  applicationName: string | null;
  status: LaunchActionStatus;
  createdAt: number;
  updatedAt: number;
  pid: number | null;
  error: string | null;
};

export type LaunchActionOutcome = {
  queued: boolean;
  launch: LaunchActionRecord;
};

export type BrowserActionKind =
  | "start"
  | "stop"
  | "openTab"
  | "activateTab"
  | "closeTab"
  | "navigate"
  | "click"
  | "type"
  | "scroll"
  | "reload";

export type BrowserActionStatus = "pending" | "applied" | "failed" | "rejected";

export type BrowserApplication = {
  id: string;
  name: string;
  executable: string;
  nodeExecutable: string;
};

export type BrowserAutomationStatus = {
  available: boolean;
  running: boolean;
  workspaceId: string;
  browserId: string | null;
  browserName: string | null;
  executable: string | null;
  pid: number | null;
  debugPort: number | null;
  startedAt: number | null;
  sessionId: string | null;
  activeTabId: string | null;
  message: string | null;
};

export type BrowserTab = {
  id: string;
  title: string;
  url: string;
  active: boolean;
};

export type BrowserActionRecord = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  kind: BrowserActionKind;
  target: string;
  detail: string | null;
  status: BrowserActionStatus;
  createdAt: number;
  updatedAt: number;
  error: string | null;
};

export type BrowserActionOutcome = {
  queued: boolean;
  action: BrowserActionRecord;
};

export type BrowserPageInspection = {
  tabId: string;
  title: string;
  url: string;
  selector: string | null;
  found: boolean;
  tag: string | null;
  text: string;
  html: string;
};

export type BrowserScreenshot = {
  id: string;
  tabId: string;
  createdAt: number;
  mimeType: string;
  dataBase64: string;
  sizeBytes: number;
  fullPage: boolean;
};

export type BrowserConsoleEntry = {
  tabId: string;
  level: string;
  message: string;
  url: string | null;
  timestamp: number;
};

export type BrowserNetworkFailure = {
  tabId: string;
  url: string | null;
  method: string | null;
  status: number | null;
  errorText: string;
  resourceType: string | null;
  timestamp: number;
};

export type BrowserDiagnostics = {
  consoleEntries: BrowserConsoleEntry[];
  networkFailures: BrowserNetworkFailure[];
};

export type MonitoringFileChangeKind = "created" | "modified" | "deleted";

export type MonitoringFileEvent = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  kind: MonitoringFileChangeKind;
  path: string;
  detectedAt: number;
  size: number | null;
};

export type MonitoringStatus = {
  enabled: boolean;
  running: boolean;
  workspaceId: string;
  workspaceName: string;
  startedAt: number | null;
  lastScanAt: number | null;
  scannedFileCount: number;
  fileScanTruncated: boolean;
  message: string | null;
};

export type MonitoringPortListener = {
  protocol: string;
  address: string;
  port: number;
  pid: number | null;
  processName: string | null;
  managedProcessId: string | null;
};

export type MonitoringProcessSnapshot = {
  processId: string;
  label: string;
  command: string;
  status: ManagedProcessStatus;
  pid: number | null;
  ports: number[];
  stdoutTail: string;
  stderrTail: string;
  outputTruncated: boolean;
  updatedAt: number;
};

export type MonitoringTerminalSnapshot = {
  commandId: string;
  command: string;
  status: TerminalCommandStatus;
  exitCode: number | null;
  stdoutTail: string;
  stderrTail: string;
  updatedAt: number;
};

export type MonitoringBrowserSnapshot = {
  status: BrowserAutomationStatus;
  tabs: BrowserTab[];
  consoleEntries: BrowserConsoleEntry[];
  networkFailures: BrowserNetworkFailure[];
};

export type MonitoringSnapshot = {
  status: MonitoringStatus;
  processes: MonitoringProcessSnapshot[];
  terminal: MonitoringTerminalSnapshot[];
  ports: MonitoringPortListener[];
  browser: MonitoringBrowserSnapshot;
  fileEvents: MonitoringFileEvent[];
};

export type WorkflowCheckStatus = "pass" | "warning" | "blocked";
export type WorkflowReadinessLevel = "ready" | "limited" | "blocked";

export type WorkflowCheck = {
  key: string;
  title: string;
  status: WorkflowCheckStatus;
  detail: string;
};

export type WorkflowReadiness = {
  workspaceId: string;
  workspaceName: string;
  level: WorkflowReadinessLevel;
  inspectionReady: boolean;
  editingReady: boolean;
  testingReady: boolean;
  gitReady: boolean;
  projectFileCount: number;
  commandPresetCount: number;
  gitBranch: string | null;
  checks: WorkflowCheck[];
  nextStep: string;
};


export type RuntimeDiagnostics = {
  version: string;
  platform: string;
  architecture: string;
  dataDirectory: string;
  logFile: string;
  launchAtLogin: boolean;
  sandboxAvailable: boolean;
  tunnelClientAvailable: boolean;
  gitAvailable: boolean;
  warnings: string[];
};


export type TeamSessionStatus = "active" | "paused" | "completed" | "cancelled";
export type TeamPhase = "planning" | "executing" | "reviewing" | "verifying" | "complete";
export type TeamAgentStatus = "invited" | "active" | "idle" | "offline" | "done";
export type TeamTaskStatus = "todo" | "inProgress" | "review" | "blocked" | "done" | "cancelled";
export type TeamMessageKind = "plan" | "progress" | "question" | "review" | "decision" | "handoff" | "system";

export type TeamCriterionCheck = {
  id: string;
  text: string;
  verified: boolean;
  evidence: string | null;
  verifiedByAgentId: string | null;
  verifiedAt: number | null;
};

export type TeamAgent = {
  id: string;
  name: string;
  role: string;
  clientLabel: string | null;
  status: TeamAgentStatus;
  joinedAt: number | null;
  lastSeenAt: number | null;
  currentTaskId: string | null;
};

export type TeamCycleRecord = {
  number: number;
  request: string;
  completedAt: number;
  summary: string;
  doneTaskCount: number;
  verifiedCriterionCount: number;
};

export type TeamTask = {
  id: string;
  title: string;
  description: string;
  status: TeamTaskStatus;
  priority: number;
  ownerAgentId: string | null;
  reviewerAgentId: string | null;
  contributorAgentIds: string[];
  dependsOn: string[];
  result: string | null;
  blockedReason: string | null;
  createdByAgentId: string | null;
  cycleNumber: number;
  createdAt: number;
  updatedAt: number;
};

export type TeamMessage = {
  id: string;
  agentId: string | null;
  agentName: string | null;
  kind: TeamMessageKind;
  text: string;
  taskId: string | null;
  createdAt: number;
};

export type TeamLock = {
  id: string;
  path: string;
  agentId: string;
  taskId: string | null;
  createdAt: number;
  expiresAt: number;
};

export type TeamSession = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  goal: string;
  successCriteria: string[];
  criterionChecks: TeamCriterionCheck[];
  status: TeamSessionStatus;
  phase: TeamPhase;
  agents: TeamAgent[];
  tasks: TeamTask[];
  messages: TeamMessage[];
  locks: TeamLock[];
  revision: number;
  cycleNumber: number;
  currentRequest: string | null;
  completedCycles: TeamCycleRecord[];
  persistentTeam: boolean;
  createdAt: number;
  updatedAt: number;
  completedAt: number | null;
  completionSummary: string | null;
};

export type TeamProgress = {
  openTaskCount: number;
  doneTaskCount: number;
  blockedTaskCount: number;
  verifiedCriterionCount: number;
  totalCriterionCount: number;
  progressPercent: number;
};

export type TeamSnapshot = {
  session: TeamSession;
  progress: TeamProgress;
  recommendedAction: string | null;
};

export type TeamSessionSummary = {
  id: string;
  workspaceId: string;
  workspaceName: string;
  goal: string;
  status: TeamSessionStatus;
  phase: TeamPhase;
  agentCount: number;
  joinedAgentCount: number;
  openTaskCount: number;
  doneTaskCount: number;
  createdAt: number;
  updatedAt: number;
};
