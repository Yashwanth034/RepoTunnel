import { invoke } from "@tauri-apps/api/core";
import type {
  AccessCheck,
  ActivityTimeline,
  AiAccessStatus,
  CheckpointClearResult,
  CheckpointComparison,
  CheckpointRestoreResult,
  CheckpointSummary,
  ChangeOutcome,
  ChangeRecord,
  ChatConnectionStatus,
  CommandOutcome,
  CommandPolicy,
  CommandPreset,
  CommandRecord,
  ManagedProcessOutcome,
  ManagedProcessOutput,
  ManagedProcessRecord,
  ModelHubSnapshot,
  ModelTrialSnapshot,
  TrialMode,
  ModelProviderId,
  ModelSelection,
  ModelTestResult,
  RuntimeStatus,
  HomeChatStartResult,
  HomeConversation,
  HomeConversationSummary,
  HomeProjectContextRequest,
  ProjectEntry,
  LaunchActionOutcome,
  LaunchActionRecord,
  LaunchApplication,
  GithubConnectionStatus,
  DeepIntegration,
  DesktopControlApplication,
  AiWorkspaceFrame,
  AiWorkspaceStatus,
  BrowserActionOutcome,
  BrowserActionRecord,
  BrowserApplication,
  BrowserAutomationStatus,
  BrowserDiagnostics,
  BrowserPageInspection,
  BrowserScreenshot,
  BrowserTab,
  BrowserVisualSelection,
  MonitoringFileEvent,
  MonitoringSnapshot,
  MonitoringStatus,
  TerminalCommandOutcome,
  TerminalCommandRecord,
  ExecutionStatus,
  GatewayStatus,
  GitActionRecord,
  GitCommitSummary,
  GitDiff,
  GitRepositoryStatus,
  HistoryClearResult,
  HistorySettings,
  ProjectMemory,
  ProjectSetupOutcome,
  ProjectSetupStatus,
  ProjectSnapshot,
  PublicTunnelProvider,
  PublicTunnelStatus,
  PhoneAccessMode,
  PhoneAccessStatus,
  PhoneCapability,
  PhoneDiscoveryStatus,
  PhoneRuntimeProbe,
  PhoneRuntimeStatus,
  PhoneScreenFrame,
  HttpsSetupReadiness,
  HttpsSetupInstallResult,
  HttpsSetupHostnameVerification,
  HttpsSetupResource,
  Workspace,
  WorkspaceHealth,
  WorkspaceAccessMode,
  WorkspaceChangePolicy,
  WorkflowReadiness,
  VersionRestoreResult,
  VersionTimeline,
  RuntimeDiagnostics,
  UpdateStatus,
  UpdateInstallResult,
  ResumeSnapshot,
  SafetyScanResult,
  TeamSessionSummary,
  TeamSnapshot,
  VideoAnalysisJob,
  VideoAnalysisMode,
  VideoAnalysisResult,
  VideoCleanupReport,
  VideoCleanupRequest,
  VideoAssetLicenseInput,
  VideoAssetLicenseRecord,
  VideoAssetSource,
  VideoProductionDocument,
  VideoProductionProject,
  VideoProductionResourcePolicy,
  VideoProductionScene,
  VideoProductionSceneInput,
  StoryDirectorInput,
  StoryDirectorPlan,
  StoryAnimaticPlan,
  StoryAnimaticRender,
  StoryProductionCapabilities,
  StoryRenderQueue,
  StoryShotRenderInput,
  StoryShotRenderJob,
  NarrativeQaReport,
  VideoProjectFile,
  VideoPipelineStatus,
  VideoQaReport,
  NarrationAsset,
  NarrationProviderStatus,
  NarrationRequest,
  SubtitleAsset,
  VideoRecordingStatus,
  VideoPreviewSource,
  VideoRenderJob,
  VideoRenderRequest,
  VideoRenderResult,
  VideoDiagramSpec,
  VideoSceneLayoutReport,
  VideoSceneRender,
  VideoSceneSpec,
  VideoToolsStatus,
} from "../types";

export async function selectWorkspace(): Promise<string | null> {
  return invoke<string | null>("select_workspace");
}

export async function listWorkspaces(): Promise<Workspace[]> {
  return invoke<Workspace[]>("list_workspaces");
}

export async function getWorkspaceHealth(workspaceId: string): Promise<WorkspaceHealth> {
  return invoke<WorkspaceHealth>("get_workspace_health", { workspaceId });
}

export async function relocateWorkspace(workspaceId: string, path: string): Promise<Workspace> {
  return invoke<Workspace>("relocate_workspace", { workspaceId, path });
}

export async function addWorkspace(path: string): Promise<Workspace> {
  return invoke<Workspace>("add_workspace", { path });
}

export async function createProject(name: string): Promise<Workspace> {
  return invoke<Workspace>("create_project", { name });
}

export async function removeWorkspace(id: string): Promise<Workspace[]> {
  return invoke<Workspace[]>("remove_workspace", { id });
}

export async function updateWorkspaceAccess(
  id: string,
  accessMode: WorkspaceAccessMode,
): Promise<Workspace> {
  return invoke<Workspace>("update_workspace_access", { id, accessMode });
}

export async function updateWorkspaceChangePolicy(
  id: string,
  changePolicy: WorkspaceChangePolicy,
): Promise<Workspace> {
  return invoke<Workspace>("update_workspace_change_policy", { id, changePolicy });
}


export async function updateWorkspaceCommandPolicy(
  id: string,
  commandPolicy: CommandPolicy,
): Promise<Workspace> {
  return invoke<Workspace>("update_workspace_command_policy", { id, commandPolicy });
}

export async function checkWorkspaceAccess(
  id: string,
  relativePath: string,
  write: boolean,
  mustExist: boolean,
): Promise<AccessCheck> {
  return invoke<AccessCheck>("check_workspace_access", {
    id,
    relativePath,
    write,
    mustExist,
  });
}

export async function getProjectSetup(workspaceId: string): Promise<ProjectSetupStatus> {
  return invoke<ProjectSetupStatus>("get_project_setup", { workspaceId });
}

export async function prepareProject(workspaceId: string): Promise<ProjectSetupOutcome> {
  return invoke<ProjectSetupOutcome>("prepare_project", { workspaceId });
}

export async function getProjectMemory(workspaceId: string): Promise<ProjectMemory> {
  return invoke<ProjectMemory>("get_project_memory", { workspaceId });
}

export async function getResumeSnapshot(workspaceId: string): Promise<ResumeSnapshot> {
  return invoke<ResumeSnapshot>("get_resume_snapshot", { workspaceId });
}

export async function updateProjectMemory(
  workspaceId: string,
  memory: Pick<ProjectMemory, "summary" | "goals" | "decisions" | "preferences" | "nextSteps">,
): Promise<ProjectMemory> {
  return invoke<ProjectMemory>("update_project_memory", { workspaceId, ...memory });
}

export async function inspectProject(
  workspaceId: string,
  entryLimit = 800,
): Promise<ProjectSnapshot> {
  return invoke<ProjectSnapshot>("inspect_project", { workspaceId, entryLimit });
}

export async function getWorkflowReadiness(
  workspaceId: string,
): Promise<WorkflowReadiness> {
  return invoke<WorkflowReadiness>("get_workflow_readiness", { workspaceId });
}

export async function listChanges(workspaceId?: string, limit = 40): Promise<ChangeRecord[]> {
  return invoke<ChangeRecord[]>("list_changes", {
    workspaceId: workspaceId ?? null,
    limit,
  });
}

export async function approveChange(changeId: string): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("approve_change", { changeId });
}

export async function rejectChange(changeId: string): Promise<ChangeRecord> {
  return invoke<ChangeRecord>("reject_change", { changeId });
}

export async function undoChange(changeId: string): Promise<ChangeRecord> {
  return invoke<ChangeRecord>("undo_change", { changeId });
}


export async function getVersionTimeline(workspaceId?: string): Promise<VersionTimeline> {
  return invoke<VersionTimeline>("get_version_timeline", { workspaceId: workspaceId ?? null });
}

export async function getActivityTimeline(workspaceId?: string): Promise<ActivityTimeline> {
  return invoke<ActivityTimeline>("get_activity_timeline", { workspaceId: workspaceId ?? null });
}

export async function clearVersionHistory(workspaceId: string): Promise<HistoryClearResult> {
  return invoke<HistoryClearResult>("clear_version_history", { workspaceId });
}

export async function getHistorySettings(): Promise<HistorySettings> {
  return invoke<HistorySettings>("get_history_settings");
}

export async function updateHistorySettings(
  versionHistoryLimit: number | null,
  checkpointLimit: number | null,
): Promise<HistorySettings> {
  return invoke<HistorySettings>("update_history_settings", {
    versionHistoryLimit,
    checkpointLimit,
  });
}

export async function restoreVersion(
  workspaceId: string,
  versionId: string | null,
): Promise<VersionRestoreResult> {
  return invoke<VersionRestoreResult>("restore_version", { workspaceId, versionId });
}

export async function getExecutionStatus(): Promise<ExecutionStatus> {
  return invoke<ExecutionStatus>("get_execution_status");
}

export async function listCommandPresets(workspaceId: string): Promise<CommandPreset[]> {
  return invoke<CommandPreset[]>("list_command_presets", { workspaceId });
}

export async function runWorkspaceCommand(
  workspaceId: string,
  presetId: string,
): Promise<CommandOutcome> {
  return invoke<CommandOutcome>("run_workspace_command", { workspaceId, presetId });
}

export async function listCommandHistory(
  workspaceId?: string,
  limit = 40,
): Promise<CommandRecord[]> {
  return invoke<CommandRecord[]>("list_command_history", {
    workspaceId: workspaceId ?? null,
    limit,
  });
}

export async function approveCommand(commandId: string): Promise<CommandRecord> {
  return invoke<CommandRecord>("approve_command", { commandId });
}

export async function rejectCommand(commandId: string): Promise<CommandRecord> {
  return invoke<CommandRecord>("reject_command", { commandId });
}

export async function runTerminalCommand(
  workspaceId: string,
  command: string,
  cwd?: string,
  timeoutSeconds?: number,
  env?: Record<string, string>,
): Promise<TerminalCommandOutcome> {
  return invoke<TerminalCommandOutcome>("run_terminal_command", {
    workspaceId,
    command,
    cwd: cwd ?? null,
    timeoutSeconds: timeoutSeconds ?? null,
    env: env ?? null,
  });
}

export async function runLocalTerminalCommand(
  workspaceId: string,
  command: string,
  cwd?: string,
  timeoutSeconds?: number,
): Promise<TerminalCommandOutcome> {
  return invoke<TerminalCommandOutcome>("run_local_terminal_command", {
    workspaceId,
    command,
    cwd: cwd ?? null,
    timeoutSeconds: timeoutSeconds ?? null,
  });
}

export async function listTerminalHistory(
  workspaceId?: string,
  limit = 40,
): Promise<TerminalCommandRecord[]> {
  return invoke<TerminalCommandRecord[]>("list_terminal_history", {
    workspaceId: workspaceId ?? null,
    limit,
  });
}

export async function approveTerminalCommand(commandId: string): Promise<TerminalCommandRecord> {
  return invoke<TerminalCommandRecord>("approve_terminal_command", { commandId });
}

export async function rejectTerminalCommand(commandId: string): Promise<TerminalCommandRecord> {
  return invoke<TerminalCommandRecord>("reject_terminal_command", { commandId });
}

export async function startManagedProcess(
  workspaceId: string,
  command: string,
  cwd?: string,
  label?: string,
  env?: Record<string, string>,
): Promise<ManagedProcessOutcome> {
  return invoke<ManagedProcessOutcome>("start_managed_process", {
    workspaceId,
    command,
    cwd: cwd ?? null,
    label: label ?? null,
    env: env ?? null,
  });
}

export async function startLocalManagedProcess(
  workspaceId: string,
  command: string,
  cwd?: string,
  label?: string,
): Promise<ManagedProcessOutcome> {
  return invoke<ManagedProcessOutcome>("start_local_managed_process", {
    workspaceId,
    command,
    cwd: cwd ?? null,
    label: label ?? null,
  });
}

export async function listManagedProcesses(
  workspaceId?: string,
  limit = 60,
): Promise<ManagedProcessRecord[]> {
  return invoke<ManagedProcessRecord[]>("list_managed_processes", {
    workspaceId: workspaceId ?? null,
    limit,
  });
}

export async function readManagedProcessOutput(
  processId: string,
  stdoutOffset = 0,
  stderrOffset = 0,
  maxBytes = 64 * 1024,
): Promise<ManagedProcessOutput> {
  return invoke<ManagedProcessOutput>("read_managed_process_output", {
    processId,
    stdoutOffset,
    stderrOffset,
    maxBytes,
  });
}

export async function approveManagedProcess(processId: string): Promise<ManagedProcessRecord> {
  return invoke<ManagedProcessRecord>("approve_managed_process", { processId });
}

export async function rejectManagedProcess(processId: string): Promise<ManagedProcessRecord> {
  return invoke<ManagedProcessRecord>("reject_managed_process", { processId });
}

export async function stopManagedProcess(
  processId: string,
  force = false,
): Promise<ManagedProcessRecord> {
  return invoke<ManagedProcessRecord>("stop_managed_process", { processId, force });
}

export async function restartManagedProcess(processId: string): Promise<ManagedProcessRecord> {
  return invoke<ManagedProcessRecord>("restart_managed_process", { processId });
}

export async function listLaunchableApplications(workspaceId: string): Promise<LaunchApplication[]> {
  return invoke<LaunchApplication[]>("list_launchable_applications", { workspaceId });
}

export async function getGithubConnectionStatus(): Promise<GithubConnectionStatus> {
  return invoke<GithubConnectionStatus>("get_github_connection_status");
}

export async function connectGithub(): Promise<GithubConnectionStatus> {
  return invoke<GithubConnectionStatus>("connect_github");
}

export async function cancelGithubConnection(): Promise<GithubConnectionStatus> {
  return invoke<GithubConnectionStatus>("cancel_github_connection");
}

export async function disconnectGithub(): Promise<GithubConnectionStatus> {
  return invoke<GithubConnectionStatus>("disconnect_github");
}

export async function listDeepIntegrations(workspaceId: string): Promise<DeepIntegration[]> {
  return invoke<DeepIntegration[]>("list_deep_integrations", { workspaceId });
}

export async function setDeepIntegrationEnabled(
  workspaceId: string,
  integrationId: string,
  enabled: boolean,
): Promise<DeepIntegration[]> {
  return invoke<DeepIntegration[]>("set_deep_integration_enabled", { workspaceId, integrationId, enabled });
}

export async function listDesktopControlApplications(workspaceId: string): Promise<DesktopControlApplication[]> {
  return invoke<DesktopControlApplication[]>("list_desktop_control_applications", { workspaceId });
}

export async function getDesktopControlEnabled(workspaceId: string): Promise<boolean> {
  return invoke<boolean>("get_desktop_control_enabled", { workspaceId });
}

export async function setDesktopControlEnabled(
  workspaceId: string,
  enabled: boolean,
): Promise<boolean> {
  return invoke<boolean>("set_desktop_control_enabled", { workspaceId, enabled });
}

export async function getGmailAccessEnabled(workspaceId: string): Promise<boolean> {
  return invoke<boolean>("get_gmail_access_enabled", { workspaceId });
}

export async function setGmailAccessEnabled(
  workspaceId: string,
  enabled: boolean,
): Promise<boolean> {
  return invoke<boolean>("set_gmail_access_enabled", { workspaceId, enabled });
}

export async function getAiWorkspaceStatus(workspaceId: string): Promise<AiWorkspaceStatus> {
  return invoke<AiWorkspaceStatus>("get_ai_workspace_status", { workspaceId });
}

export async function startAiWorkspace(
  workspaceId: string,
  applicationId: string,
  target?: string,
): Promise<AiWorkspaceStatus> {
  return invoke<AiWorkspaceStatus>("start_ai_workspace", {
    workspaceId,
    applicationId,
    target: target?.trim() || null,
  });
}

export async function stopAiWorkspace(
  workspaceId: string,
  appSessionId: string,
): Promise<AiWorkspaceStatus> {
  return invoke<AiWorkspaceStatus>("stop_ai_workspace", { workspaceId, appSessionId });
}

export async function getAiWorkspaceFrame(
  workspaceId: string,
  appSessionId: string,
  maxWidth = 1440,
): Promise<AiWorkspaceFrame> {
  return invoke<AiWorkspaceFrame>("get_ai_workspace_frame", {
    workspaceId,
    appSessionId,
    maxWidth,
  });
}

export async function aiWorkspaceAction(
  workspaceId: string,
  appSessionId: string,
  action: "activate" | "click" | "key" | "type" | "scroll",
  options: {
    windowId?: string;
    xRatio?: number;
    yRatio?: number;
    clickCount?: number;
    shortcut?: string;
    text?: string;
    deltaX?: number;
    deltaY?: number;
  } = {},
): Promise<Record<string, unknown>> {
  return invoke<Record<string, unknown>>("ai_workspace_action", {
    workspaceId,
    appSessionId,
    action,
    windowId: options.windowId ?? null,
    xRatio: options.xRatio ?? null,
    yRatio: options.yRatio ?? null,
    clickCount: options.clickCount ?? null,
    shortcut: options.shortcut ?? null,
    text: options.text ?? null,
    deltaX: options.deltaX ?? null,
    deltaY: options.deltaY ?? null,
  });
}

export async function openUrl(
  workspaceId: string,
  url: string,
  applicationId?: string,
): Promise<LaunchActionOutcome> {
  return invoke<LaunchActionOutcome>("open_url", {
    workspaceId,
    url,
    applicationId: applicationId ?? null,
  });
}

export async function openWorkspacePath(
  workspaceId: string,
  relativePath: string,
  applicationId?: string,
): Promise<LaunchActionOutcome> {
  return invoke<LaunchActionOutcome>("open_workspace_path", {
    workspaceId,
    relativePath,
    applicationId: applicationId ?? null,
  });
}

export async function launchApplication(
  workspaceId: string,
  applicationId: string,
): Promise<LaunchActionOutcome> {
  return invoke<LaunchActionOutcome>("launch_application", { workspaceId, applicationId });
}

export async function listLaunchHistory(
  workspaceId?: string,
  limit = 60,
): Promise<LaunchActionRecord[]> {
  return invoke<LaunchActionRecord[]>("list_launch_history", {
    workspaceId: workspaceId ?? null,
    limit,
  });
}

export async function approveLaunchAction(launchId: string): Promise<LaunchActionRecord> {
  return invoke<LaunchActionRecord>("approve_launch_action", { launchId });
}

export async function rejectLaunchAction(launchId: string): Promise<LaunchActionRecord> {
  return invoke<LaunchActionRecord>("reject_launch_action", { launchId });
}

export async function listAutomationBrowsers(workspaceId: string): Promise<BrowserApplication[]> {
  return invoke<BrowserApplication[]>("list_automation_browsers", { workspaceId });
}

export async function getBrowserAutomationStatus(workspaceId: string): Promise<BrowserAutomationStatus> {
  return invoke<BrowserAutomationStatus>("get_browser_automation_status", { workspaceId });
}

export async function startBrowserAutomation(
  workspaceId: string,
  applicationId: string,
): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("start_browser_automation", { workspaceId, applicationId });
}

export async function stopBrowserAutomation(workspaceId: string): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("stop_browser_automation", { workspaceId });
}

export async function listBrowserTabs(workspaceId: string): Promise<BrowserTab[]> {
  return invoke<BrowserTab[]>("list_browser_tabs", { workspaceId });
}

export async function browserOpenTab(workspaceId: string, url: string): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("browser_open_tab", { workspaceId, url });
}

export async function browserActivateTab(workspaceId: string, tabId: string): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("browser_activate_tab", { workspaceId, tabId });
}

export async function browserCloseTab(workspaceId: string, tabId: string): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("browser_close_tab", { workspaceId, tabId });
}

export async function browserNavigate(
  workspaceId: string,
  tabId: string,
  url: string,
): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("browser_navigate", { workspaceId, tabId, url });
}

export async function browserClick(
  workspaceId: string,
  tabId: string,
  selector: string,
): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("browser_click", { workspaceId, tabId, selector });
}

export async function browserType(
  workspaceId: string,
  tabId: string,
  selector: string,
  text: string,
  clearFirst = true,
): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("browser_type", { workspaceId, tabId, selector, text, clearFirst });
}

export async function browserScroll(
  workspaceId: string,
  tabId: string,
  deltaX: number,
  deltaY: number,
): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("browser_scroll", { workspaceId, tabId, deltaX, deltaY });
}

export async function browserReload(workspaceId: string, tabId: string): Promise<BrowserActionOutcome> {
  return invoke<BrowserActionOutcome>("browser_reload", { workspaceId, tabId });
}

export async function browserInspectPage(
  workspaceId: string,
  tabId: string,
  selector?: string,
  maxChars = 12000,
): Promise<BrowserPageInspection> {
  return invoke<BrowserPageInspection>("browser_inspect_page", {
    workspaceId,
    tabId,
    selector: selector?.trim() ? selector.trim() : null,
    maxChars,
  });
}

export async function browserPickElement(
  workspaceId: string,
  tabId: string,
  xRatio: number,
  yRatio: number,
): Promise<BrowserVisualSelection> {
  return invoke<BrowserVisualSelection>("browser_pick_element", { workspaceId, tabId, xRatio, yRatio });
}

export async function getBrowserVisualSelection(
  workspaceId: string,
): Promise<BrowserVisualSelection | null> {
  return invoke<BrowserVisualSelection | null>("get_browser_visual_selection", { workspaceId });
}

export async function browserTakeScreenshot(
  workspaceId: string,
  tabId: string,
  fullPage = false,
): Promise<BrowserScreenshot> {
  return invoke<BrowserScreenshot>("browser_take_screenshot", { workspaceId, tabId, fullPage });
}

export async function getBrowserDiagnostics(
  workspaceId: string,
  tabId?: string,
  limit = 40,
): Promise<BrowserDiagnostics> {
  return invoke<BrowserDiagnostics>("get_browser_diagnostics", {
    workspaceId,
    tabId: tabId ?? null,
    limit,
  });
}

export async function listBrowserHistory(
  workspaceId?: string,
  limit = 60,
): Promise<BrowserActionRecord[]> {
  return invoke<BrowserActionRecord[]>("list_browser_history", {
    workspaceId: workspaceId ?? null,
    limit,
  });
}

export async function approveBrowserAction(actionId: string): Promise<BrowserActionRecord> {
  return invoke<BrowserActionRecord>("approve_browser_action", { actionId });
}

export async function rejectBrowserAction(actionId: string): Promise<BrowserActionRecord> {
  return invoke<BrowserActionRecord>("reject_browser_action", { actionId });
}

export async function getMonitoringStatus(workspaceId: string): Promise<MonitoringStatus> {
  return invoke<MonitoringStatus>("get_monitoring_status", { workspaceId });
}

export async function startWorkspaceMonitoring(workspaceId: string): Promise<MonitoringStatus> {
  return invoke<MonitoringStatus>("start_workspace_monitoring", { workspaceId });
}

export async function stopWorkspaceMonitoring(workspaceId: string): Promise<MonitoringStatus> {
  return invoke<MonitoringStatus>("stop_workspace_monitoring", { workspaceId });
}

export async function getMonitoringSnapshot(workspaceId: string): Promise<MonitoringSnapshot> {
  return invoke<MonitoringSnapshot>("get_monitoring_snapshot", { workspaceId });
}

export async function listMonitoringFileEvents(
  workspaceId?: string,
  limit = 60,
): Promise<MonitoringFileEvent[]> {
  return invoke<MonitoringFileEvent[]>("list_monitoring_file_events", {
    workspaceId: workspaceId ?? null,
    limit,
  });
}

export async function getGitStatus(workspaceId: string): Promise<GitRepositoryStatus> {
  return invoke<GitRepositoryStatus>("get_git_status", { workspaceId });
}

export async function getGitDiff(workspaceId: string, staged: boolean): Promise<GitDiff> {
  return invoke<GitDiff>("get_git_diff", { workspaceId, staged });
}

export async function getGitLog(
  workspaceId: string,
  limit = 12,
): Promise<GitCommitSummary[]> {
  return invoke<GitCommitSummary[]>("get_git_log", { workspaceId, limit });
}

export async function requestGitStage(
  workspaceId: string,
  paths: string[],
): Promise<GitActionRecord> {
  return invoke<GitActionRecord>("request_git_stage", { workspaceId, paths });
}

export async function requestGitCommit(
  workspaceId: string,
  message: string,
): Promise<GitActionRecord> {
  return invoke<GitActionRecord>("request_git_commit", { workspaceId, message });
}

export async function listGitActions(
  workspaceId?: string,
  limit = 40,
): Promise<GitActionRecord[]> {
  return invoke<GitActionRecord[]>("list_git_actions", {
    workspaceId: workspaceId ?? null,
    limit,
  });
}

export async function approveGitAction(actionId: string): Promise<GitActionRecord> {
  return invoke<GitActionRecord>("approve_git_action", { actionId });
}

export async function rejectGitAction(actionId: string): Promise<GitActionRecord> {
  return invoke<GitActionRecord>("reject_git_action", { actionId });
}

export async function requestGitRestoreFile(
  workspaceId: string,
  relativePath: string,
): Promise<ChangeOutcome> {
  return invoke<ChangeOutcome>("request_git_restore_file", { workspaceId, relativePath });
}

export async function createCheckpoint(workspaceId: string): Promise<CheckpointSummary> {
  return invoke<CheckpointSummary>("create_checkpoint", { workspaceId });
}

export async function listCheckpoints(workspaceId?: string): Promise<CheckpointSummary[]> {
  return invoke<CheckpointSummary[]>("list_checkpoints", { workspaceId: workspaceId ?? null });
}

export async function compareCheckpoint(
  workspaceId: string,
  checkpointId: string,
): Promise<CheckpointComparison> {
  return invoke<CheckpointComparison>("compare_checkpoint", { workspaceId, checkpointId });
}

export async function restoreCheckpoint(
  workspaceId: string,
  checkpointId: string,
): Promise<CheckpointRestoreResult> {
  return invoke<CheckpointRestoreResult>("restore_checkpoint", { workspaceId, checkpointId });
}

export async function deleteCheckpoint(
  workspaceId: string,
  checkpointId: string,
): Promise<void> {
  return invoke<void>("delete_checkpoint", { workspaceId, checkpointId });
}

export async function renameCheckpoint(
  workspaceId: string,
  checkpointId: string,
  name: string | null,
): Promise<CheckpointSummary> {
  return invoke<CheckpointSummary>("rename_checkpoint", { workspaceId, checkpointId, name });
}

export async function setCheckpointPinned(
  workspaceId: string,
  checkpointId: string,
  pinned: boolean,
): Promise<CheckpointSummary> {
  return invoke<CheckpointSummary>("set_checkpoint_pinned", {
    workspaceId,
    checkpointId,
    pinned,
  });
}

export async function clearCheckpoints(workspaceId?: string): Promise<CheckpointClearResult> {
  return invoke<CheckpointClearResult>("clear_checkpoints", { workspaceId: workspaceId ?? null });
}

export async function runSafetyScan(workspaceId: string): Promise<SafetyScanResult> {
  return invoke<SafetyScanResult>("run_safety_scan", { workspaceId });
}

export async function getAiAccessStatus(): Promise<AiAccessStatus> {
  return invoke<AiAccessStatus>("get_ai_access_status");
}

export async function setAiAccessPaused(paused: boolean): Promise<AiAccessStatus> {
  return invoke<AiAccessStatus>("set_ai_access_paused", { paused });
}

export async function getGatewayStatus(): Promise<GatewayStatus> {
  return invoke<GatewayStatus>("get_gateway_status");
}

export async function startGateway(): Promise<GatewayStatus> {
  return invoke<GatewayStatus>("start_gateway");
}

export async function stopGateway(): Promise<GatewayStatus> {
  return invoke<GatewayStatus>("stop_gateway");
}

export async function getPublicTunnelStatus(): Promise<PublicTunnelStatus> {
  return invoke<PublicTunnelStatus>("get_public_tunnel_status");
}

export async function getPhoneDiscovery(): Promise<PhoneDiscoveryStatus> {
  return invoke<PhoneDiscoveryStatus>("get_phone_discovery");
}

export async function pairPhoneWirelessly(code: string): Promise<PhoneDiscoveryStatus> {
  return invoke<PhoneDiscoveryStatus>("pair_phone_wirelessly", { code });
}

export async function getPhoneAccessStatus(): Promise<PhoneAccessStatus> {
  return invoke<PhoneAccessStatus>("get_phone_access_status");
}

export async function selectPhoneDevice(deviceId: string | null): Promise<PhoneAccessStatus> {
  return invoke<PhoneAccessStatus>("select_phone_device", { deviceId });
}

export async function setPhoneAccessMode(
  deviceId: string,
  mode: PhoneAccessMode,
  limitedCapabilities: PhoneCapability[],
): Promise<PhoneAccessStatus> {
  return invoke<PhoneAccessStatus>("set_phone_access_mode", {
    deviceId,
    mode,
    limitedCapabilities,
  });
}

export async function setPhoneAccessPaused(paused: boolean): Promise<PhoneAccessStatus> {
  return invoke<PhoneAccessStatus>("set_phone_access_paused", { paused });
}

export async function getPhoneRuntimeStatus(): Promise<PhoneRuntimeStatus> {
  return invoke<PhoneRuntimeStatus>("get_phone_runtime_status");
}

export async function ensurePhoneRuntime(
  deviceId: string,
  preferredTransport?: "usb" | "wireless",
): Promise<PhoneRuntimeStatus> {
  return invoke<PhoneRuntimeStatus>("ensure_phone_runtime", {
    deviceId,
    preferredTransport,
  });
}

export async function clearPhoneRuntime(): Promise<PhoneRuntimeStatus> {
  return invoke<PhoneRuntimeStatus>("clear_phone_runtime");
}

export async function probePhoneRuntime(deviceId: string): Promise<PhoneRuntimeProbe> {
  return invoke<PhoneRuntimeProbe>("probe_phone_runtime", { deviceId });
}

export async function getPhoneScreenFrame(
  deviceId: string,
  afterCapturedAt?: number,
): Promise<PhoneScreenFrame> {
  return invoke<PhoneScreenFrame>("get_phone_screen_frame", {
    deviceId,
    afterCapturedAt,
  });
}

export async function phoneTap(deviceId: string, xRatio: number, yRatio: number): Promise<void> {
  return invoke<void>("phone_tap", { deviceId, xRatio, yRatio });
}

export async function phoneSwipe(
  deviceId: string,
  startXRatio: number,
  startYRatio: number,
  endXRatio: number,
  endYRatio: number,
  durationMs: number,
): Promise<void> {
  return invoke<void>("phone_swipe", {
    deviceId,
    startXRatio,
    startYRatio,
    endXRatio,
    endYRatio,
    durationMs,
  });
}

export type PhoneUiKey =
  | "back"
  | "home"
  | "enter"
  | "recents"
  | "escape"
  | "tab"
  | "delete"
  | "dpadUp"
  | "dpadDown"
  | "dpadLeft"
  | "dpadRight";

export async function phoneKeyEvent(deviceId: string, key: PhoneUiKey): Promise<void> {
  return invoke<void>("phone_key_event", { deviceId, key });
}

export async function phoneTypeText(deviceId: string, text: string): Promise<void> {
  return invoke<void>("phone_type_text", { deviceId, text });
}

export async function getHttpsSetupReadiness(): Promise<HttpsSetupReadiness> {
  return invoke<HttpsSetupReadiness>("get_https_setup_readiness");
}

export async function installHttpsSetupWireguardConfig(): Promise<HttpsSetupInstallResult> {
  return invoke<HttpsSetupInstallResult>("install_https_setup_wireguard_config");
}

export async function openHttpsSetupResource(resource: HttpsSetupResource): Promise<void> {
  return invoke<void>("open_https_setup_resource", { resource });
}

export async function verifyHttpsSetupHostname(
  hostname: string,
): Promise<HttpsSetupHostnameVerification> {
  return invoke<HttpsSetupHostnameVerification>("verify_https_setup_hostname", { hostname });
}

export async function configurePublicTunnel(
  provider: PublicTunnelProvider,
  credential: string,
  publicUrl?: string,
): Promise<PublicTunnelStatus> {
  return invoke<PublicTunnelStatus>("configure_public_tunnel", {
    provider,
    credential,
    publicUrl: publicUrl?.trim() ? publicUrl.trim() : null,
  });
}

export async function restartPublicTunnel(): Promise<PublicTunnelStatus> {
  return invoke<PublicTunnelStatus>("restart_public_tunnel");
}

export async function provisionDirectHttpsCertificate(staging = false): Promise<PublicTunnelStatus> {
  return invoke<PublicTunnelStatus>("provision_direct_https_certificate", { staging });
}

export async function clearPublicTunnel(): Promise<PublicTunnelStatus> {
  return invoke<PublicTunnelStatus>("clear_public_tunnel");
}

export async function revokeMcpAccess(): Promise<void> {
  return invoke<void>("revoke_mcp_access");
}

export async function getChatConnectionStatus(): Promise<ChatConnectionStatus> {
  return invoke<ChatConnectionStatus>("get_chat_connection_status");
}

export async function startChatConnection(
  tunnelId: string,
  apiKey: string,
): Promise<ChatConnectionStatus> {
  return invoke<ChatConnectionStatus>("start_chat_connection", { tunnelId, apiKey });
}

export async function stopChatConnection(): Promise<ChatConnectionStatus> {
  return invoke<ChatConnectionStatus>("stop_chat_connection");
}


export async function getVideoToolsStatus(): Promise<VideoToolsStatus> {
  return invoke<VideoToolsStatus>("get_video_tools_status");
}

export async function installVideoTools(): Promise<VideoToolsStatus> {
  return invoke<VideoToolsStatus>("install_video_tools");
}

export async function startVideoAnalysis(
  workspaceId: string,
  source: string,
  mode: VideoAnalysisMode,
  startSeconds?: number,
  endSeconds?: number,
  maxFrames?: number,
): Promise<VideoAnalysisJob> {
  return invoke<VideoAnalysisJob>("start_video_analysis", {
    workspaceId,
    source,
    mode,
    startSeconds: startSeconds ?? null,
    endSeconds: endSeconds ?? null,
    maxFrames: maxFrames ?? null,
  });
}

export async function getVideoAnalysisJob(jobId: string): Promise<VideoAnalysisJob> {
  return invoke<VideoAnalysisJob>("get_video_analysis_job", { jobId });
}

export async function listVideoAnalysisJobs(workspaceId?: string, limit = 20): Promise<VideoAnalysisJob[]> {
  return invoke<VideoAnalysisJob[]>("list_video_analysis_jobs", {
    workspaceId: workspaceId ?? null,
    limit,
  });
}

export async function getVideoAnalysisResult(jobId: string): Promise<VideoAnalysisResult> {
  return invoke<VideoAnalysisResult>("get_video_analysis_result", { jobId });
}

export async function cancelVideoAnalysis(jobId: string): Promise<VideoAnalysisJob> {
  return invoke<VideoAnalysisJob>("cancel_video_analysis", { jobId });
}

export async function clearVideoCache(): Promise<VideoToolsStatus> {
  return invoke<VideoToolsStatus>("clear_video_cache");
}

export async function createVideoProject(
  workspaceId: string,
  name: string,
  productionMode: "tutorial" | "story" = "tutorial",
  aspectRatio: "16:9" | "9:16" | "1:1" | "4:5" = "16:9",
  width?: number,
  height?: number,
  fps?: number,
): Promise<VideoProductionProject> {
  return invoke<VideoProductionProject>("create_video_project", {
    workspaceId,
    name,
    productionMode,
    aspectRatio,
    width: width ?? null,
    height: height ?? null,
    fps: fps ?? null,
  });
}

export async function listVideoProjects(workspaceId: string): Promise<VideoProductionProject[]> {
  return invoke<VideoProductionProject[]>("list_video_projects", { workspaceId });
}

export async function importVideoProjectFolder(
  workspaceId: string,
  folderPath: string,
): Promise<VideoProductionProject> {
  return invoke<VideoProductionProject>("import_video_project_folder", {
    workspaceId,
    folderPath,
  });
}

export async function setVideoProjectPinned(
  workspaceId: string,
  projectId: string,
  pinned: boolean,
): Promise<VideoProductionProject> {
  return invoke<VideoProductionProject>("set_video_project_pinned", { workspaceId, projectId, pinned });
}

export async function setVideoProjectResourcePolicy(
  workspaceId: string,
  projectId: string,
  policy: VideoProductionResourcePolicy,
): Promise<VideoProductionProject> {
  return invoke<VideoProductionProject>("set_video_project_resource_policy", {
    workspaceId,
    projectId,
    policy,
  });
}

export async function upsertVideoProjectScene(
  workspaceId: string,
  projectId: string,
  scene: VideoProductionSceneInput,
): Promise<VideoProductionScene> {
  return invoke<VideoProductionScene>("upsert_video_project_scene", {
    workspaceId,
    projectId,
    scene,
  });
}

export async function getVideoProjectScene(
  workspaceId: string,
  projectId: string,
  sceneId: string,
): Promise<VideoProductionScene> {
  return invoke<VideoProductionScene>("get_video_project_scene", {
    workspaceId,
    projectId,
    sceneId,
  });
}

export async function listVideoProjectScenes(
  workspaceId: string,
  projectId: string,
): Promise<VideoProductionScene[]> {
  return invoke<VideoProductionScene[]>("list_video_project_scenes", {
    workspaceId,
    projectId,
  });
}

export async function listVideoProjectFiles(
  workspaceId: string,
  projectId: string,
): Promise<VideoProjectFile[]> {
  return invoke<VideoProjectFile[]>("list_video_project_files", { workspaceId, projectId });
}

export async function readVideoProjectTextFile(
  workspaceId: string,
  projectId: string,
  relativePath: string,
): Promise<string> {
  return invoke<string>("read_video_project_text_file", { workspaceId, projectId, relativePath });
}

export async function deleteVideoProject(
  workspaceId: string,
  projectId: string,
): Promise<void> {
  return invoke<void>("delete_video_project", { workspaceId, projectId });
}

export async function getVideoProject(
  workspaceId: string,
  projectId: string,
): Promise<VideoProductionProject> {
  return invoke<VideoProductionProject>("get_video_project", { workspaceId, projectId });
}

export async function getVideoStoryCapabilities(): Promise<StoryProductionCapabilities> {
  return invoke<StoryProductionCapabilities>("get_video_story_capabilities");
}

export async function compileVideoStoryPlan(
  workspaceId: string,
  projectId: string,
  input: StoryDirectorInput,
): Promise<StoryDirectorPlan> {
  return invoke<StoryDirectorPlan>("compile_video_story_plan", {
    workspaceId,
    projectId,
    input,
  });
}

export async function getVideoStoryPlan(
  workspaceId: string,
  projectId: string,
): Promise<StoryDirectorPlan> {
  return invoke<StoryDirectorPlan>("get_video_story_plan", { workspaceId, projectId });
}

export async function getVideoStoryAnimaticPlan(
  workspaceId: string,
  projectId: string,
): Promise<StoryAnimaticPlan> {
  return invoke<StoryAnimaticPlan>("get_video_story_animatic_plan", { workspaceId, projectId });
}

export async function renderVideoStoryAnimatic(
  workspaceId: string,
  projectId: string,
): Promise<StoryAnimaticRender> {
  return invoke<StoryAnimaticRender>("render_video_story_animatic", {
    workspaceId,
    projectId,
  });
}

export async function getVideoStoryQa(
  workspaceId: string,
  projectId: string,
): Promise<NarrativeQaReport> {
  return invoke<NarrativeQaReport>("get_video_story_qa", { workspaceId, projectId });
}

export async function getVideoStoryRenderQueue(
  workspaceId: string,
  projectId: string,
): Promise<StoryRenderQueue> {
  return invoke<StoryRenderQueue>("get_video_story_render_queue", { workspaceId, projectId });
}

export async function recordVideoStoryShotRender(
  workspaceId: string,
  projectId: string,
  input: StoryShotRenderInput,
): Promise<StoryRenderQueue> {
  return invoke<StoryRenderQueue>("record_video_story_shot_render", {
    workspaceId,
    projectId,
    input,
  });
}

export async function startVideoStoryShotRender(
  workspaceId: string,
  projectId: string,
  shotId: string,
  force = false,
): Promise<StoryShotRenderJob> {
  return invoke<StoryShotRenderJob>("start_video_story_shot_render", {
    workspaceId,
    projectId,
    shotId,
    force,
  });
}

export async function getVideoStoryShotRender(
  workspaceId: string,
  projectId: string,
  jobId: string,
): Promise<StoryShotRenderJob> {
  return invoke<StoryShotRenderJob>("get_video_story_shot_render", {
    workspaceId,
    projectId,
    jobId,
  });
}

export async function cancelVideoStoryShotRender(
  workspaceId: string,
  projectId: string,
  jobId: string,
): Promise<StoryShotRenderJob> {
  return invoke<StoryShotRenderJob>("cancel_video_story_shot_render", {
    workspaceId,
    projectId,
    jobId,
  });
}

export async function updateVideoProjectStatus(
  workspaceId: string,
  projectId: string,
  status: string,
  detail?: string,
): Promise<VideoProductionProject> {
  return invoke<VideoProductionProject>("update_video_project_status", {
    workspaceId,
    projectId,
    status,
    detail: detail ?? null,
  });
}

export async function writeVideoProjectDocument(
  workspaceId: string,
  projectId: string,
  document: "script" | "storyboard" | "timeline",
  content: string,
): Promise<VideoProductionDocument> {
  return invoke<VideoProductionDocument>("write_video_project_document", {
    workspaceId,
    projectId,
    document,
    content,
  });
}

export async function readVideoProjectDocument(
  workspaceId: string,
  projectId: string,
  document: "script" | "storyboard" | "timeline",
): Promise<VideoProductionDocument> {
  return invoke<VideoProductionDocument>("read_video_project_document", {
    workspaceId,
    projectId,
    document,
  });
}

export async function startVideoProjectRecording(
  workspaceId: string,
  projectId: string,
  appSessionId: string,
  fps = 30,
  maxSeconds = 900,
): Promise<VideoRecordingStatus> {
  return invoke<VideoRecordingStatus>("start_video_project_recording", {
    workspaceId,
    projectId,
    appSessionId,
    fps,
    maxSeconds,
  });
}

export async function getVideoProjectRecording(
  workspaceId: string,
  projectId?: string,
): Promise<VideoRecordingStatus | null> {
  return invoke<VideoRecordingStatus | null>("get_video_project_recording", {
    workspaceId,
    projectId: projectId ?? null,
  });
}

export async function stopVideoProjectRecording(
  workspaceId: string,
  projectId: string,
): Promise<VideoRecordingStatus> {
  return invoke<VideoRecordingStatus>("stop_video_project_recording", {
    workspaceId,
    projectId,
  });
}

export async function validateVideoProjectScene(
  workspaceId: string,
  projectId: string,
  scene: VideoSceneSpec,
): Promise<VideoSceneLayoutReport> {
  return invoke<VideoSceneLayoutReport>("validate_video_project_scene", {
    workspaceId,
    projectId,
    scene,
  });
}

export async function renderVideoProjectScene(
  workspaceId: string,
  projectId: string,
  scene: VideoSceneSpec,
): Promise<VideoSceneRender> {
  return invoke<VideoSceneRender>("render_video_project_scene", {
    workspaceId,
    projectId,
    scene,
  });
}

export async function renderVideoProjectDiagram(
  workspaceId: string,
  projectId: string,
  diagram: VideoDiagramSpec,
): Promise<VideoSceneRender> {
  return invoke<VideoSceneRender>("render_video_project_diagram", {
    workspaceId,
    projectId,
    diagram,
  });
}

export async function startVideoProjectRender(
  workspaceId: string,
  projectId: string,
  request: VideoRenderRequest,
): Promise<VideoRenderJob> {
  return invoke<VideoRenderJob>("start_video_project_render", {
    workspaceId,
    projectId,
    request,
  });
}

export async function getVideoProjectRender(
  workspaceId: string,
  projectId: string,
  jobId: string,
): Promise<VideoRenderJob> {
  return invoke<VideoRenderJob>("get_video_project_render", {
    workspaceId,
    projectId,
    jobId,
  });
}

export async function listVideoProjectRenders(
  workspaceId: string,
  projectId: string,
): Promise<VideoRenderJob[]> {
  return invoke<VideoRenderJob[]>("list_video_project_renders", {
    workspaceId,
    projectId,
  });
}

export async function cancelVideoProjectRender(
  workspaceId: string,
  projectId: string,
  jobId: string,
): Promise<VideoRenderJob> {
  return invoke<VideoRenderJob>("cancel_video_project_render", {
    workspaceId,
    projectId,
    jobId,
  });
}

export async function getVideoProjectPipelineStatus(
  workspaceId: string,
  projectId: string,
): Promise<VideoPipelineStatus> {
  return invoke<VideoPipelineStatus>("get_video_project_pipeline_status", {
    workspaceId,
    projectId,
  });
}

export async function cleanVideoProject(
  workspaceId: string,
  projectId: string,
  request: VideoCleanupRequest = {},
): Promise<VideoCleanupReport> {
  return invoke<VideoCleanupReport>("clean_video_project", {
    workspaceId,
    projectId,
    request,
  });
}

export async function qaVideoProject(
  workspaceId: string,
  projectId: string,
  assetPath?: string | null,
): Promise<VideoQaReport> {
  return invoke<VideoQaReport>("qa_video_project", {
    workspaceId,
    projectId,
    assetPath: assetPath ?? null,
  });
}

export async function renderVideoProjectTimeline(
  workspaceId: string,
  projectId: string,
  request: VideoRenderRequest,
): Promise<VideoRenderResult> {
  return invoke<VideoRenderResult>("render_video_project_timeline", {
    workspaceId,
    projectId,
    request,
  });
}

export async function prepareVideoProjectPreview(
  workspaceId: string,
  projectId: string,
): Promise<VideoPreviewSource> {
  return invoke<VideoPreviewSource>("prepare_video_project_preview", {
    workspaceId,
    projectId,
  });
}

export async function prepareVideoProjectFilePreview(
  workspaceId: string,
  projectId: string,
  relativePath: string,
): Promise<VideoPreviewSource> {
  return invoke<VideoPreviewSource>("prepare_video_project_file_preview", {
    workspaceId,
    projectId,
    relativePath,
  });
}

export async function readVideoPreviewChunk(
  previewPath: string,
  offset: number,
  length: number,
): Promise<ArrayBuffer> {
  return invoke<ArrayBuffer>("read_video_preview_chunk", {
    previewPath,
    offset,
    length,
  });
}

export async function listVideoAssetSources(): Promise<VideoAssetSource[]> {
  return invoke<VideoAssetSource[]>("list_video_asset_sources");
}

export async function recordVideoAssetLicense(
  workspaceId: string,
  projectId: string,
  input: VideoAssetLicenseInput,
): Promise<VideoAssetLicenseRecord> {
  return invoke<VideoAssetLicenseRecord>("record_video_asset_license", {
    workspaceId,
    projectId,
    input,
  });
}

export async function getVideoNarrationProviders(): Promise<NarrationProviderStatus[]> {
  return invoke<NarrationProviderStatus[]>("get_video_narration_providers");
}

export async function createVideoProjectSubtitles(
  workspaceId: string,
  projectId: string,
  language: string,
  text: string,
  durationSeconds?: number,
): Promise<SubtitleAsset> {
  return invoke<SubtitleAsset>("create_video_project_subtitles", {
    workspaceId,
    projectId,
    language,
    text,
    durationSeconds: durationSeconds ?? null,
  });
}

export async function synthesizeVideoProjectNarration(
  workspaceId: string,
  projectId: string,
  request: NarrationRequest,
): Promise<NarrationAsset> {
  return invoke<NarrationAsset>("synthesize_video_project_narration", {
    workspaceId,
    projectId,
    request,
  });
}

export async function getModelHub(): Promise<ModelHubSnapshot> {
  return invoke<ModelHubSnapshot>("get_model_hub");
}

export async function refreshModelRuntime(provider: ModelProviderId): Promise<RuntimeStatus> {
  return invoke<RuntimeStatus>("refresh_model_runtime", { provider });
}

export async function updateModelRuntimeEndpoint(
  provider: ModelProviderId,
  endpoint: string,
): Promise<string> {
  return invoke<string>("update_model_runtime_endpoint", { provider, endpoint });
}

export async function setSelectedLocalModel(
  selection: ModelSelection | null,
): Promise<ModelSelection | null> {
  return invoke<ModelSelection | null>("set_selected_local_model", { selection });
}

export async function testLocalModel(selection: ModelSelection): Promise<ModelTestResult> {
  return invoke<ModelTestResult>("test_local_model", { selection });
}

export async function getModelTrial(): Promise<ModelTrialSnapshot> {
  return invoke<ModelTrialSnapshot>("get_model_trial");
}

export async function runModelTrial(mode: TrialMode, selections: ModelSelection[]): Promise<ModelTrialSnapshot> {
  return invoke<ModelTrialSnapshot>("run_model_trial", { mode, selections });
}

export async function cancelModelTrial(): Promise<boolean> {
  return invoke<boolean>("cancel_model_trial");
}

export async function listHomeConversations(workspaceId: string | null): Promise<HomeConversationSummary[]> {
  return invoke<HomeConversationSummary[]>("list_home_conversations", { workspaceId });
}

export async function getHomeConversation(conversationId: string): Promise<HomeConversation> {
  return invoke<HomeConversation>("get_home_conversation", { conversationId });
}

export async function createHomeConversation(workspaceId: string | null): Promise<HomeConversation> {
  return invoke<HomeConversation>("create_home_conversation", { workspaceId });
}

export async function deleteHomeConversation(conversationId: string): Promise<void> {
  return invoke<void>("delete_home_conversation", { conversationId });
}

export async function listHomeContextFiles(
  workspaceId: string,
  limit = 250,
): Promise<ProjectEntry[]> {
  return invoke<ProjectEntry[]>("list_home_context_files", { workspaceId, limit });
}

export async function beginHomeChat(
  workspaceId: string | null,
  conversationId: string,
  question: string,
  context: HomeProjectContextRequest,
  allowChanges = false,
): Promise<HomeChatStartResult> {
  return invoke<HomeChatStartResult>("begin_home_chat", {
    workspaceId,
    conversationId,
    question,
    context,
    allowChanges,
  });
}

export async function cancelHomeChat(generationId: string): Promise<boolean> {
  return invoke<boolean>("cancel_home_chat", { generationId });
}

export async function getRuntimeDiagnostics(): Promise<RuntimeDiagnostics> {
  return invoke<RuntimeDiagnostics>("get_runtime_diagnostics");
}

export async function setLaunchAtLogin(enabled: boolean): Promise<RuntimeDiagnostics> {
  return invoke<RuntimeDiagnostics>("set_launch_at_login", { enabled });
}

export async function getUpdateStatus(): Promise<UpdateStatus> {
  return invoke<UpdateStatus>("get_update_status");
}

export async function checkForUpdates(manual = false): Promise<UpdateStatus> {
  return invoke<UpdateStatus>("check_for_updates", { manual });
}

export async function setAutoUpdateChecks(enabled: boolean): Promise<UpdateStatus> {
  return invoke<UpdateStatus>("set_auto_update_checks", { enabled });
}

export async function deferUpdate(version: string): Promise<UpdateStatus> {
  return invoke<UpdateStatus>("defer_update", { version });
}

export async function installUpdateAndRestart(): Promise<UpdateInstallResult> {
  return invoke<UpdateInstallResult>("install_update_and_restart");
}

export async function listTeamSessions(workspaceId?: string): Promise<TeamSessionSummary[]> {
  return invoke<TeamSessionSummary[]>("list_team_sessions", { workspaceId: workspaceId ?? null });
}

export async function getTeamSession(sessionId: string): Promise<TeamSnapshot> {
  return invoke<TeamSnapshot>("get_team_session", { sessionId });
}

export async function createTeamSession(
  workspaceId: string,
  goal: string,
  successCriteria: string[],
  agentAName: string,
  agentARole: string,
  agentBName: string,
  agentBRole: string,
): Promise<TeamSnapshot> {
  return invoke<TeamSnapshot>("create_team_session", {
    workspaceId,
    goal,
    successCriteria,
    agentAName,
    agentARole,
    agentBName,
    agentBRole,
  });
}

export async function postTeamUserMessage(sessionId: string, text: string): Promise<TeamSnapshot> {
  return invoke<TeamSnapshot>("post_team_user_message", { sessionId, text });
}

export async function pauseTeamSession(sessionId: string): Promise<TeamSnapshot> {
  return invoke<TeamSnapshot>("pause_team_session", { sessionId });
}

export async function resumeTeamSession(sessionId: string): Promise<TeamSnapshot> {
  return invoke<TeamSnapshot>("resume_team_session", { sessionId });
}

export async function cancelTeamSession(sessionId: string, summary?: string): Promise<TeamSnapshot> {
  return invoke<TeamSnapshot>("cancel_team_session", { sessionId, summary: summary ?? null });
}

export async function completeTeamSession(sessionId: string, summary: string): Promise<TeamSnapshot> {
  return invoke<TeamSnapshot>("complete_team_session", { sessionId, summary });
}

export async function deleteTeamSession(sessionId: string): Promise<void> {
  return invoke<void>("delete_team_session", { sessionId });
}
