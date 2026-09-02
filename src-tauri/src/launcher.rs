use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use url::Url;

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{
        CommandPolicy, LaunchActionKind, LaunchActionOutcome, LaunchActionRecord,
        LaunchActionStatus, LaunchApplication, Workspace, WorkspaceChangePolicy,
    },
};

const LAUNCH_HISTORY_FILE: &str = "launch-history.json";
const MAX_HISTORY: usize = 250;
const MAX_URL_LENGTH: usize = 8 * 1024;
const MAX_TARGET_LENGTH: usize = 16 * 1024;

static LAUNCH_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static LAUNCH_STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum StoredLaunchRequest {
    Url {
        url: String,
        application_id: Option<String>,
    },
    WorkspacePath {
        relative_path: String,
        application_id: Option<String>,
    },
    Application {
        application_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredLaunchAction {
    record: LaunchActionRecord,
    request: StoredLaunchRequest,
}

#[derive(Clone, Copy)]
struct CatalogApplication {
    id: &'static str,
    name: &'static str,
    category: &'static str,
    executables: &'static [&'static str],
    supports_urls: bool,
    supports_paths: bool,
}

const APPLICATION_CATALOG: &[CatalogApplication] = &[
    CatalogApplication {
        id: "google-chrome",
        name: "Google Chrome",
        category: "browser",
        executables: &["google-chrome-stable", "google-chrome"],
        supports_urls: true,
        supports_paths: true,
    },
    CatalogApplication {
        id: "chromium",
        name: "Chromium",
        category: "browser",
        executables: &["chromium", "chromium-browser"],
        supports_urls: true,
        supports_paths: true,
    },
    CatalogApplication {
        id: "brave",
        name: "Brave",
        category: "browser",
        executables: &["brave-browser", "brave"],
        supports_urls: true,
        supports_paths: true,
    },
    CatalogApplication {
        id: "microsoft-edge",
        name: "Microsoft Edge",
        category: "browser",
        executables: &["microsoft-edge-stable", "microsoft-edge"],
        supports_urls: true,
        supports_paths: true,
    },
    CatalogApplication {
        id: "firefox",
        name: "Firefox",
        category: "browser",
        executables: &["firefox"],
        supports_urls: true,
        supports_paths: true,
    },
    CatalogApplication {
        id: "vscode",
        name: "Visual Studio Code",
        category: "editor",
        executables: &["code"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "vscodium",
        name: "VSCodium",
        category: "editor",
        executables: &["codium"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "cursor",
        name: "Cursor",
        category: "editor",
        executables: &["cursor"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "zed",
        name: "Zed",
        category: "editor",
        executables: &["zed"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "sublime-text",
        name: "Sublime Text",
        category: "editor",
        executables: &["subl", "sublime_text"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "gnome-text-editor",
        name: "GNOME Text Editor",
        category: "editor",
        executables: &["gnome-text-editor"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "microsoft-word",
        name: "Microsoft Word",
        category: "document",
        executables: &[
            "WINWORD.EXE",
            "WINWORD",
            "/Applications/Microsoft Word.app/Contents/MacOS/Microsoft Word",
        ],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "microsoft-excel",
        name: "Microsoft Excel",
        category: "spreadsheet",
        executables: &[
            "EXCEL.EXE",
            "EXCEL",
            "/Applications/Microsoft Excel.app/Contents/MacOS/Microsoft Excel",
        ],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "microsoft-powerpoint",
        name: "Microsoft PowerPoint",
        category: "presentation",
        executables: &[
            "POWERPNT.EXE",
            "POWERPNT",
            "/Applications/Microsoft PowerPoint.app/Contents/MacOS/Microsoft PowerPoint",
        ],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "libreoffice-writer",
        name: "LibreOffice Writer",
        category: "document",
        executables: &[
            "libreoffice",
            "soffice",
            "/usr/bin/libreoffice",
            "/snap/bin/libreoffice",
            "/Applications/LibreOffice.app/Contents/MacOS/soffice",
        ],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "libreoffice-calc",
        name: "LibreOffice Calc",
        category: "spreadsheet",
        executables: &[
            "libreoffice",
            "soffice",
            "/usr/bin/libreoffice",
            "/snap/bin/libreoffice",
            "/Applications/LibreOffice.app/Contents/MacOS/soffice",
        ],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "libreoffice-impress",
        name: "LibreOffice Impress",
        category: "presentation",
        executables: &[
            "libreoffice",
            "soffice",
            "/usr/bin/libreoffice",
            "/snap/bin/libreoffice",
            "/Applications/LibreOffice.app/Contents/MacOS/soffice",
        ],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "apple-pages",
        name: "Pages",
        category: "document",
        executables: &["/Applications/Pages.app/Contents/MacOS/Pages"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "apple-numbers",
        name: "Numbers",
        category: "spreadsheet",
        executables: &["/Applications/Numbers.app/Contents/MacOS/Numbers"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "apple-keynote",
        name: "Keynote",
        category: "presentation",
        executables: &["/Applications/Keynote.app/Contents/MacOS/Keynote"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "android-studio",
        name: "Android Studio",
        category: "development",
        executables: &[
            "studio",
            "android-studio",
            "/opt/android-studio/bin/studio",
            "/usr/local/android-studio/bin/studio",
            "/snap/bin/android-studio",
        ],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "unity",
        name: "Unity",
        category: "game engine",
        executables: &[
            "unity-editor",
            "unity",
            "Unity",
            "unityhub",
            "/snap/bin/unityhub",
        ],
        supports_urls: false,
        supports_paths: false,
    },
    CatalogApplication {
        id: "blender",
        name: "Blender",
        category: "3D",
        executables: &["blender", "/snap/bin/blender"],
        supports_urls: false,
        supports_paths: false,
    },
    CatalogApplication {
        id: "godot",
        name: "Godot",
        category: "game engine",
        executables: &["godot4", "godot", "godot3", "/snap/bin/godot"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "docker",
        name: "Docker",
        category: "development",
        executables: &["docker", "/usr/bin/docker", "/usr/local/bin/docker"],
        supports_urls: false,
        supports_paths: false,
    },
    CatalogApplication {
        id: "nautilus",
        name: "Files",
        category: "files",
        executables: &["nautilus"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "nemo",
        name: "Nemo",
        category: "files",
        executables: &["nemo"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "dolphin",
        name: "Dolphin",
        category: "files",
        executables: &["dolphin"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "thunar",
        name: "Thunar",
        category: "files",
        executables: &["thunar"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "pcmanfm",
        name: "PCManFM",
        category: "files",
        executables: &["pcmanfm"],
        supports_urls: false,
        supports_paths: true,
    },
    CatalogApplication {
        id: "gnome-terminal",
        name: "GNOME Terminal",
        category: "terminal",
        executables: &["gnome-terminal"],
        supports_urls: false,
        supports_paths: false,
    },
    CatalogApplication {
        id: "konsole",
        name: "Konsole",
        category: "terminal",
        executables: &["konsole"],
        supports_urls: false,
        supports_paths: false,
    },
    CatalogApplication {
        id: "kitty",
        name: "Kitty",
        category: "terminal",
        executables: &["kitty"],
        supports_urls: false,
        supports_paths: false,
    },
    CatalogApplication {
        id: "alacritty",
        name: "Alacritty",
        category: "terminal",
        executables: &["alacritty"],
        supports_urls: false,
        supports_paths: false,
    },
    CatalogApplication {
        id: "xfce-terminal",
        name: "Xfce Terminal",
        category: "terminal",
        executables: &["xfce4-terminal"],
        supports_urls: false,
        supports_paths: false,
    },
];

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn new_launch_id() -> String {
    format!(
        "launch-{:x}-{:x}",
        now_millis(),
        LAUNCH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn launch_history_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(LAUNCH_HISTORY_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel launch history: {error}"))
}

fn load_history_unlocked(app: &AppHandle) -> Result<Vec<StoredLaunchAction>, String> {
    let path = launch_history_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read launch history: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved launch history is invalid: {error}"))
}

fn save_history_unlocked(app: &AppHandle, records: &[StoredLaunchAction]) -> Result<(), String> {
    let path = launch_history_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel launch history directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create launch history directory: {error}"))?;
    let contents = serde_json::to_string_pretty(records)
        .map_err(|error| format!("Could not serialize launch history: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Could not save launch history: {error}"))
}

fn with_history<T>(
    app: &AppHandle,
    task: impl FnOnce(&mut Vec<StoredLaunchAction>) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = LAUNCH_STORE_LOCK
        .lock()
        .map_err(|_| "Launch history is unavailable.".to_string())?;
    let mut records = load_history_unlocked(app)?;
    let result = task(&mut records)?;
    records.sort_by_key(|entry| std::cmp::Reverse(entry.record.created_at));
    let mut completed_seen = 0usize;
    records.retain(|entry| {
        if entry.record.status == LaunchActionStatus::Pending {
            true
        } else if completed_seen < MAX_HISTORY {
            completed_seen = completed_seen.saturating_add(1);
            true
        } else {
            false
        }
    });
    save_history_unlocked(app, &records)?;
    Ok(result)
}

fn effective_command_policy(workspace: &Workspace) -> CommandPolicy {
    if workspace.change_policy == WorkspaceChangePolicy::Automatic {
        CommandPolicy::Automatic
    } else {
        workspace.command_policy
    }
}

#[cfg(windows)]
fn find_windows_office_executable(name: &str) -> Option<PathBuf> {
    let executable = name.to_ascii_uppercase();
    if !matches!(
        executable.as_str(),
        "WINWORD.EXE" | "WINWORD" | "EXCEL.EXE" | "EXCEL" | "POWERPNT.EXE" | "POWERPNT"
    ) {
        return None;
    }
    let executable = if executable.ends_with(".EXE") {
        executable
    } else {
        format!("{executable}.EXE")
    };
    let office_dirs = [
        "Microsoft Office/root/Office16",
        "Microsoft Office/Office16",
        "Microsoft Office/root/Office15",
        "Microsoft Office/Office15",
    ];
    for root_name in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        let Some(root) = env::var_os(root_name) else {
            continue;
        };
        for directory in office_dirs {
            let candidate = PathBuf::from(&root).join(directory).join(&executable);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_executable(name: &str) -> Option<PathBuf> {
    if name.contains('/') || name.contains('\\') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    if let Some(path_value) = env::var_os("PATH") {
        if let Some(candidate) = env::split_paths(&path_value)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
        {
            return Some(candidate);
        }
    }
    #[cfg(windows)]
    if let Some(candidate) = find_windows_office_executable(name) {
        return Some(candidate);
    }
    None
}

pub(crate) fn application_launch_args(application_id: &str) -> &'static [&'static str] {
    match application_id {
        "libreoffice-writer" => &["--writer"],
        "libreoffice-calc" => &["--calc"],
        "libreoffice-impress" => &["--impress"],
        _ => &[],
    }
}

fn application_args(application_id: &str, target: Option<String>) -> Vec<String> {
    let mut args = application_launch_args(application_id)
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    if let Some(target) = target {
        args.push(target);
    }
    args
}

fn catalog_entry(application_id: &str) -> Option<CatalogApplication> {
    APPLICATION_CATALOG
        .iter()
        .copied()
        .find(|application| application.id == application_id)
}

fn resolve_application(application_id: &str) -> Result<(CatalogApplication, PathBuf), String> {
    let application = catalog_entry(application_id).ok_or_else(|| {
        "That application is not in RepoTunnel's allowed launcher catalog.".to_string()
    })?;
    let executable = application
        .executables
        .iter()
        .find_map(|candidate| find_executable(candidate))
        .ok_or_else(|| {
            format!(
                "{} is not installed or is not available on PATH.",
                application.name
            )
        })?;
    Ok((application, executable))
}

pub(crate) fn list_applications() -> Vec<LaunchApplication> {
    APPLICATION_CATALOG
        .iter()
        .filter_map(|application| {
            let executable = application
                .executables
                .iter()
                .find_map(|candidate| find_executable(candidate))?;
            Some(LaunchApplication {
                id: application.id.to_string(),
                name: application.name.to_string(),
                category: application.category.to_string(),
                executable: executable.to_string_lossy().into_owned(),
                supports_urls: application.supports_urls,
                supports_paths: application.supports_paths,
            })
        })
        .collect()
}

fn default_opener() -> Result<(PathBuf, Vec<String>), String> {
    if let Some(path) = find_executable("xdg-open") {
        return Ok((path, Vec::new()));
    }
    if let Some(path) = find_executable("gio") {
        return Ok((path, vec!["open".to_string()]));
    }
    Err("No supported desktop opener was found (xdg-open or gio).".to_string())
}

fn spawn_detached(executable: &Path, args: &[String]) -> Result<u32, String> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not launch application: {error}"))?;
    let pid = child.id();
    let _ = thread::Builder::new()
        .name("repotunnel-launch-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(pid)
}

fn validate_url(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("URL cannot be empty.".to_string());
    }
    if trimmed.len() > MAX_URL_LENGTH {
        return Err(format!("URL cannot exceed {MAX_URL_LENGTH} bytes."));
    }
    let parsed =
        Url::parse(trimmed).map_err(|_| "Enter a valid http:// or https:// URL.".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(
            "RepoTunnel only opens http:// and https:// URLs through this launcher.".to_string(),
        );
    }
    if parsed.host_str().is_none() {
        return Err("The URL must include a host.".to_string());
    }
    Ok(parsed.to_string())
}

fn validate_relative_target(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.len() > MAX_TARGET_LENGTH {
        return Err(format!(
            "Workspace path cannot exceed {MAX_TARGET_LENGTH} bytes."
        ));
    }
    Ok(trimmed.to_string())
}

fn target_application_name(application_id: Option<&str>) -> Result<Option<String>, String> {
    match application_id {
        Some(id) => {
            let (application, _) = resolve_application(id)?;
            Ok(Some(application.name.to_string()))
        }
        None => Ok(None),
    }
}

fn pending_record(
    workspace: &Workspace,
    kind: LaunchActionKind,
    target: String,
    application_id: Option<String>,
    application_name: Option<String>,
) -> LaunchActionRecord {
    let now = now_millis();
    LaunchActionRecord {
        id: new_launch_id(),
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        kind,
        target,
        application_id,
        application_name,
        status: LaunchActionStatus::Pending,
        created_at: now,
        updated_at: now,
        pid: None,
        error: None,
    }
}

fn execute_request(workspace: &Workspace, request: &StoredLaunchRequest) -> Result<u32, String> {
    match request {
        StoredLaunchRequest::Url {
            url,
            application_id,
        } => {
            let url = validate_url(url)?;
            if let Some(application_id) = application_id {
                let (application, executable) = resolve_application(application_id)?;
                if !application.supports_urls {
                    return Err(format!(
                        "{} is not configured to open URLs through RepoTunnel.",
                        application.name
                    ));
                }
                spawn_detached(&executable, &[url])
            } else {
                let (executable, mut args) = default_opener()?;
                args.push(url);
                spawn_detached(&executable, &args)
            }
        }
        StoredLaunchRequest::WorkspacePath {
            relative_path,
            application_id,
        } => {
            let target =
                resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
            let target = target.to_string_lossy().into_owned();
            if let Some(application_id) = application_id {
                let (application, executable) = resolve_application(application_id)?;
                if !application.supports_paths {
                    return Err(format!(
                        "{} is not configured to open workspace paths through RepoTunnel.",
                        application.name
                    ));
                }
                let args = application_args(application_id, Some(target));
                spawn_detached(&executable, &args)
            } else {
                let (executable, mut args) = default_opener()?;
                args.push(target);
                spawn_detached(&executable, &args)
            }
        }
        StoredLaunchRequest::Application { application_id } => {
            let (_, executable) = resolve_application(application_id)?;
            let args = application_args(application_id, None);
            spawn_detached(&executable, &args)
        }
    }
}

fn execute_and_store(
    app: &AppHandle,
    workspace: &Workspace,
    stored: StoredLaunchAction,
) -> Result<LaunchActionRecord, String> {
    let result = execute_request(workspace, &stored.request);
    with_history(app, |records| {
        let current = records
            .iter_mut()
            .find(|entry| entry.record.id == stored.record.id)
            .ok_or_else(|| "Launch history changed while the action was running.".to_string())?;
        current.record.updated_at = now_millis();
        match result {
            Ok(pid) => {
                current.record.status = LaunchActionStatus::Launched;
                current.record.pid = Some(pid);
                current.record.error = None;
            }
            Err(error) => {
                current.record.status = LaunchActionStatus::Failed;
                current.record.pid = None;
                current.record.error = Some(error);
            }
        }
        Ok(current.record.clone())
    })
}

fn request_action(
    app: &AppHandle,
    workspace: &Workspace,
    record: LaunchActionRecord,
    request: StoredLaunchRequest,
) -> Result<LaunchActionOutcome, String> {
    let policy = effective_command_policy(workspace);
    if policy == CommandPolicy::Disabled {
        return Err("Application launching is disabled for this project because command execution is disabled.".to_string());
    }
    let stored = StoredLaunchAction {
        record: record.clone(),
        request,
    };
    with_history(app, |records| {
        records.push(stored.clone());
        Ok(())
    })?;
    if policy == CommandPolicy::Review {
        return Ok(LaunchActionOutcome {
            queued: true,
            launch: record,
        });
    }
    let launch = execute_and_store(app, workspace, stored)?;
    Ok(LaunchActionOutcome {
        queued: false,
        launch,
    })
}

pub(crate) fn request_open_url(
    app: &AppHandle,
    workspace: &Workspace,
    url: String,
    application_id: Option<String>,
) -> Result<LaunchActionOutcome, String> {
    let url = validate_url(&url)?;
    if let Some(application_id) = application_id.as_deref() {
        let (application, _) = resolve_application(application_id)?;
        if !application.supports_urls {
            return Err(format!(
                "{} is not configured to open URLs through RepoTunnel.",
                application.name
            ));
        }
    }
    let application_name = target_application_name(application_id.as_deref())?;
    let record = pending_record(
        workspace,
        LaunchActionKind::Url,
        url.clone(),
        application_id.clone(),
        application_name,
    );
    request_action(
        app,
        workspace,
        record,
        StoredLaunchRequest::Url {
            url,
            application_id,
        },
    )
}

pub(crate) fn request_open_workspace_path(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: String,
    application_id: Option<String>,
) -> Result<LaunchActionOutcome, String> {
    let relative_path = validate_relative_target(&relative_path)?;
    let target = resolve_workspace_path(workspace, &relative_path, AccessOperation::Read, true)?;
    if let Some(application_id) = application_id.as_deref() {
        let (application, _) = resolve_application(application_id)?;
        if !application.supports_paths {
            return Err(format!(
                "{} is not configured to open workspace paths through RepoTunnel.",
                application.name
            ));
        }
    }
    let application_name = target_application_name(application_id.as_deref())?;
    let display_target = if relative_path.is_empty() {
        "Project root".to_string()
    } else {
        relative_path.clone()
    };
    if !target.exists() {
        return Err("The requested workspace path does not exist.".to_string());
    }
    let record = pending_record(
        workspace,
        LaunchActionKind::WorkspacePath,
        display_target,
        application_id.clone(),
        application_name,
    );
    request_action(
        app,
        workspace,
        record,
        StoredLaunchRequest::WorkspacePath {
            relative_path,
            application_id,
        },
    )
}

pub(crate) fn request_launch_application(
    app: &AppHandle,
    workspace: &Workspace,
    application_id: String,
) -> Result<LaunchActionOutcome, String> {
    let (application, _) = resolve_application(&application_id)?;
    let record = pending_record(
        workspace,
        LaunchActionKind::Application,
        application.name.to_string(),
        Some(application_id.clone()),
        Some(application.name.to_string()),
    );
    request_action(
        app,
        workspace,
        record,
        StoredLaunchRequest::Application { application_id },
    )
}

pub(crate) fn clear_workspace_history(
    app: &AppHandle,
    workspace_id: &str,
) -> Result<usize, String> {
    with_history(app, |records| {
        let before = records.len();
        records.retain(|entry| {
            entry.record.workspace_id != workspace_id
                || entry.record.status == LaunchActionStatus::Pending
        });
        Ok(before.saturating_sub(records.len()))
    })
}

pub(crate) fn list_history(
    app: &AppHandle,
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<LaunchActionRecord>, String> {
    let _guard = LAUNCH_STORE_LOCK
        .lock()
        .map_err(|_| "Launch history is unavailable.".to_string())?;
    let mut records = load_history_unlocked(app)?
        .into_iter()
        .map(|entry| entry.record)
        .filter(|record| workspace_id.is_none_or(|id| record.workspace_id == id))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        let left_pending = left.status == LaunchActionStatus::Pending;
        let right_pending = right.status == LaunchActionStatus::Pending;
        right_pending
            .cmp(&left_pending)
            .then_with(|| right.created_at.cmp(&left.created_at))
    });
    records.truncate(limit.clamp(1, 100));
    Ok(records)
}

pub(crate) fn get_action(app: &AppHandle, launch_id: &str) -> Result<LaunchActionRecord, String> {
    let _guard = LAUNCH_STORE_LOCK
        .lock()
        .map_err(|_| "Launch history is unavailable.".to_string())?;
    load_history_unlocked(app)?
        .into_iter()
        .find(|entry| entry.record.id == launch_id)
        .map(|entry| entry.record)
        .ok_or_else(|| "That launch request no longer exists.".to_string())
}

pub(crate) fn approve_action(
    app: &AppHandle,
    workspace: &Workspace,
    launch_id: &str,
) -> Result<LaunchActionRecord, String> {
    let stored = {
        let _guard = LAUNCH_STORE_LOCK
            .lock()
            .map_err(|_| "Launch history is unavailable.".to_string())?;
        let records = load_history_unlocked(app)?;
        let stored = records
            .into_iter()
            .find(|entry| entry.record.id == launch_id)
            .ok_or_else(|| "That launch request no longer exists.".to_string())?;
        if stored.record.workspace_id != workspace.id {
            return Err("That launch request belongs to a different project.".to_string());
        }
        if stored.record.status != LaunchActionStatus::Pending {
            return Err("Only pending launch actions can be approved.".to_string());
        }
        stored
    };
    execute_and_store(app, workspace, stored)
}

pub(crate) fn reject_action(
    app: &AppHandle,
    launch_id: &str,
) -> Result<LaunchActionRecord, String> {
    with_history(app, |records| {
        let stored = records
            .iter_mut()
            .find(|entry| entry.record.id == launch_id)
            .ok_or_else(|| "That launch request no longer exists.".to_string())?;
        if stored.record.status != LaunchActionStatus::Pending {
            return Err("Only pending launch actions can be rejected.".to_string());
        }
        stored.record.status = LaunchActionStatus::Rejected;
        stored.record.updated_at = now_millis();
        Ok(stored.record.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::validate_url;

    #[test]
    fn launcher_accepts_web_urls() {
        assert!(validate_url("http://localhost:5173").is_ok());
        assert!(validate_url("https://example.com/path").is_ok());
    }

    #[test]
    fn launcher_rejects_non_web_schemes() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
    }
}
