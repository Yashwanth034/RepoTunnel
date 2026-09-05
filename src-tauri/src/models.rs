use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub(crate) enum WorkspaceAccessMode {
    ReadOnly,
    #[default]
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub(crate) enum WorkspaceChangePolicy {
    #[default]
    Review,
    Automatic,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub(crate) enum CommandPolicy {
    Disabled,
    #[default]
    Review,
    Automatic,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Workspace {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) added_at: u64,
    #[serde(default)]
    pub(crate) access_mode: WorkspaceAccessMode,
    #[serde(default)]
    pub(crate) change_policy: WorkspaceChangePolicy,
    #[serde(default)]
    pub(crate) command_policy: CommandPolicy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectMemory {
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) summary: String,
    pub(crate) goals: Vec<String>,
    pub(crate) decisions: Vec<String>,
    pub(crate) preferences: Vec<String>,
    pub(crate) next_steps: Vec<String>,
    pub(crate) updated_at: u64,
    #[serde(default)]
    pub(crate) git_head_at_update: Option<String>,
    #[serde(default)]
    pub(crate) activity_updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectSetupStatus {
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) project_kind: String,
    pub(crate) framework: String,
    pub(crate) package_manager: Option<String>,
    pub(crate) dependencies_ready: bool,
    pub(crate) setup_needed: bool,
    pub(crate) setup_command: Option<String>,
    pub(crate) dev_command: Option<String>,
    pub(crate) dev_url: Option<String>,
    pub(crate) detected_port: Option<u16>,
    pub(crate) notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectSetupOutcome {
    pub(crate) setup: ProjectSetupStatus,
    pub(crate) command: TerminalCommandRecord,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkspaceHealth {
    pub(crate) workspace_id: String,
    pub(crate) available: bool,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AiAccessStatus {
    pub(crate) paused: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckpointSummary {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) pinned: bool,
    pub(crate) created_at: u64,
    pub(crate) file_count: usize,
    pub(crate) total_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckpointComparison {
    pub(crate) checkpoint: CheckpointSummary,
    pub(crate) added_count: usize,
    pub(crate) modified_count: usize,
    pub(crate) deleted_count: usize,
    pub(crate) added: Vec<String>,
    pub(crate) modified: Vec<String>,
    pub(crate) deleted: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckpointRestoreResult {
    pub(crate) checkpoint: CheckpointSummary,
    pub(crate) pre_restore_checkpoint: CheckpointSummary,
    pub(crate) restored_files: usize,
    pub(crate) removed_files: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistorySettings {
    #[serde(default)]
    pub(crate) version_history_limit: Option<usize>,
    #[serde(default)]
    pub(crate) checkpoint_limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ActivityKind {
    Files,
    Terminal,
    Process,
    Launcher,
    Browser,
    Git,
    Monitoring,
    Verification,
    Team,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ActivityStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Rejected,
    Stopped,
    Observed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivityEvent {
    pub(crate) id: String,
    pub(crate) kind: ActivityKind,
    pub(crate) action: String,
    pub(crate) summary: String,
    pub(crate) detail: Option<String>,
    pub(crate) status: ActivityStatus,
    pub(crate) source_id: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivityGroup {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) trace_group_id: Option<String>,
    pub(crate) summary: String,
    #[serde(default)]
    pub(crate) version_ids: Vec<String>,
    pub(crate) events: Vec<ActivityEvent>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivityTimeline {
    pub(crate) groups: Vec<ActivityGroup>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryClearResult {
    pub(crate) removed_versions: usize,
    pub(crate) removed_changes: usize,
    pub(crate) removed_activities: usize,
    pub(crate) removed_operational_records: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckpointClearResult {
    pub(crate) removed_checkpoints: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SafetyScanCheck {
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) detail: String,
    pub(crate) items: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SafetyScanResult {
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) level: String,
    pub(crate) file_count: usize,
    pub(crate) ignored_entry_count: usize,
    pub(crate) pending_reviews: usize,
    pub(crate) checks: Vec<SafetyScanCheck>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GatewayStatus {
    pub(crate) running: bool,
    pub(crate) port: Option<u16>,
    pub(crate) workspace_count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicTunnelStatus {
    pub(crate) configured: bool,
    pub(crate) provider: String,
    pub(crate) provider_available: bool,
    pub(crate) cloudflared_available: bool,
    pub(crate) cloudflare_origin_port: u16,
    pub(crate) direct_https_port: u16,
    pub(crate) direct_http_challenge_port: u16,
    pub(crate) certbot_available: bool,
    pub(crate) certbot_version: Option<String>,
    pub(crate) tls_trusted: bool,
    pub(crate) public_reachable: bool,
    pub(crate) local_ready: bool,
    pub(crate) running: bool,
    pub(crate) ready: bool,
    pub(crate) public_url: Option<String>,
    pub(crate) mcp_url: Option<String>,
    pub(crate) auto_start: bool,
    pub(crate) request_count: u64,
    pub(crate) last_remote_request_at: Option<u64>,
    pub(crate) usage_label: String,
    pub(crate) usage_url: String,
    pub(crate) origin_port: Option<u16>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChatConnectionStatus {
    pub(crate) client_available: bool,
    pub(crate) client_version: Option<String>,
    pub(crate) running: bool,
    pub(crate) ready: bool,
    pub(crate) tunnel_id: Option<String>,
    pub(crate) admin_url: Option<String>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessCheck {
    pub(crate) allowed: bool,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DirectoryEntry {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) size: Option<u64>,
    pub(crate) modified_at: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileContent {
    pub(crate) path: String,
    pub(crate) content: String,
    pub(crate) size: u64,
    pub(crate) modified_at: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImagePreview {
    pub(crate) path: String,
    pub(crate) mime_type: String,
    pub(crate) size: u64,
    pub(crate) data_base64: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileInfo {
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) size: Option<u64>,
    pub(crate) modified_at: Option<u64>,
    pub(crate) readonly: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchMatch {
    pub(crate) path: String,
    pub(crate) line: usize,
    pub(crate) column: usize,
    pub(crate) preview: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LanguageStat {
    pub(crate) name: String,
    pub(crate) files: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectOverview {
    pub(crate) file_count: usize,
    pub(crate) directory_count: usize,
    pub(crate) text_file_count: usize,
    pub(crate) binary_file_count: usize,
    pub(crate) large_file_count: usize,
    pub(crate) ignored_entry_count: usize,
    pub(crate) ignored_entries: Vec<String>,
    pub(crate) total_bytes: u64,
    pub(crate) languages: Vec<LanguageStat>,
    pub(crate) manifests: Vec<String>,
    pub(crate) truncated: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectEntry {
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) size: Option<u64>,
    pub(crate) binary: bool,
    pub(crate) large: bool,
    pub(crate) language: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectSnapshot {
    pub(crate) overview: ProjectOverview,
    pub(crate) entries: Vec<ProjectEntry>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ChangeOperation {
    CreateFile,
    WriteFile,
    PatchFile,
    CreateDirectory,
    RenameEntry,
    MoveEntry,
    DeleteEntry,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ChangeStatus {
    Pending,
    Applied,
    Rejected,
    Undone,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VersionFileChange {
    pub(crate) operation: ChangeOperation,
    pub(crate) primary_path: String,
    pub(crate) secondary_path: Option<String>,
    pub(crate) summary: String,
    pub(crate) diff: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VersionRecord {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) parent_id: Option<String>,
    #[serde(default)]
    pub(crate) edit_group_id: Option<String>,
    pub(crate) before_snapshot_id: String,
    pub(crate) after_snapshot_id: String,
    pub(crate) summary: String,
    pub(crate) files: Vec<VersionFileChange>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VersionTimeline {
    pub(crate) records: Vec<VersionRecord>,
    pub(crate) current_version_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VersionRestoreResult {
    pub(crate) current_version_id: Option<String>,
    pub(crate) recovery_checkpoint_id: Option<String>,
    pub(crate) restored_files: usize,
    pub(crate) removed_files: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangeRecord {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) operation: ChangeOperation,
    pub(crate) primary_path: String,
    pub(crate) secondary_path: Option<String>,
    pub(crate) summary: String,
    pub(crate) diff: Option<String>,
    pub(crate) status: ChangeStatus,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) can_undo: bool,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangeOutcome {
    pub(crate) applied: bool,
    pub(crate) queued: bool,
    pub(crate) change: ChangeRecord,
    pub(crate) file: Option<FileInfo>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitFileChange {
    pub(crate) path: String,
    pub(crate) index_status: String,
    pub(crate) worktree_status: String,
    pub(crate) staged: bool,
    pub(crate) unstaged: bool,
    pub(crate) untracked: bool,
    pub(crate) conflicted: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitRepositoryStatus {
    pub(crate) available: bool,
    pub(crate) message: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) head: Option<String>,
    pub(crate) detached: bool,
    pub(crate) ahead: usize,
    pub(crate) behind: usize,
    pub(crate) staged_count: usize,
    pub(crate) unstaged_count: usize,
    pub(crate) untracked_count: usize,
    pub(crate) conflicted_count: usize,
    pub(crate) changes: Vec<GitFileChange>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitDiff {
    pub(crate) staged: bool,
    pub(crate) content: String,
    pub(crate) truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitCommitSummary {
    pub(crate) hash: String,
    pub(crate) short_hash: String,
    pub(crate) author: String,
    pub(crate) timestamp: u64,
    pub(crate) subject: String,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GitActionKind {
    Stage,
    Commit,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GitActionStatus {
    Pending,
    Applied,
    Rejected,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitActionRecord {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) kind: GitActionKind,
    pub(crate) summary: String,
    pub(crate) detail: Option<String>,
    pub(crate) status: GitActionStatus,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) commit_hash: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CommandStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Rejected,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandPreset {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) command: String,
    pub(crate) timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandRecord {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) preset_id: String,
    pub(crate) label: String,
    pub(crate) command: String,
    pub(crate) status: CommandStatus,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) output_truncated: bool,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandOutcome {
    pub(crate) queued: bool,
    pub(crate) command: CommandRecord,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecutionStatus {
    pub(crate) sandbox_available: bool,
    pub(crate) sandbox_version: Option<String>,
    pub(crate) message: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TerminalCommandStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Rejected,
    TimedOut,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalCommandRecord {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) status: TerminalCommandStatus,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) output_truncated: bool,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalCommandOutcome {
    pub(crate) queued: bool,
    pub(crate) command: TerminalCommandRecord,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ManagedProcessStatus {
    Pending,
    Running,
    Exited,
    Stopped,
    Failed,
    Rejected,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManagedProcessRecord {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) label: String,
    pub(crate) command: String,
    pub(crate) cwd: String,
    pub(crate) status: ManagedProcessStatus,
    pub(crate) pid: Option<u32>,
    pub(crate) created_at: u64,
    pub(crate) started_at: Option<u64>,
    pub(crate) updated_at: u64,
    pub(crate) exited_at: Option<u64>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) restart_count: u32,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManagedProcessOutcome {
    pub(crate) queued: bool,
    pub(crate) process: ManagedProcessRecord,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManagedProcessOutput {
    pub(crate) process_id: String,
    pub(crate) status: ManagedProcessStatus,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) stdout_offset: u64,
    pub(crate) stderr_offset: u64,
    pub(crate) next_stdout_offset: u64,
    pub(crate) next_stderr_offset: u64,
    pub(crate) stdout_has_more: bool,
    pub(crate) stderr_has_more: bool,
    pub(crate) output_truncated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LaunchActionKind {
    Url,
    WorkspacePath,
    Application,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LaunchActionStatus {
    Pending,
    Launched,
    Failed,
    Rejected,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LaunchApplication {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) category: String,
    pub(crate) executable: String,
    pub(crate) supports_urls: bool,
    pub(crate) supports_paths: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LaunchActionRecord {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) kind: LaunchActionKind,
    pub(crate) target: String,
    pub(crate) application_id: Option<String>,
    pub(crate) application_name: Option<String>,
    pub(crate) status: LaunchActionStatus,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) pid: Option<u32>,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LaunchActionOutcome {
    pub(crate) queued: bool,
    pub(crate) launch: LaunchActionRecord,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BrowserActionKind {
    Start,
    Stop,
    OpenTab,
    ActivateTab,
    CloseTab,
    Navigate,
    Click,
    Type,
    Scroll,
    Reload,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum BrowserActionStatus {
    Pending,
    Applied,
    Failed,
    Rejected,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserApplication {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) executable: String,
    pub(crate) node_executable: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserAutomationStatus {
    pub(crate) available: bool,
    pub(crate) running: bool,
    pub(crate) workspace_id: String,
    pub(crate) browser_id: Option<String>,
    pub(crate) browser_name: Option<String>,
    pub(crate) executable: Option<String>,
    pub(crate) pid: Option<u32>,
    pub(crate) debug_port: Option<u16>,
    pub(crate) started_at: Option<u64>,
    pub(crate) session_id: Option<String>,
    pub(crate) active_tab_id: Option<String>,
    pub(crate) message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserTab {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserActionRecord {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) kind: BrowserActionKind,
    pub(crate) target: String,
    pub(crate) detail: Option<String>,
    pub(crate) status: BrowserActionStatus,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserActionOutcome {
    pub(crate) queued: bool,
    pub(crate) action: BrowserActionRecord,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserPageInspection {
    pub(crate) tab_id: String,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) selector: Option<String>,
    pub(crate) found: bool,
    pub(crate) tag: Option<String>,
    pub(crate) text: String,
    pub(crate) html: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserVisualSelection {
    pub(crate) workspace_id: String,
    pub(crate) tab_id: String,
    pub(crate) url: String,
    pub(crate) selector: String,
    pub(crate) tag: String,
    pub(crate) text: String,
    pub(crate) html: String,
    pub(crate) selected_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserScreenshot {
    pub(crate) id: String,
    pub(crate) tab_id: String,
    pub(crate) created_at: u64,
    pub(crate) mime_type: String,
    pub(crate) data_base64: String,
    pub(crate) size_bytes: u64,
    pub(crate) full_page: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserConsoleEntry {
    pub(crate) tab_id: String,
    pub(crate) level: String,
    pub(crate) message: String,
    pub(crate) url: Option<String>,
    pub(crate) timestamp: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserNetworkFailure {
    pub(crate) tab_id: String,
    pub(crate) url: Option<String>,
    pub(crate) method: Option<String>,
    pub(crate) status: Option<u16>,
    pub(crate) error_text: String,
    pub(crate) resource_type: Option<String>,
    pub(crate) timestamp: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserDiagnostics {
    pub(crate) console_entries: Vec<BrowserConsoleEntry>,
    pub(crate) network_failures: Vec<BrowserNetworkFailure>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MonitoringFileChangeKind {
    Created,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitoringFileEvent {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) kind: MonitoringFileChangeKind,
    pub(crate) path: String,
    pub(crate) detected_at: u64,
    pub(crate) size: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitoringStatus {
    pub(crate) enabled: bool,
    pub(crate) running: bool,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) started_at: Option<u64>,
    pub(crate) last_scan_at: Option<u64>,
    pub(crate) scanned_file_count: usize,
    pub(crate) file_scan_truncated: bool,
    pub(crate) message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitoringPortListener {
    pub(crate) protocol: String,
    pub(crate) address: String,
    pub(crate) port: u16,
    pub(crate) pid: Option<u32>,
    pub(crate) process_name: Option<String>,
    pub(crate) managed_process_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitoringProcessSnapshot {
    pub(crate) process_id: String,
    pub(crate) label: String,
    pub(crate) command: String,
    pub(crate) status: ManagedProcessStatus,
    pub(crate) pid: Option<u32>,
    pub(crate) ports: Vec<u16>,
    pub(crate) stdout_tail: String,
    pub(crate) stderr_tail: String,
    pub(crate) output_truncated: bool,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitoringTerminalSnapshot {
    pub(crate) command_id: String,
    pub(crate) command: String,
    pub(crate) status: TerminalCommandStatus,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout_tail: String,
    pub(crate) stderr_tail: String,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitoringBrowserSnapshot {
    pub(crate) status: BrowserAutomationStatus,
    pub(crate) tabs: Vec<BrowserTab>,
    pub(crate) console_entries: Vec<BrowserConsoleEntry>,
    pub(crate) network_failures: Vec<BrowserNetworkFailure>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitoringSnapshot {
    pub(crate) status: MonitoringStatus,
    pub(crate) processes: Vec<MonitoringProcessSnapshot>,
    pub(crate) terminal: Vec<MonitoringTerminalSnapshot>,
    pub(crate) ports: Vec<MonitoringPortListener>,
    pub(crate) browser: MonitoringBrowserSnapshot,
    pub(crate) file_events: Vec<MonitoringFileEvent>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WorkflowCheckStatus {
    Pass,
    Warning,
    Blocked,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum WorkflowReadinessLevel {
    Ready,
    Limited,
    Blocked,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowCheck {
    pub(crate) key: String,
    pub(crate) title: String,
    pub(crate) status: WorkflowCheckStatus,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkflowReadiness {
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) level: WorkflowReadinessLevel,
    pub(crate) inspection_ready: bool,
    pub(crate) editing_ready: bool,
    pub(crate) testing_ready: bool,
    pub(crate) git_ready: bool,
    pub(crate) project_file_count: usize,
    pub(crate) command_preset_count: usize,
    pub(crate) git_branch: Option<String>,
    pub(crate) checks: Vec<WorkflowCheck>,
    pub(crate) next_step: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeDiagnostics {
    pub(crate) version: String,
    pub(crate) platform: String,
    pub(crate) architecture: String,
    pub(crate) data_directory: String,
    pub(crate) log_file: String,
    pub(crate) launch_at_login: bool,
    pub(crate) sandbox_available: bool,
    pub(crate) tunnel_client_available: bool,
    pub(crate) git_available: bool,
    pub(crate) warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TeamSessionStatus {
    Active,
    Paused,
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TeamPhase {
    Planning,
    Executing,
    Reviewing,
    Verifying,
    Complete,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TeamAgentStatus {
    Invited,
    Active,
    Idle,
    Offline,
    Done,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TeamTaskStatus {
    Todo,
    InProgress,
    Review,
    Blocked,
    Done,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TeamMessageKind {
    Plan,
    Progress,
    Question,
    Review,
    Decision,
    Handoff,
    System,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamCriterionCheck {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) verified: bool,
    pub(crate) evidence: Option<String>,
    pub(crate) verified_by_agent_id: Option<String>,
    pub(crate) verified_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamAgent {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) client_label: Option<String>,
    pub(crate) status: TeamAgentStatus,
    pub(crate) joined_at: Option<u64>,
    pub(crate) last_seen_at: Option<u64>,
    pub(crate) current_task_id: Option<String>,
    /// Internal AI-resume context. Persisted for MCP handoff continuity; the desktop UI does not surface it.
    #[serde(default)]
    pub(crate) resume_checkpoint: Option<String>,
}

fn default_team_cycle_number() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamCycleRecord {
    pub(crate) number: u32,
    pub(crate) request: String,
    pub(crate) completed_at: u64,
    pub(crate) summary: String,
    pub(crate) done_task_count: usize,
    pub(crate) verified_criterion_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamTask {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) status: TeamTaskStatus,
    pub(crate) priority: u8,
    pub(crate) owner_agent_id: Option<String>,
    pub(crate) reviewer_agent_id: Option<String>,
    #[serde(default)]
    pub(crate) contributor_agent_ids: Vec<String>,
    #[serde(default)]
    pub(crate) depends_on: Vec<String>,
    pub(crate) result: Option<String>,
    pub(crate) blocked_reason: Option<String>,
    pub(crate) created_by_agent_id: Option<String>,
    #[serde(default = "default_team_cycle_number")]
    pub(crate) cycle_number: u32,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamMessage {
    pub(crate) id: String,
    pub(crate) agent_id: Option<String>,
    pub(crate) agent_name: Option<String>,
    pub(crate) kind: TeamMessageKind,
    pub(crate) text: String,
    pub(crate) task_id: Option<String>,
    pub(crate) created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamLock {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) agent_id: String,
    pub(crate) task_id: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) expires_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamSession {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) goal: String,
    #[serde(default)]
    pub(crate) success_criteria: Vec<String>,
    #[serde(default)]
    pub(crate) criterion_checks: Vec<TeamCriterionCheck>,
    pub(crate) status: TeamSessionStatus,
    pub(crate) phase: TeamPhase,
    #[serde(default)]
    pub(crate) agents: Vec<TeamAgent>,
    #[serde(default)]
    pub(crate) tasks: Vec<TeamTask>,
    #[serde(default)]
    pub(crate) messages: Vec<TeamMessage>,
    #[serde(default)]
    pub(crate) locks: Vec<TeamLock>,
    #[serde(default)]
    pub(crate) revision: u64,
    #[serde(default = "default_team_cycle_number")]
    pub(crate) cycle_number: u32,
    #[serde(default)]
    pub(crate) current_request: Option<String>,
    #[serde(default)]
    pub(crate) completed_cycles: Vec<TeamCycleRecord>,
    #[serde(default)]
    pub(crate) persistent_team: bool,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) completed_at: Option<u64>,
    pub(crate) completion_summary: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamProgress {
    pub(crate) open_task_count: usize,
    pub(crate) done_task_count: usize,
    pub(crate) blocked_task_count: usize,
    pub(crate) verified_criterion_count: usize,
    pub(crate) total_criterion_count: usize,
    pub(crate) progress_percent: u8,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamSnapshot {
    pub(crate) session: TeamSession,
    pub(crate) progress: TeamProgress,
    pub(crate) recommended_action: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TeamSessionSummary {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) goal: String,
    pub(crate) status: TeamSessionStatus,
    pub(crate) phase: TeamPhase,
    pub(crate) agent_count: usize,
    pub(crate) joined_agent_count: usize,
    pub(crate) open_task_count: usize,
    pub(crate) done_task_count: usize,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}
