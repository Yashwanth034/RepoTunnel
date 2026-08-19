use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    changes,
    models::{
        ChangeOutcome, GitActionKind, GitActionRecord, GitActionStatus, GitCommitSummary, GitDiff,
        GitFileChange, GitRepositoryStatus, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy,
    },
    secret_guard,
    storage::load_workspaces,
};

const ACTION_HISTORY_FILE: &str = "git-history.json";
const ACTION_REQUEST_DIRECTORY: &str = "git-requests";
const MAX_HISTORY: usize = 200;
const MAX_DIFF_BYTES: usize = 256 * 1024;
const MAX_COMMIT_MESSAGE_BYTES: usize = 5 * 1024;
static ACTION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum GitRequest {
    Stage {
        workspace_id: String,
        paths: Vec<String>,
        expected_fingerprint: String,
    },
    Commit {
        workspace_id: String,
        message: String,
        expected_head: Option<String>,
        staged_fingerprint: String,
    },
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn new_action_id() -> String {
    format!(
        "git-{:x}-{:x}",
        now_millis(),
        ACTION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn app_data_path(app: &AppHandle, relative: &str) -> Result<PathBuf, String> {
    app.path()
        .resolve(relative, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel Git storage: {error}"))
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel Git storage directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel Git storage directory: {error}"))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    ensure_parent(path)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("git-data");
    let temporary = path.with_file_name(format!(".{file_name}.{:x}.tmp", now_millis()));

    let result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("Could not create temporary Git data: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("Could not write Git data: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not flush Git data: {error}"))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not replace Git data safely: {error}"))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app_data_path(app, ACTION_HISTORY_FILE)
}

fn request_path(app: &AppHandle, id: &str) -> Result<PathBuf, String> {
    app_data_path(app, &format!("{ACTION_REQUEST_DIRECTORY}/{id}.json"))
}

fn load_history(app: &AppHandle) -> Result<Vec<GitActionRecord>, String> {
    let path = history_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read Git activity history: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved Git activity history is invalid: {error}"))
}

fn save_history(app: &AppHandle, history: &[GitActionRecord]) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(history)
        .map_err(|error| format!("Could not serialize Git activity history: {error}"))?;
    atomic_write(&history_path(app)?, &contents)
}

fn persist_record(app: &AppHandle, record: &GitActionRecord) -> Result<(), String> {
    let mut history = load_history(app)?;
    if let Some(existing) = history.iter_mut().find(|item| item.id == record.id) {
        *existing = record.clone();
    } else {
        history.push(record.clone());
    }
    history.sort_by_key(|record| std::cmp::Reverse(record.created_at));
    history.truncate(MAX_HISTORY);
    save_history(app, &history)
}

fn write_request(app: &AppHandle, id: &str, request: &GitRequest) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(request)
        .map_err(|error| format!("Could not serialize pending Git request: {error}"))?;
    atomic_write(&request_path(app, id)?, &bytes)
}

fn read_request(app: &AppHandle, id: &str) -> Result<GitRequest, String> {
    let path = request_path(app, id)?;
    let bytes =
        fs::read(&path).map_err(|error| format!("Could not read pending Git request: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("Saved Git request is invalid: {error}"))
}

fn remove_request(app: &AppHandle, id: &str) {
    if let Ok(path) = request_path(app, id) {
        let _ = fs::remove_file(path);
    }
}

fn git_binary() -> Result<PathBuf, String> {
    let path_value = env::var_os("PATH").unwrap_or_default();
    for directory in env::split_paths(&path_value) {
        let candidate = directory.join("git");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err("Git was not found in PATH.".to_string())
}

fn workspace_root(workspace: &Workspace) -> Result<PathBuf, String> {
    let root = Path::new(&workspace.path)
        .canonicalize()
        .map_err(|error| format!("Could not resolve the approved workspace: {error}"))?;
    let git_directory = root.join(".git");
    if !git_directory.is_dir() {
        return Err(
            "This workspace is not a supported Git repository. RepoTunnel currently requires the .git directory to live inside the approved workspace root."
                .to_string(),
        );
    }
    let canonical_git = git_directory
        .canonicalize()
        .map_err(|error| format!("Could not resolve this repository's .git directory: {error}"))?;
    if !canonical_git.starts_with(&root) {
        return Err(
            "This repository's Git metadata resolves outside the approved workspace.".to_string(),
        );
    }

    let binary = git_binary()?;
    let output = Command::new(&binary)
        .current_dir(&root)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("Could not validate the Git workspace boundary: {error}"))?;
    if !output.status.success() {
        return Err("The approved folder is not a valid Git worktree.".to_string());
    }
    let reported_root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
        .canonicalize()
        .map_err(|error| format!("Could not validate the Git worktree root: {error}"))?;
    if reported_root != root {
        return Err(
            "Git reports a worktree root outside the approved workspace boundary.".to_string(),
        );
    }

    let git_dir_output = Command::new(binary)
        .current_dir(&root)
        .args(["rev-parse", "--absolute-git-dir"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("Could not validate the Git metadata boundary: {error}"))?;
    if !git_dir_output.status.success() {
        return Err("Git metadata could not be validated.".to_string());
    }
    let reported_git = PathBuf::from(
        String::from_utf8_lossy(&git_dir_output.stdout)
            .trim()
            .to_string(),
    )
    .canonicalize()
    .map_err(|error| format!("Could not validate the Git metadata directory: {error}"))?;
    if !reported_git.starts_with(&root) {
        return Err("Git metadata resolves outside the approved workspace boundary.".to_string());
    }
    Ok(root)
}

fn base_git_command(workspace: &Workspace) -> Result<Command, String> {
    let root = workspace_root(workspace)?;
    let binary = git_binary()?;
    let mut command = Command::new(binary);
    command
        .current_dir(root)
        .arg("--no-pager")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("TERM", "dumb")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIFF_OPTS");
    Ok(command)
}

fn run_git(workspace: &Workspace, args: &[&str]) -> Result<Output, String> {
    let output = base_git_command(workspace)?
        .args(args)
        .output()
        .map_err(|error| format!("Could not start Git: {error}"))?;
    Ok(output)
}

fn head_hash(workspace: &Workspace) -> Result<Option<String>, String> {
    let output = run_git(workspace, &["rev-parse", "--verify", "HEAD"])?;
    if output.status.success() {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!value.is_empty()).then_some(value));
    }
    Ok(None)
}

fn branch_name(workspace: &Workspace) -> Result<(Option<String>, bool), String> {
    let output = run_git(workspace, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok(((!branch.is_empty()).then_some(branch), false));
    }
    Ok((None, head_hash(workspace)?.is_some()))
}

fn upstream_counts(workspace: &Workspace) -> (usize, usize) {
    let Ok(output) = run_git(
        workspace,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    ) else {
        return (0, 0);
    };
    if !output.status.success() {
        return (0, 0);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut values = text
        .split_whitespace()
        .filter_map(|value| value.parse::<usize>().ok());
    (values.next().unwrap_or(0), values.next().unwrap_or(0))
}

fn path_is_visible(workspace: &Workspace, relative_path: &str) -> bool {
    resolve_workspace_path(workspace, relative_path, AccessOperation::Read, false).is_ok()
}

fn parse_status(workspace: &Workspace) -> Result<Vec<GitFileChange>, String> {
    let output = run_git(
        workspace,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
    )?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "Git could not read repository status.".to_string()
        } else {
            format!("Git could not read repository status: {detail}")
        });
    }

    let parts = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut changes = Vec::new();
    let mut index = 0usize;
    while index < parts.len() {
        let item = parts[index];
        index += 1;
        if item.is_empty() || item.len() < 3 {
            continue;
        }
        let x = item[0] as char;
        let y = item[1] as char;
        let current_path = String::from_utf8_lossy(&item[3..]).into_owned();
        let mut display_path = current_path.clone();
        let mut visible = path_is_visible(workspace, &current_path);
        if (matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C'))
            && index < parts.len()
            && !parts[index].is_empty()
        {
            let old_path = String::from_utf8_lossy(parts[index]).into_owned();
            index += 1;
            visible = visible && path_is_visible(workspace, &old_path);
            display_path = format!("{old_path} → {current_path}");
        }
        if !visible {
            continue;
        }
        let conflicted = matches!(
            (x, y),
            ('D', 'D')
                | ('A', 'U')
                | ('U', 'D')
                | ('U', 'A')
                | ('D', 'U')
                | ('A', 'A')
                | ('U', 'U')
        );
        changes.push(GitFileChange {
            path: display_path,
            index_status: x.to_string(),
            worktree_status: y.to_string(),
            staged: x != ' ' && x != '?',
            unstaged: y != ' ' && y != '?',
            untracked: x == '?' && y == '?',
            conflicted,
        });
    }
    Ok(changes)
}

pub(crate) fn repository_status(workspace: &Workspace) -> GitRepositoryStatus {
    let root = match workspace_root(workspace) {
        Ok(root) => root,
        Err(message) => {
            return GitRepositoryStatus {
                available: false,
                message: Some(message),
                branch: None,
                head: None,
                detached: false,
                ahead: 0,
                behind: 0,
                staged_count: 0,
                unstaged_count: 0,
                untracked_count: 0,
                conflicted_count: 0,
                changes: Vec::new(),
            }
        }
    };
    if git_binary().is_err() {
        return GitRepositoryStatus {
            available: false,
            message: Some("Git was not found in PATH.".to_string()),
            branch: None,
            head: None,
            detached: false,
            ahead: 0,
            behind: 0,
            staged_count: 0,
            unstaged_count: 0,
            untracked_count: 0,
            conflicted_count: 0,
            changes: Vec::new(),
        };
    }
    let _ = root;

    match parse_status(workspace) {
        Ok(changes) => {
            let (branch, detached) = branch_name(workspace).unwrap_or((None, false));
            let head = head_hash(workspace).ok().flatten();
            let (ahead, behind) = upstream_counts(workspace);
            GitRepositoryStatus {
                available: true,
                message: Some(
                    "Git access is limited to this approved repository. Commits require local approval and use staged changes only."
                        .to_string(),
                ),
                branch,
                head,
                detached,
                ahead,
                behind,
                staged_count: changes.iter().filter(|change| change.staged).count(),
                unstaged_count: changes.iter().filter(|change| change.unstaged).count(),
                untracked_count: changes.iter().filter(|change| change.untracked).count(),
                conflicted_count: changes.iter().filter(|change| change.conflicted).count(),
                changes,
            }
        }
        Err(message) => GitRepositoryStatus {
            available: false,
            message: Some(message),
            branch: None,
            head: None,
            detached: false,
            ahead: 0,
            behind: 0,
            staged_count: 0,
            unstaged_count: 0,
            untracked_count: 0,
            conflicted_count: 0,
            changes: Vec::new(),
        },
    }
}

fn limit_text(mut bytes: Vec<u8>) -> (String, bool) {
    if bytes.len() <= MAX_DIFF_BYTES {
        return (String::from_utf8_lossy(&bytes).into_owned(), false);
    }
    bytes.truncate(MAX_DIFF_BYTES);
    (String::from_utf8_lossy(&bytes).into_owned(), true)
}

fn diff_paths(workspace: &Workspace, staged: bool) -> Result<(Vec<String>, usize), String> {
    let mut command = base_git_command(workspace)?;
    command.arg("diff");
    if staged {
        command.arg("--cached");
    }
    let output = command
        .args(["--name-status", "-z", "--", "."])
        .output()
        .map_err(|error| format!("Could not inspect Git diff paths: {error}"))?;
    if !output.status.success() {
        return Err("Git could not inspect changed paths.".to_string());
    }

    let fields = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut safe = Vec::new();
    let mut blocked = 0usize;
    let mut index = 0usize;
    while index < fields.len() {
        if fields[index].is_empty() {
            index += 1;
            continue;
        }
        let status = String::from_utf8_lossy(fields[index]).into_owned();
        index += 1;
        if index >= fields.len() || fields[index].is_empty() {
            break;
        }
        let first_path = String::from_utf8_lossy(fields[index]).into_owned();
        index += 1;
        let is_rename_or_copy = status.starts_with('R') || status.starts_with('C');
        let second_path = if is_rename_or_copy && index < fields.len() && !fields[index].is_empty()
        {
            let value = String::from_utf8_lossy(fields[index]).into_owned();
            index += 1;
            Some(value)
        } else {
            None
        };

        let visible = path_is_visible(workspace, &first_path)
            && second_path
                .as_deref()
                .is_none_or(|path| path_is_visible(workspace, path));
        if visible {
            safe.push(second_path.unwrap_or(first_path));
        } else {
            blocked += 1;
        }
    }
    Ok((safe, blocked))
}

pub(crate) fn diff(workspace: &Workspace, staged: bool) -> Result<GitDiff, String> {
    workspace_root(workspace)?;
    let (paths, _) = diff_paths(workspace, staged)?;
    if paths.is_empty() {
        return Ok(GitDiff {
            staged,
            content: String::new(),
            truncated: false,
        });
    }
    let mut command = base_git_command(workspace)?;
    command.arg("diff");
    if staged {
        command.arg("--cached");
    }
    command.args(["--no-ext-diff", "--no-textconv", "--unified=3", "--"]);
    for path in &paths {
        command.arg(path);
    }
    let output = command
        .output()
        .map_err(|error| format!("Could not start Git diff: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "Git could not produce the diff.".to_string()
        } else {
            format!("Git could not produce the diff: {detail}")
        });
    }
    let (content, truncated) = limit_text(output.stdout);
    Ok(GitDiff {
        staged,
        content: secret_guard::redact_text(&content),
        truncated,
    })
}

pub(crate) fn recent_commits(
    workspace: &Workspace,
    limit: usize,
) -> Result<Vec<GitCommitSummary>, String> {
    workspace_root(workspace)?;
    let limit = limit.clamp(1, 50).to_string();
    let output = run_git(
        workspace,
        &[
            "log",
            "-n",
            &limit,
            "--pretty=format:%H%x1f%h%x1f%an%x1f%ct%x1f%s%x1e",
        ],
    )?;
    if !output.status.success() {
        if head_hash(workspace)?.is_none() {
            return Ok(Vec::new());
        }
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "Git could not read commit history.".to_string()
        } else {
            format!("Git could not read commit history: {detail}")
        });
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut commits = Vec::new();
    for record in text.split('\x1e') {
        let record = record.trim_matches(|character| character == '\r' || character == '\n');
        if record.is_empty() {
            continue;
        }
        let fields = record.split('\x1f').collect::<Vec<_>>();
        if fields.len() < 5 {
            continue;
        }
        commits.push(GitCommitSummary {
            hash: fields[0].to_string(),
            short_hash: fields[1].to_string(),
            author: fields[2].to_string(),
            timestamp: fields[3].parse::<u64>().unwrap_or(0).saturating_mul(1000),
            subject: fields[4..].join(" "),
        });
    }
    Ok(commits)
}

fn scan_staged_secrets(workspace: &Workspace) -> Result<(), String> {
    let (paths, blocked) = diff_paths(workspace, true)?;
    if blocked > 0 {
        return Err("The staged set contains one or more RepoTunnel-protected paths.".to_string());
    }
    for path in paths {
        let spec = format!(":{path}");
        let output = run_git(workspace, &["show", &spec])?;
        if output.status.success() {
            secret_guard::scan_bytes(&path, &output.stdout)?;
        }
    }
    Ok(())
}

fn staged_fingerprint(workspace: &Workspace) -> Result<(String, GitDiff), String> {
    let (paths, blocked) = diff_paths(workspace, true)?;
    if blocked > 0 {
        return Err("The staged set contains one or more RepoTunnel-protected paths. Unstage those protected files before requesting a commit.".to_string());
    }
    if paths.is_empty() {
        return Err("There are no staged changes to commit. Stage the files you want included, then request the commit again.".to_string());
    }
    scan_staged_secrets(workspace)?;
    let staged = diff(workspace, true)?;
    let output = run_git(
        workspace,
        &["diff", "--cached", "--raw", "-z", "--no-abbrev", "--", "."],
    )?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err("Git could not fingerprint the staged changes.".to_string());
    }
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in &output.stdout {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Ok((format!("{hash:016x}:{}", output.stdout.len()), staged))
}

fn has_unmerged_paths(workspace: &Workspace) -> Result<bool, String> {
    let output = run_git(
        workspace,
        &["diff", "--name-only", "--diff-filter=U", "-z", "--", "."],
    )?;
    Ok(!output.stdout.is_empty())
}

fn validate_stage_path(workspace: &Workspace, relative_path: &str) -> Result<(), String> {
    if relative_path.trim().is_empty() || relative_path == "." || relative_path.contains(" → ") {
        return Err(
            "Git staging requires an exact file path relative to the workspace root.".to_string(),
        );
    }
    let resolved = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, false)?;
    if resolved.exists() {
        let metadata = fs::symlink_metadata(&resolved).map_err(|error| {
            format!("Could not inspect '{relative_path}' before staging: {error}")
        })?;
        if metadata.file_type().is_symlink() {
            return Err("RepoTunnel does not stage symlink entries.".to_string());
        }
        if metadata.is_dir() {
            return Err("Git staging requests must name files, not directories.".to_string());
        }
        secret_guard::scan_file(&resolved, relative_path)?;
    }

    let attribute = run_git(
        workspace,
        &["check-attr", "-z", "filter", "--", relative_path],
    )?;
    if !attribute.status.success() {
        return Err(format!(
            "Git could not inspect attributes for '{relative_path}'."
        ));
    }
    let fields = attribute
        .stdout
        .split(|byte| *byte == 0)
        .collect::<Vec<_>>();
    if fields.len() >= 3 {
        let value = String::from_utf8_lossy(fields[2]).trim().to_string();
        if value != "unspecified" && value != "unset" && !value.is_empty() {
            return Err(format!(
                "'{relative_path}' uses a Git clean filter. RepoTunnel refuses to stage filtered files because filters may execute external programs."
            ));
        }
    }
    Ok(())
}

fn stage_fingerprint(workspace: &Workspace, paths: &[String]) -> Result<String, String> {
    let mut sorted = paths.to_vec();
    sorted.sort();
    sorted.dedup();
    if sorted.is_empty() || sorted.len() > 100 {
        return Err("Git staging requires between 1 and 100 explicit file paths.".to_string());
    }

    let mut material = Vec::new();
    material.extend_from_slice(head_hash(workspace)?.unwrap_or_default().as_bytes());
    material.push(0);
    for path in &sorted {
        validate_stage_path(workspace, path)?;
        material.extend_from_slice(path.as_bytes());
        material.push(0);

        let index = run_git(workspace, &["ls-files", "-s", "--", path])?;
        material.extend_from_slice(&index.stdout);
        material.push(0);

        let resolved = resolve_workspace_path(workspace, path, AccessOperation::Read, false)?;
        if resolved.is_file() {
            let hash = run_git(workspace, &["hash-object", "--no-filters", "--", path])?;
            if !hash.status.success() {
                return Err(format!(
                    "Git could not fingerprint '{path}' before staging."
                ));
            }
            material.extend_from_slice(&hash.stdout);
        } else {
            material.extend_from_slice(b"<missing>");
        }
        material.push(0xff);
    }

    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in &material {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Ok(format!("{hash:016x}:{}", material.len()))
}

pub(crate) fn validate_ai_terminal_git_command(
    workspace: &Workspace,
    command: &str,
    user_requested_push: bool,
) -> Result<(), String> {
    let lower = command.to_ascii_lowercase();
    let normalized = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.contains("git add ")
        || normalized.ends_with("git add")
        || normalized.contains("git commit ")
        || normalized.ends_with("git commit")
    {
        return Err("Use RepoTunnel's dedicated Git stage/commit tools instead of raw git add/git commit. This keeps AI Auto behavior auditable and ensures the secret guard runs before Git history changes.".to_string());
    }
    if normalized.contains("git push") {
        if !user_requested_push {
            return Err("Git push is blocked until the user explicitly asks for the current work to be pushed. AI Auto removes approval popups; it does not grant standing permission to publish changes to a remote repository.".to_string());
        }
        preflight_ai_push(workspace)?;
    }
    Ok(())
}

pub(crate) fn preflight_ai_push(workspace: &Workspace) -> Result<(), String> {
    workspace_root(workspace)?;
    let names = run_git(workspace, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])?;
    if !names.status.success() {
        return Err("RepoTunnel could not inspect the committed project before push.".to_string());
    }
    let mut count = 0usize;
    for raw in names
        .stdout
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
    {
        count = count.saturating_add(1);
        if count > 5_000 {
            return Err("RepoTunnel secret guard refused to preflight more than 5,000 tracked files for one push.".to_string());
        }
        let path = String::from_utf8_lossy(raw).into_owned();
        let spec = format!("HEAD:{path}");
        let blob = run_git(workspace, &["show", &spec])?;
        if blob.status.success() && blob.stdout.len() <= 2 * 1024 * 1024 {
            secret_guard::scan_bytes(&path, &blob.stdout)?;
        }
    }
    Ok(())
}

pub(crate) fn request_stage(
    app: &AppHandle,
    workspace: &Workspace,
    mut paths: Vec<String>,
) -> Result<GitActionRecord, String> {
    if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
        return Err("This project is read-only. Git staging is disabled until read/write access is enabled.".to_string());
    }
    workspace_root(workspace)?;
    paths.sort();
    paths.dedup();
    let expected_fingerprint = stage_fingerprint(workspace, &paths)?;
    let id = new_action_id();
    let created_at = now_millis();
    let detail = paths.join("\n");
    let request = GitRequest::Stage {
        workspace_id: workspace.id.clone(),
        paths: paths.clone(),
        expected_fingerprint,
    };
    let record = GitActionRecord {
        id: id.clone(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        kind: GitActionKind::Stage,
        summary: if paths.len() == 1 {
            format!("Stage {}", paths[0])
        } else {
            format!("Stage {} files", paths.len())
        },
        detail: Some(detail),
        status: GitActionStatus::Pending,
        created_at,
        updated_at: created_at,
        commit_hash: None,
        error: None,
    };
    write_request(app, &id, &request)?;
    if let Err(error) = persist_record(app, &record) {
        remove_request(app, &id);
        return Err(error);
    }
    if workspace.change_policy == WorkspaceChangePolicy::Automatic {
        approve_action(app, &id)
    } else {
        Ok(record)
    }
}

pub(crate) fn request_commit(
    app: &AppHandle,
    workspace: &Workspace,
    message: String,
) -> Result<GitActionRecord, String> {
    if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
        return Err("This project is read-only. Git commits are disabled until read/write access is enabled.".to_string());
    }
    workspace_root(workspace)?;
    if has_unmerged_paths(workspace)? {
        return Err("Resolve all Git conflicts before requesting a commit.".to_string());
    }
    let message = message.trim().to_string();
    if message.is_empty() {
        return Err("Commit message cannot be empty.".to_string());
    }
    if message.len() > MAX_COMMIT_MESSAGE_BYTES {
        return Err("Commit message is too large.".to_string());
    }
    let (fingerprint, staged) = staged_fingerprint(workspace)?;
    let id = new_action_id();
    let created_at = now_millis();
    let request = GitRequest::Commit {
        workspace_id: workspace.id.clone(),
        message: message.clone(),
        expected_head: head_hash(workspace)?,
        staged_fingerprint: fingerprint,
    };
    let record = GitActionRecord {
        id: id.clone(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        kind: GitActionKind::Commit,
        summary: format!("Commit staged changes: {message}"),
        detail: Some(staged.content),
        status: GitActionStatus::Pending,
        created_at,
        updated_at: created_at,
        commit_hash: None,
        error: None,
    };
    write_request(app, &id, &request)?;
    if let Err(error) = persist_record(app, &record) {
        remove_request(app, &id);
        return Err(error);
    }
    if workspace.change_policy == WorkspaceChangePolicy::Automatic {
        approve_action(app, &id)
    } else {
        Ok(record)
    }
}

fn approved_workspace(app: &AppHandle, id: &str) -> Result<Workspace, String> {
    load_workspaces(app)?
        .into_iter()
        .find(|workspace| workspace.id == id)
        .ok_or_else(|| "That project is no longer approved in RepoTunnel.".to_string())
}

fn fail_action(
    app: &AppHandle,
    action_id: &str,
    mut record: GitActionRecord,
    message: String,
) -> Result<GitActionRecord, String> {
    record.status = GitActionStatus::Failed;
    record.updated_at = now_millis();
    record.error = Some(message.clone());
    persist_record(app, &record)?;
    remove_request(app, action_id);
    Err(message)
}

pub(crate) fn approve_action(app: &AppHandle, action_id: &str) -> Result<GitActionRecord, String> {
    let mut record = load_history(app)?
        .into_iter()
        .find(|record| record.id == action_id)
        .ok_or_else(|| "That Git request no longer exists.".to_string())?;
    if record.status != GitActionStatus::Pending {
        return Err("Only pending Git requests can be approved.".to_string());
    }
    let request = read_request(app, action_id)?;
    let workspace_id = match &request {
        GitRequest::Stage { workspace_id, .. } | GitRequest::Commit { workspace_id, .. } => {
            workspace_id
        }
    };
    let workspace = approved_workspace(app, workspace_id)?;
    if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
        return fail_action(
            app,
            action_id,
            record,
            "This project is now read-only, so the Git request cannot be applied.".to_string(),
        );
    }
    workspace_root(&workspace)?;

    match request {
        GitRequest::Stage {
            paths,
            expected_fingerprint,
            ..
        } => {
            if record.kind != GitActionKind::Stage {
                return fail_action(
                    app,
                    action_id,
                    record,
                    "The saved Git staging request does not match its history record.".to_string(),
                );
            }
            let current_fingerprint = stage_fingerprint(&workspace, &paths)?;
            if current_fingerprint != expected_fingerprint {
                return fail_action(
                    app,
                    action_id,
                    record,
                    "One or more files changed after this staging request was prepared. Request staging again so the current files can be reviewed.".to_string(),
                );
            }
            for path in &paths {
                validate_stage_path(&workspace, path)?;
            }
            let output = base_git_command(&workspace)?
                .arg("add")
                .arg("--all")
                .arg("--")
                .args(&paths)
                .output()
                .map_err(|error| {
                    format!("Could not start the approved Git staging action: {error}")
                })?;
            if !output.status.success() {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return fail_action(
                    app,
                    action_id,
                    record,
                    if detail.is_empty() {
                        "Git staging failed.".to_string()
                    } else {
                        format!("Git staging failed: {detail}")
                    },
                );
            }
            record.status = GitActionStatus::Applied;
            record.updated_at = now_millis();
            record.error = None;
            persist_record(app, &record)?;
            remove_request(app, action_id);
            Ok(record)
        }
        GitRequest::Commit {
            message,
            expected_head,
            staged_fingerprint: expected_fingerprint,
            ..
        } => {
            if record.kind != GitActionKind::Commit {
                return fail_action(
                    app,
                    action_id,
                    record,
                    "The saved Git commit request does not match its history record.".to_string(),
                );
            }
            let current_head = head_hash(&workspace)?;
            if current_head != expected_head {
                return fail_action(
                    app,
                    action_id,
                    record,
                    "HEAD changed after this commit was prepared. Request a fresh commit so the staged diff can be reviewed again.".to_string(),
                );
            }
            let (current_fingerprint, _) = staged_fingerprint(&workspace)?;
            if current_fingerprint != expected_fingerprint {
                return fail_action(
                    app,
                    action_id,
                    record,
                    "The staged changes changed after this commit was prepared. Request a fresh commit so the current staged diff can be reviewed.".to_string(),
                );
            }

            let output = base_git_command(&workspace)?
                .arg("-c")
                .arg("commit.gpgSign=false")
                .args(["commit", "--no-verify", "--no-gpg-sign", "-m", &message])
                .output()
                .map_err(|error| format!("Could not start the approved Git commit: {error}"))?;

            if !output.status.success() {
                let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return fail_action(
                    app,
                    action_id,
                    record,
                    if detail.is_empty() {
                        "Git commit failed. Check that this repository has a valid author identity and staged changes.".to_string()
                    } else {
                        format!("Git commit failed: {detail}")
                    },
                );
            }

            record.status = GitActionStatus::Applied;
            record.updated_at = now_millis();
            record.commit_hash = head_hash(&workspace)?;
            record.error = None;
            persist_record(app, &record)?;
            remove_request(app, action_id);
            Ok(record)
        }
    }
}

pub(crate) fn reject_action(app: &AppHandle, action_id: &str) -> Result<GitActionRecord, String> {
    let mut record = load_history(app)?
        .into_iter()
        .find(|record| record.id == action_id)
        .ok_or_else(|| "That Git request no longer exists.".to_string())?;
    if record.status != GitActionStatus::Pending {
        return Err("Only pending Git requests can be rejected.".to_string());
    }
    record.status = GitActionStatus::Rejected;
    record.updated_at = now_millis();
    record.error = None;
    persist_record(app, &record)?;
    remove_request(app, action_id);
    Ok(record)
}

pub(crate) fn clear_workspace_history(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<usize, String> {
    let mut history = load_history(app)?;
    let removed = history
        .iter()
        .filter(|record| {
            record.workspace_id == workspace_id && record.status != GitActionStatus::Pending
        })
        .map(|record| record.id.clone())
        .collect::<Vec<_>>();
    history.retain(|record| {
        record.workspace_id != workspace_id || record.status == GitActionStatus::Pending
    });
    save_history(app, &history)?;
    for id in &removed {
        if let Ok(path) = request_path(app, id) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(removed.len())
}

pub(crate) fn list_actions(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<GitActionRecord>, String> {
    let approved_ids = load_workspaces(app)?
        .into_iter()
        .map(|workspace| workspace.id)
        .collect::<std::collections::HashSet<_>>();
    let mut history = load_history(app)?
        .into_iter()
        .filter(|record| approved_ids.contains(&record.workspace_id))
        .filter(|record| workspace_id.is_none_or(|id| record.workspace_id == id))
        .collect::<Vec<_>>();
    history.sort_by(|left, right| {
        let left_pending = left.status == GitActionStatus::Pending;
        let right_pending = right.status == GitActionStatus::Pending;
        right_pending
            .cmp(&left_pending)
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    history.truncate(limit.clamp(1, 100));
    Ok(history)
}

pub(crate) fn request_restore_file(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: String,
    edit_group_id: Option<&str>,
) -> Result<ChangeOutcome, String> {
    if workspace.access_mode != WorkspaceAccessMode::ReadWrite {
        return Err("This project is read-only. Git restore is disabled until read/write access is enabled.".to_string());
    }
    workspace_root(workspace)?;
    let resolved = resolve_workspace_path(workspace, &relative_path, AccessOperation::Write, true)?;
    if !resolved.is_file() {
        return Err("Git restore currently supports tracked text files only.".to_string());
    }

    let tracked = run_git(
        workspace,
        &["ls-files", "--error-unmatch", "--", &relative_path],
    )?;
    if !tracked.status.success() {
        return Err("That path is not tracked by Git.".to_string());
    }
    let object = format!("HEAD:{relative_path}");
    let output = run_git(workspace, &["show", &object])?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "Git could not read the HEAD version of that file.".to_string()
        } else {
            format!("Git could not read the HEAD version of that file: {detail}")
        });
    }
    let content = String::from_utf8(output.stdout).map_err(|_| {
        "Git restore through RepoTunnel currently supports UTF-8 text files only.".to_string()
    })?;
    changes::write_file(app, workspace, relative_path, content, edit_group_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_message_limit_is_reasonable() {
        const { assert!(MAX_COMMIT_MESSAGE_BYTES >= 1024) };
    }

    #[test]
    fn git_action_ids_are_distinct() {
        assert_ne!(new_action_id(), new_action_id());
    }
}
