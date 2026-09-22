use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    desktop_control,
    models::Workspace,
    video,
};

const VIDEO_PROJECTS_DIR: &str = "video-projects";
const MANIFEST_FILE: &str = "video-project.json";
const STANDALONE_MARKER_FILE: &str = ".repotunnel-video-project";
const SCHEMA_VERSION: u32 = 1;
static PROJECT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static RECORDING_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static ACTIVE_RECORDING: OnceLock<Mutex<Option<ActiveRecording>>> = OnceLock::new();

const PROJECT_DIRECTORIES: &[&str] = &[
    "script",
    "storyboard",
    "recordings/raw",
    "recordings/selected",
    "animations/generated",
    "animations/source",
    "assets/images",
    "assets/video",
    "assets/audio",
    "narration",
    "subtitles",
    "timeline",
    "thumbnails",
    "renders/drafts",
    "renders/final",
    "qa",
    "licenses",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoProductionAsset {
    pub(crate) kind: String,
    pub(crate) relative_path: String,
    pub(crate) created_at: u64,
    #[serde(default)]
    pub(crate) label: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoProductionCheckpoint {
    pub(crate) stage: String,
    pub(crate) status: String,
    pub(crate) updated_at: u64,
    #[serde(default)]
    pub(crate) detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoProductionProject {
    pub(crate) schema_version: u32,
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) name: String,
    pub(crate) slug: String,
    pub(crate) relative_path: String,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) pinned: bool,
    pub(crate) aspect_ratio: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    #[serde(default)]
    pub(crate) script_path: Option<String>,
    #[serde(default)]
    pub(crate) storyboard_path: Option<String>,
    #[serde(default)]
    pub(crate) timeline_path: Option<String>,
    #[serde(default)]
    pub(crate) current_preview: Option<String>,
    #[serde(default)]
    pub(crate) latest_draft: Option<String>,
    #[serde(default)]
    pub(crate) final_export: Option<String>,
    #[serde(default)]
    pub(crate) current_subtitle: Option<String>,
    #[serde(default)]
    pub(crate) assets: Vec<VideoProductionAsset>,
    #[serde(default)]
    pub(crate) checkpoints: Vec<VideoProductionCheckpoint>,
    #[serde(default)]
    pub(crate) attention_required: bool,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoProductionDocument {
    pub(crate) project_id: String,
    pub(crate) document: String,
    pub(crate) relative_path: String,
    pub(crate) content: String,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoProjectFile {
    pub(crate) relative_path: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) size_bytes: u64,
    pub(crate) modified_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoRecordingStatus {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) project_id: String,
    pub(crate) status: String,
    pub(crate) capture_target: String,
    pub(crate) relative_path: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) started_at: u64,
    pub(crate) stopped_at: Option<u64>,
    pub(crate) message: String,
}

struct ActiveRecording {
    workspace: Workspace,
    status: VideoRecordingStatus,
    child: Child,
}

fn recording_state() -> &'static Mutex<Option<ActiveRecording>> {
    ACTIVE_RECORDING.get_or_init(|| Mutex::new(None))
}

fn now_millis() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_millis();
    Ok(u64::try_from(millis).unwrap_or(u64::MAX))
}

fn new_project_id() -> Result<String, String> {
    let timestamp = now_millis()?;
    let sequence = PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(format!("video-{timestamp:x}-{sequence:x}"))
}

fn slugify(name: &str) -> String {
    let mut output = String::new();
    let mut pending_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !output.is_empty() {
                output.push('-');
            }
            output.push(ch.to_ascii_lowercase());
            pending_dash = false;
        } else if ch.is_alphanumeric() {
            if pending_dash && !output.is_empty() {
                output.push('-');
            }
            for lower in ch.to_lowercase() {
                output.push(lower);
            }
            pending_dash = false;
        } else {
            pending_dash = true;
        }
        if output.chars().count() >= 64 {
            break;
        }
    }
    if output.is_empty() {
        "video-project".to_string()
    } else {
        output.trim_matches('-').to_string()
    }
}

fn validate_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Video project name cannot be empty.".to_string());
    }
    if trimmed.chars().count() > 120 {
        return Err("Video project name must be 120 characters or fewer.".to_string());
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err("Video project name contains unsupported control characters.".to_string());
    }
    Ok(trimmed.to_string())
}

fn validate_format(
    aspect_ratio: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
) -> Result<(String, u32, u32, u32), String> {
    let ratio = aspect_ratio.unwrap_or("16:9").trim();
    let default = match ratio {
        "16:9" => (1920, 1080),
        "9:16" => (1080, 1920),
        "1:1" => (1080, 1080),
        "4:5" => (1080, 1350),
        _ => {
            return Err("Video aspect ratio must be 16:9, 9:16, 1:1, or 4:5.".to_string());
        }
    };
    let width = width.unwrap_or(default.0);
    let height = height.unwrap_or(default.1);
    let fps = fps.unwrap_or(30);
    if !(320..=7680).contains(&width) || !(240..=4320).contains(&height) {
        return Err("Video dimensions are outside RepoTunnel's supported range.".to_string());
    }
    if !(12..=120).contains(&fps) {
        return Err("Video frame rate must be between 12 and 120 FPS.".to_string());
    }
    Ok((ratio.to_string(), width, height, fps))
}

fn projects_root(_workspace: &Workspace) -> Result<PathBuf, String> {
    #[cfg(test)]
    {
        Ok(PathBuf::from(&_workspace.path)
            .join(".repotunnel-video-test-home")
            .join("Projects"))
    }

    #[cfg(not(test))]
    {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| {
                "RepoTunnel could not resolve your home directory for Video Project creation."
                    .to_string()
            })?;
        Ok(home.join("Projects"))
    }
}

fn check_workspace_access(workspace: &Workspace, operation: AccessOperation) -> Result<(), String> {
    let _ = resolve_workspace_path(workspace, "", operation, true)?;
    Ok(())
}

fn validate_project_child(
    root: &Path,
    relative: &str,
    must_exist: bool,
) -> Result<PathBuf, String> {
    let mut candidate = root.to_path_buf();
    let normalized = relative.trim().replace('\\', "/");
    if normalized.starts_with('/') {
        return Err("Video Project path must stay inside its project folder.".to_string());
    }
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(part) => candidate.push(part),
            Component::CurDir => {}
            _ => {
                return Err("Video Project path must stay inside its project folder.".to_string());
            }
        }
        if candidate.exists() {
            let metadata = fs::symlink_metadata(&candidate)
                .map_err(|error| format!("Could not inspect Video Project path: {error}"))?;
            if metadata.file_type().is_symlink() {
                return Err("Video Project paths cannot traverse symbolic links.".to_string());
            }
        }
    }
    if must_exist && !candidate.exists() {
        return Err("Video Project path does not exist.".to_string());
    }
    Ok(candidate)
}

fn standalone_project_root(
    workspace: &Workspace,
    project: &VideoProductionProject,
) -> Result<PathBuf, String> {
    let root = projects_root(workspace)?;
    validate_project_child(&root, &project.slug, false)
}

fn standalone_identity_matches(root: &Path, project_id: &str) -> bool {
    let manifest = root.join(MANIFEST_FILE);
    let marker = root.join(STANDALONE_MARKER_FILE);
    let marker_matches = fs::symlink_metadata(&marker)
        .ok()
        .filter(|metadata| !metadata.file_type().is_symlink() && metadata.is_file())
        .and_then(|_| fs::read_to_string(&marker).ok())
        .is_some_and(|value| value.trim() == project_id);
    marker_matches
        && load_manifest(&manifest)
            .map(|project| project.id == project_id)
            .unwrap_or(false)
}

pub(crate) fn project_root(
    workspace: &Workspace,
    project: &VideoProductionProject,
    operation: AccessOperation,
) -> Result<PathBuf, String> {
    check_workspace_access(workspace, operation)?;

    let standalone = standalone_project_root(workspace, project)?;
    if standalone_identity_matches(&standalone, &project.id) {
        let metadata = fs::symlink_metadata(&standalone)
            .map_err(|error| format!("Could not inspect Video Project folder: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("Video Project path is not a regular directory.".to_string());
        }
        return Ok(standalone);
    }

    resolve_workspace_path(workspace, &project.relative_path, operation, true)
}

pub(crate) fn resolve_project_path(
    workspace: &Workspace,
    project: &VideoProductionProject,
    relative: &str,
    operation: AccessOperation,
    must_exist: bool,
) -> Result<PathBuf, String> {
    let normalized = relative.trim().replace('\\', "/");
    let prefix = format!("{}/", project.relative_path);
    let inside = if normalized == project.relative_path {
        ""
    } else {
        normalized
            .strip_prefix(&prefix)
            .ok_or_else(|| "Video Project path escaped its project root.".to_string())?
    };
    let root = project_root(workspace, project, operation)?;
    validate_project_child(&root, inside, must_exist)
}

fn manifest_path(project_root: &Path) -> PathBuf {
    project_root.join(MANIFEST_FILE)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve video project parent directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create video project directory: {error}"))?;
    let temporary = parent.join(format!(
        ".video-project-write-{}-{}.tmp",
        std::process::id(),
        PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| format!("Could not create temporary video project file: {error}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("Could not save video project data: {error}"))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("Could not finalize video project data: {error}"))
}

fn save_manifest(root: &Path, project: &VideoProductionProject) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(project)
        .map_err(|error| format!("Could not serialize video project: {error}"))?;
    write_atomic(&manifest_path(root), &bytes)
}

fn load_manifest(path: &Path) -> Result<VideoProductionProject, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read video project manifest: {error}"))?;
    let project: VideoProductionProject = serde_json::from_str(&contents)
        .map_err(|error| format!("Video project manifest is invalid: {error}"))?;
    if project.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "Unsupported video project schema version {}.",
            project.schema_version
        ));
    }
    Ok(project)
}

fn next_available_slug(root: &Path, preferred: &str) -> String {
    if !root.join(preferred).exists() {
        return preferred.to_string();
    }
    for suffix in 2..=9999 {
        let candidate = format!("{preferred}-{suffix}");
        if !root.join(&candidate).exists() {
            return candidate;
        }
    }
    format!(
        "{preferred}-{:x}",
        PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) fn create_project(
    workspace: &Workspace,
    name: &str,
    aspect_ratio: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
) -> Result<VideoProductionProject, String> {
    let name = validate_name(name)?;
    let (aspect_ratio, width, height, fps) = validate_format(aspect_ratio, width, height, fps)?;
    check_workspace_access(workspace, AccessOperation::Write)?;
    let root = projects_root(workspace)?;
    fs::create_dir_all(&root)
        .map_err(|error| format!("Could not create ~/Projects for Video Projects: {error}"))?;

    let slug = next_available_slug(&root, &slugify(&name));
    let relative_path = format!("{VIDEO_PROJECTS_DIR}/{slug}");
    let project_root = root.join(&slug);
    fs::create_dir(&project_root)
        .map_err(|error| format!("Could not create standalone Video Project: {error}"))?;

    let result = (|| {
        for directory in PROJECT_DIRECTORIES {
            fs::create_dir_all(project_root.join(directory))
                .map_err(|error| format!("Could not initialize video project folders: {error}"))?;
        }

        write_atomic(&project_root.join("script/script.md"), b"")?;
        write_atomic(
            &project_root.join("storyboard/storyboard.json"),
            br#"{
  "version": 1,
  "scenes": []
}
"#,
        )?;
        write_atomic(
            &project_root.join("timeline/timeline.json"),
            br#"{
  "version": 1,
  "tracks": []
}
"#,
        )?;

        let now = now_millis()?;
        let project = VideoProductionProject {
            schema_version: SCHEMA_VERSION,
            id: new_project_id()?,
            workspace_id: workspace.id.clone(),
            name,
            slug,
            relative_path: relative_path.clone(),
            status: "planning".to_string(),
            pinned: false,
            aspect_ratio,
            width,
            height,
            fps,
            created_at: now,
            updated_at: now,
            script_path: Some(format!("{relative_path}/script/script.md")),
            storyboard_path: Some(format!("{relative_path}/storyboard/storyboard.json")),
            timeline_path: Some(format!("{relative_path}/timeline/timeline.json")),
            current_preview: None,
            latest_draft: None,
            final_export: None,
            current_subtitle: None,
            assets: Vec::new(),
            checkpoints: vec![VideoProductionCheckpoint {
                stage: "project".to_string(),
                status: "completed".to_string(),
                updated_at: now,
                detail: Some("Video Project initialized.".to_string()),
            }],
            attention_required: false,
            last_error: None,
        };
        save_manifest(&project_root, &project)?;
        write_atomic(
            &project_root.join(STANDALONE_MARKER_FILE),
            project.id.as_bytes(),
        )?;
        Ok(project)
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&project_root);
    }
    result
}

fn collect_projects_from_root(
    root: &Path,
    workspace: &Workspace,
    standalone: bool,
    projects: &mut Vec<VideoProductionProject>,
) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    if !root.is_dir() {
        return Err("Video Projects root exists but is not a directory.".to_string());
    }

    for entry in fs::read_dir(root)
        .map_err(|error| format!("Could not list Video Projects: {error}"))?
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let manifest = path.join(MANIFEST_FILE);
        let Ok(manifest_metadata) = fs::symlink_metadata(&manifest) else {
            continue;
        };
        if manifest_metadata.file_type().is_symlink() || !manifest_metadata.is_file() {
            continue;
        }

        let project = match load_manifest(&manifest) {
            Ok(project) => project,
            Err(_) if standalone => continue,
            Err(error) => return Err(error),
        };
        if project.workspace_id != workspace.id {
            continue;
        }
        if standalone {
            let folder_name = path.file_name().and_then(|value| value.to_str());
            let expected_relative = format!("{VIDEO_PROJECTS_DIR}/{}", project.slug);
            if folder_name != Some(project.slug.as_str())
                || project.relative_path != expected_relative
                || !standalone_identity_matches(&path, &project.id)
            {
                continue;
            }
        }
        if !projects.iter().any(|existing| existing.id == project.id) {
            projects.push(project);
        }
    }
    Ok(())
}

pub(crate) fn list_projects(workspace: &Workspace) -> Result<Vec<VideoProductionProject>, String> {
    check_workspace_access(workspace, AccessOperation::Read)?;

    let mut projects = Vec::new();
    let standalone_root = projects_root(workspace)?;
    collect_projects_from_root(&standalone_root, workspace, true, &mut projects)?;

    let legacy_root =
        resolve_workspace_path(workspace, VIDEO_PROJECTS_DIR, AccessOperation::Read, false)?;
    collect_projects_from_root(&legacy_root, workspace, false, &mut projects)?;

    projects.sort_by(|left, right| {
        right
            .pinned
            .cmp(&left.pinned)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });
    Ok(projects)
}

pub(crate) fn get_project(
    workspace: &Workspace,
    project_id: &str,
) -> Result<VideoProductionProject, String> {
    list_projects(workspace)?
        .into_iter()
        .find(|project| project.id == project_id)
        .ok_or_else(|| "Video Project was not found in this approved workspace.".to_string())
}

pub(crate) fn set_project_pinned(
    workspace: &Workspace,
    project_id: &str,
    pinned: bool,
) -> Result<VideoProductionProject, String> {
    let mut project = get_project(workspace, project_id)?;
    let root = project_root(workspace, &project, AccessOperation::Write)?;
    project.pinned = pinned;
    project.updated_at = now_millis()?;
    save_manifest(&root, &project)?;
    Ok(project)
}

fn project_file_kind(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "mp4" | "m4v" | "webm" | "mov" | "mkv" | "avi" | "mpg" | "mpeg" | "wmv" | "3gp" | "ts"
        | "mts" | "m2ts" => "video",
        "wav" | "mp3" | "m4a" | "aac" | "flac" | "ogg" | "opus" | "wma" => "audio",
        "vtt" | "srt" | "ass" | "ssa" => "subtitle",
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" => "image",
        "md" | "txt" | "json" | "csv" | "yaml" | "yml" => "text",
        _ => "file",
    }
}

fn collect_project_files(
    root: &Path,
    directory: &Path,
    project_relative: &str,
    depth: usize,
    output: &mut Vec<VideoProjectFile>,
) -> Result<(), String> {
    if depth > 12 || output.len() >= 4000 {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("Could not list Video Project files: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("Could not inspect Video Project file: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Could not inspect Video Project file: {error}"))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_project_files(root, &path, project_relative, depth + 1, output)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let inside = path
            .strip_prefix(root)
            .map_err(|_| "Video Project file escaped its project directory.".to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        if inside == MANIFEST_FILE
            || inside == STANDALONE_MARKER_FILE
            || inside.contains("/.render-")
        {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("file")
            .to_string();
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        output.push(VideoProjectFile {
            relative_path: format!("{project_relative}/{inside}"),
            name,
            kind: project_file_kind(&path).to_string(),
            size_bytes: metadata.len(),
            modified_at,
        });
    }
    Ok(())
}

pub(crate) fn list_project_files(
    workspace: &Workspace,
    project_id: &str,
) -> Result<Vec<VideoProjectFile>, String> {
    let project = get_project(workspace, project_id)?;
    let root = project_root(workspace, &project, AccessOperation::Read)?;
    let mut files = Vec::new();
    collect_project_files(&root, &root, &project.relative_path, 0, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

pub(crate) fn read_project_text_file(
    workspace: &Workspace,
    project_id: &str,
    relative_path: &str,
) -> Result<String, String> {
    let project = get_project(workspace, project_id)?;
    let path = resolve_project_path(
        workspace,
        &project,
        relative_path,
        AccessOperation::Read,
        true,
    )?;
    if project_file_kind(&path) != "text" && project_file_kind(&path) != "subtitle" {
        return Err("This Video Project file is not a text-editable format.".to_string());
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect Video Project text file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 8 * 1024 * 1024
    {
        return Err(
            "Video Project text file is unavailable or exceeds the 8 MiB safety limit.".to_string(),
        );
    }
    fs::read_to_string(&path)
        .map_err(|error| format!("Could not read Video Project text file: {error}"))
}

const IMPORT_MAX_FILES: usize = 256;
const IMPORT_MAX_BYTES: u64 = 20 * 1024 * 1024 * 1024;

fn import_kind(path: &Path) -> Option<(&'static str, &'static str)> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "mp4" | "m4v" | "webm" | "mov" | "mkv" | "avi" | "mpg" | "mpeg" | "wmv" | "3gp" | "ts"
        | "mts" | "m2ts" => Some(("video", "assets/video")),
        "wav" | "mp3" | "m4a" | "aac" | "flac" | "ogg" | "opus" | "wma" => {
            Some(("audio", "assets/audio"))
        }
        "vtt" | "srt" | "ass" | "ssa" => Some(("subtitle", "subtitles")),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" => Some(("image", "assets/images")),
        _ => None,
    }
}

fn collect_import_files(
    directory: &Path,
    depth: usize,
    output: &mut Vec<(PathBuf, &'static str, &'static str, u64)>,
    total_bytes: &mut u64,
) -> Result<(), String> {
    if depth > 6 {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("Could not read selected media folder: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("Could not inspect selected media folder: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Could not inspect selected media item: {error}"))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_import_files(&path, depth + 1, output, total_bytes)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Some((kind, destination)) = import_kind(&path) else {
            continue;
        };
        if output.len() >= IMPORT_MAX_FILES {
            return Err(format!(
                "Selected folder contains more than {IMPORT_MAX_FILES} supported media files."
            ));
        }
        *total_bytes = total_bytes.saturating_add(metadata.len());
        if *total_bytes > IMPORT_MAX_BYTES {
            return Err(
                "Selected media exceeds RepoTunnel's 20 GiB import safety limit.".to_string(),
            );
        }
        output.push((path, kind, destination, metadata.len()));
    }
    Ok(())
}

fn safe_import_filename(path: &Path, fallback: &str) -> String {
    let original = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(fallback);
    let mut output = String::with_capacity(original.len());
    for ch in original.chars().take(180) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    let trimmed = output.trim().trim_matches('.').trim().to_string();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
    }
}

fn next_import_target(directory: &Path, filename: &str) -> PathBuf {
    let candidate = directory.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("media");
    let extension = path.extension().and_then(|value| value.to_str());
    for suffix in 2..=9999 {
        let next = match extension {
            Some(extension) => format!("{stem}-{suffix}.{extension}"),
            None => format!("{stem}-{suffix}"),
        };
        let candidate = directory.join(next);
        if !candidate.exists() {
            return candidate;
        }
    }
    directory.join(format!(
        "{stem}-{:x}{}",
        PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        extension
            .map(|value| format!(".{value}"))
            .unwrap_or_default()
    ))
}

fn srt_to_vtt(source: &str) -> String {
    let mut output = String::from("WEBVTT\n\n");
    for line in source.lines() {
        if line.contains("-->") {
            output.push_str(&line.replace(',', "."));
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }
    output
}

pub(crate) fn import_folder(
    workspace: &Workspace,
    folder_path: &str,
) -> Result<VideoProductionProject, String> {
    let source = PathBuf::from(folder_path);
    let metadata = fs::symlink_metadata(&source)
        .map_err(|error| format!("Could not inspect selected folder: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(
            "Select a regular folder containing video, audio, or subtitle files.".to_string(),
        );
    }
    let source = source
        .canonicalize()
        .map_err(|error| format!("Could not resolve selected folder: {error}"))?;

    let mut files = Vec::new();
    let mut total_bytes = 0_u64;
    collect_import_files(&source, 0, &mut files, &mut total_bytes)?;
    if files.is_empty() {
        return Err(
            "No supported video, audio, or subtitle files were found in the selected folder."
                .to_string(),
        );
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let name = source
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Imported Video Project");
    let mut project = create_project(workspace, name, None, None, None, None)?;
    let root = project_root(workspace, &project, AccessOperation::Write)?;

    let result = (|| {
        let mut preview_relative: Option<String> = None;
        let mut audio_preview_relative: Option<String> = None;
        let mut subtitle_relative: Option<String> = None;

        for (source_path, kind, destination, _) in files {
            let destination_dir = root.join(destination);
            fs::create_dir_all(&destination_dir)
                .map_err(|error| format!("Could not prepare imported media folder: {error}"))?;
            let fallback = match kind {
                "video" => "video.mp4",
                "audio" => "audio.wav",
                "subtitle" => "subtitles.vtt",
                "image" => "image.png",
                _ => "asset.bin",
            };
            let filename = safe_import_filename(&source_path, fallback);
            let target = next_import_target(&destination_dir, &filename);
            fs::copy(&source_path, &target).map_err(|error| {
                format!("Could not import '{}': {error}", source_path.display())
            })?;

            let relative_inside_project = target
                .strip_prefix(&root)
                .map_err(|_| "Imported media escaped the Video Project directory.".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            project = register_asset(
                workspace,
                &project.id,
                &format!("imported-{kind}"),
                &relative_inside_project,
                source_path.file_name().and_then(|value| value.to_str()),
            )?;

            let extension = target
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if kind == "video" && preview_relative.is_none() {
                preview_relative = Some(relative_inside_project.clone());
            }
            if kind == "audio" && audio_preview_relative.is_none() {
                audio_preview_relative = Some(relative_inside_project.clone());
            }
            if kind == "subtitle" && subtitle_relative.is_none() {
                if extension == "vtt" {
                    subtitle_relative = Some(relative_inside_project.clone());
                } else if extension == "srt" {
                    let source_text = fs::read_to_string(&target).map_err(|error| {
                        format!("Could not read imported SRT subtitles: {error}")
                    })?;
                    let vtt_target = next_import_target(&root.join("subtitles"), "imported.vtt");
                    write_atomic(&vtt_target, srt_to_vtt(&source_text).as_bytes())?;
                    let vtt_relative = vtt_target
                        .strip_prefix(&root)
                        .map_err(|_| {
                            "Imported subtitles escaped the Video Project directory.".to_string()
                        })?
                        .to_string_lossy()
                        .replace('\\', "/");
                    project = register_asset(
                        workspace,
                        &project.id,
                        "imported-subtitle",
                        &vtt_relative,
                        Some("Converted from imported SRT"),
                    )?;
                    subtitle_relative = Some(vtt_relative);
                } else if matches!(extension.as_str(), "ass" | "ssa") {
                    subtitle_relative = Some(relative_inside_project.clone());
                }
            }
        }

        let preferred_preview = preview_relative
            .as_deref()
            .or(audio_preview_relative.as_deref());
        project = set_render_outputs(
            workspace,
            &project.id,
            preferred_preview,
            None,
            None,
            subtitle_relative.as_deref(),
        )?;
        Ok(project)
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&root);
    }
    result
}

pub(crate) fn delete_project(workspace: &Workspace, project_id: &str) -> Result<(), String> {
    {
        let guard = recording_state()
            .lock()
            .map_err(|_| "Video recording state is unavailable.".to_string())?;
        if guard
            .as_ref()
            .is_some_and(|recording| recording.status.project_id == project_id)
        {
            return Err(
                "Stop the active recording before deleting this Video Project.".to_string(),
            );
        }
    }
    let project = get_project(workspace, project_id)?;
    let root = project_root(workspace, &project, AccessOperation::Write)?;
    let metadata = fs::symlink_metadata(&root)
        .map_err(|error| format!("Could not inspect Video Project before deletion: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Video Project path is not a regular directory.".to_string());
    }
    fs::remove_dir_all(&root).map_err(|error| format!("Could not delete Video Project: {error}"))
}

pub(crate) fn update_project_status(
    workspace: &Workspace,
    project_id: &str,
    status: &str,
    detail: Option<&str>,
) -> Result<VideoProductionProject, String> {
    let allowed = [
        "planning",
        "recording",
        "generating",
        "editing",
        "review",
        "completed",
        "failed",
    ];
    if !allowed.contains(&status) {
        return Err("Unsupported Video Project status.".to_string());
    }
    let mut project = get_project(workspace, project_id)?;
    let root = project_root(workspace, &project, AccessOperation::Write)?;
    let now = now_millis()?;
    project.status = status.to_string();
    project.updated_at = now;
    project.checkpoints.push(VideoProductionCheckpoint {
        stage: status.to_string(),
        status: if status == "failed" {
            "failed".to_string()
        } else {
            "in_progress".to_string()
        },
        updated_at: now,
        detail: detail.map(str::to_string),
    });
    project.attention_required = status == "failed";
    project.last_error =
        (status == "failed").then(|| detail.unwrap_or("Video Project failed.").to_string());
    save_manifest(&root, &project)?;
    Ok(project)
}

fn document_location(document: &str) -> Result<(&'static str, &'static str), String> {
    match document {
        "script" => Ok(("script/script.md", "script")),
        "storyboard" => Ok(("storyboard/storyboard.json", "storyboard")),
        "timeline" => Ok(("timeline/timeline.json", "timeline")),
        _ => Err("Video Project document must be script, storyboard, or timeline.".to_string()),
    }
}

pub(crate) fn write_document(
    workspace: &Workspace,
    project_id: &str,
    document: &str,
    content: &str,
) -> Result<VideoProductionDocument, String> {
    if content.len() > 8 * 1024 * 1024 {
        return Err("Video Project document exceeds the 8 MiB safety limit.".to_string());
    }
    let (relative_document, field) = document_location(document)?;
    let mut project = get_project(workspace, project_id)?;
    let root = project_root(workspace, &project, AccessOperation::Write)?;
    let target = root.join(relative_document);
    write_atomic(&target, content.as_bytes())?;

    let now = now_millis()?;
    let project_relative = format!("{}/{relative_document}", project.relative_path);
    match field {
        "script" => project.script_path = Some(project_relative.clone()),
        "storyboard" => project.storyboard_path = Some(project_relative.clone()),
        "timeline" => project.timeline_path = Some(project_relative.clone()),
        _ => {}
    }
    project.updated_at = now;
    if !project
        .checkpoints
        .iter()
        .any(|checkpoint| checkpoint.stage == field && checkpoint.status == "completed")
    {
        project.checkpoints.push(VideoProductionCheckpoint {
            stage: field.to_string(),
            status: "completed".to_string(),
            updated_at: now,
            detail: Some(format!("{field} saved.")),
        });
    }
    save_manifest(&root, &project)?;

    Ok(VideoProductionDocument {
        project_id: project.id,
        document: document.to_string(),
        relative_path: project_relative,
        content: content.to_string(),
        updated_at: now,
    })
}

pub(crate) fn read_document(
    workspace: &Workspace,
    project_id: &str,
    document: &str,
) -> Result<VideoProductionDocument, String> {
    let (relative_document, _) = document_location(document)?;
    let project = get_project(workspace, project_id)?;
    let root = project_root(workspace, &project, AccessOperation::Read)?;
    let target = root.join(relative_document);
    let content = fs::read_to_string(&target)
        .map_err(|error| format!("Could not read Video Project {document}: {error}"))?;
    let metadata = fs::metadata(&target)
        .map_err(|error| format!("Could not inspect Video Project {document}: {error}"))?;
    if metadata.len() > 8 * 1024 * 1024 {
        return Err("Video Project document exceeds the 8 MiB safety limit.".to_string());
    }
    let updated_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(project.updated_at);

    Ok(VideoProductionDocument {
        project_id: project.id,
        document: document.to_string(),
        relative_path: format!("{}/{relative_document}", project.relative_path),
        content,
        updated_at,
    })
}

pub(crate) fn register_asset(
    workspace: &Workspace,
    project_id: &str,
    kind: &str,
    project_relative_asset_path: &str,
    label: Option<&str>,
) -> Result<VideoProductionProject, String> {
    let mut project = get_project(workspace, project_id)?;
    let root = project_root(workspace, &project, AccessOperation::Write)?;
    let relative_to_workspace =
        format!("{}/{}", project.relative_path, project_relative_asset_path);
    let asset_path = resolve_project_path(
        workspace,
        &project,
        &relative_to_workspace,
        AccessOperation::Read,
        true,
    )?;
    if !asset_path.is_file() {
        return Err("Video Project asset must be a regular file.".to_string());
    }
    let now = now_millis()?;
    if !project
        .assets
        .iter()
        .any(|asset| asset.relative_path == relative_to_workspace)
    {
        project.assets.push(VideoProductionAsset {
            kind: kind.to_string(),
            relative_path: relative_to_workspace,
            created_at: now,
            label: label.map(str::to_string),
        });
    }
    project.updated_at = now;
    save_manifest(&root, &project)?;
    Ok(project)
}

pub(crate) fn set_render_outputs(
    workspace: &Workspace,
    project_id: &str,
    current_preview: Option<&str>,
    latest_draft: Option<&str>,
    final_export: Option<&str>,
    current_subtitle: Option<&str>,
) -> Result<VideoProductionProject, String> {
    let mut project = get_project(workspace, project_id)?;
    let root = project_root(workspace, &project, AccessOperation::Write)?;

    for value in [
        current_preview,
        latest_draft,
        final_export,
        current_subtitle,
    ]
    .into_iter()
    .flatten()
    {
        let relative = format!("{}/{}", project.relative_path, value);
        let path =
            resolve_project_path(workspace, &project, &relative, AccessOperation::Read, true)?;
        if !path.is_file() {
            return Err("Video render output must be a regular file.".to_string());
        }
    }

    project.current_preview =
        current_preview.map(|value| format!("{}/{}", project.relative_path, value));
    project.latest_draft = latest_draft.map(|value| format!("{}/{}", project.relative_path, value));
    project.final_export = final_export.map(|value| format!("{}/{}", project.relative_path, value));
    project.current_subtitle =
        current_subtitle.map(|value| format!("{}/{}", project.relative_path, value));
    project.updated_at = now_millis()?;
    if project.final_export.is_some() {
        project.status = "completed".to_string();
        project.checkpoints.push(VideoProductionCheckpoint {
            stage: "render".to_string(),
            status: "completed".to_string(),
            updated_at: project.updated_at,
            detail: Some("Final video export registered.".to_string()),
        });
    }
    save_manifest(&root, &project)?;
    Ok(project)
}

fn recording_fps(value: Option<u32>) -> Result<u32, String> {
    let fps = value.unwrap_or(30);
    if !(12..=60).contains(&fps) {
        return Err("Video recording frame rate must be between 12 and 60 FPS.".to_string());
    }
    Ok(fps)
}

fn recording_max_seconds(value: Option<u32>) -> Result<u32, String> {
    let seconds = value.unwrap_or(900);
    if !(1..=3600).contains(&seconds) {
        return Err("Video recording duration must be between 1 and 3600 seconds.".to_string());
    }
    Ok(seconds)
}

struct AiWorkspaceCapture<'a> {
    display: &'a str,
    xauth_path: &'a Path,
    width: u32,
    height: u32,
    fps: u32,
    max_seconds: u32,
    output: &'a Path,
}

#[cfg(target_os = "linux")]
fn build_ai_workspace_capture_command(
    ffmpeg: &Path,
    capture: &AiWorkspaceCapture<'_>,
) -> Result<Command, String> {
    if capture.display.trim().is_empty() {
        return Err("AI Workspace display is unavailable for recording.".to_string());
    }
    if !capture.xauth_path.is_file() {
        return Err("AI Workspace X11 authorization is unavailable for recording.".to_string());
    }
    let mut command = Command::new(ffmpeg);
    command
        .args([
            "-hide_banner",
            "-nostats",
            "-loglevel",
            "warning",
            "-y",
            "-f",
            "x11grab",
            "-draw_mouse",
            "1",
            "-framerate",
            &capture.fps.to_string(),
            "-video_size",
            &format!("{}x{}", capture.width, capture.height),
            "-i",
            capture.display,
            "-t",
            &capture.max_seconds.to_string(),
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
        ])
        .env("DISPLAY", capture.display)
        .env("XAUTHORITY", capture.xauth_path)
        .arg(capture.output);
    video::configure_background_command(&mut command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

#[cfg(not(target_os = "linux"))]
fn build_ai_workspace_capture_command(
    _ffmpeg: &Path,
    _capture: &AiWorkspaceCapture<'_>,
) -> Result<Command, String> {
    Err(
        "AI Workspace recording is currently available on Linux; Windows/macOS recording adapters are not yet validated."
            .to_string(),
    )
}

fn graceful_stop_recording(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"q\n");
        let _ = stdin.flush();
    }
    for _ in 0..50 {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(40));
    }
    video::terminate_child(child);
}

fn finish_recording(mut active: ActiveRecording, completion_message: &str) -> VideoRecordingStatus {
    graceful_stop_recording(&mut active.child);
    let now = now_millis().unwrap_or(active.status.started_at);
    active.status.stopped_at = Some(now);

    let project = match get_project(&active.workspace, &active.status.project_id) {
        Ok(project) => project,
        Err(error) => {
            active.status.status = "failed".to_string();
            active.status.message = error;
            return active.status;
        }
    };
    let file_path = resolve_project_path(
        &active.workspace,
        &project,
        &active.status.relative_path,
        AccessOperation::Read,
        true,
    );
    let usable = file_path
        .as_ref()
        .ok()
        .and_then(|path| fs::metadata(path).ok())
        .is_some_and(|metadata| metadata.is_file() && metadata.len() > 0);

    if !usable {
        active.status.status = "failed".to_string();
        active.status.message =
            "Recording stopped but no usable video file was produced.".to_string();
        let _ = update_project_status(
            &active.workspace,
            &active.status.project_id,
            "failed",
            Some(&active.status.message),
        );
        return active.status;
    }
    let prefix = format!("{}/", project.relative_path);
    let Some(project_asset_path) = active.status.relative_path.strip_prefix(&prefix) else {
        active.status.status = "failed".to_string();
        active.status.message = "Recorded video path is outside its Video Project.".to_string();
        return active.status;
    };
    if let Err(error) = register_asset(
        &active.workspace,
        &active.status.project_id,
        "recording",
        project_asset_path,
        Some("AI Workspace screen recording"),
    ) {
        active.status.status = "failed".to_string();
        active.status.message = error;
        return active.status;
    }
    let _ = update_project_status(
        &active.workspace,
        &active.status.project_id,
        "editing",
        Some(completion_message),
    );
    active.status.status = "completed".to_string();
    active.status.message = completion_message.to_string();
    active.status
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn start_ai_workspace_recording(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    display: &str,
    xauth_path: &Path,
    width: u32,
    height: u32,
    fps: Option<u32>,
    max_seconds: Option<u32>,
) -> Result<VideoRecordingStatus, String> {
    if !desktop_control::is_enabled(app, &workspace.id)? {
        return Err(
            "Video recording requires Desktop Control. Enable it locally in Commands → Applications & links."
                .to_string(),
        );
    }
    let project = get_project(workspace, project_id)?;
    let _ = project_root(workspace, &project, AccessOperation::Write)?;
    if width == 0 || height == 0 || width > 7680 || height > 4320 {
        return Err("AI Workspace recording dimensions are invalid.".to_string());
    }
    let fps = recording_fps(fps)?;
    let max_seconds = recording_max_seconds(max_seconds)?;

    {
        let guard = recording_state()
            .lock()
            .map_err(|_| "Video recording state is unavailable.".to_string())?;
        if let Some(active) = guard.as_ref() {
            return Err(format!(
                "Another RepoTunnel video recording is already active for Video Project {}.",
                active.status.project_id
            ));
        }
    }

    let ffmpeg = video::ensure_ffmpeg_program(app)?;
    let started_at = now_millis()?;
    let sequence = RECORDING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let asset_path = format!("recordings/raw/recording-{started_at}-{sequence}.mkv");
    let relative_path = format!("{}/{}", project.relative_path, asset_path);
    let output = resolve_project_path(
        workspace,
        &project,
        &relative_path,
        AccessOperation::Write,
        false,
    )?;
    if output.exists() {
        return Err("Video recording output path already exists.".to_string());
    }

    let capture = AiWorkspaceCapture {
        display,
        xauth_path,
        width,
        height,
        fps,
        max_seconds,
        output: &output,
    };
    let mut command = build_ai_workspace_capture_command(&ffmpeg, &capture)?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start background screen recording: {error}"))?;
    thread::sleep(Duration::from_millis(220));
    if let Some(exit) = child
        .try_wait()
        .map_err(|error| format!("Could not verify screen recording startup: {error}"))?
    {
        let _ = fs::remove_file(&output);
        return Err(format!(
            "FFmpeg screen recording exited immediately with status {exit}."
        ));
    }

    let status = VideoRecordingStatus {
        id: format!("recording-{started_at:x}-{sequence:x}"),
        workspace_id: workspace.id.clone(),
        project_id: project.id.clone(),
        status: "recording".to_string(),
        capture_target: "aiWorkspace".to_string(),
        relative_path,
        width,
        height,
        fps,
        started_at,
        stopped_at: None,
        message: format!(
            "Recording isolated AI Workspace at {width}×{height}, {fps} FPS. Camera and microphone are not captured."
        ),
    };

    update_project_status(
        workspace,
        &project.id,
        "recording",
        Some("AI Workspace recording started."),
    )?;

    let mut guard = recording_state()
        .lock()
        .map_err(|_| "Video recording state is unavailable.".to_string())?;
    *guard = Some(ActiveRecording {
        workspace: workspace.clone(),
        status: status.clone(),
        child,
    });
    Ok(status)
}

pub(crate) fn get_recording_status(
    workspace_id: &str,
    project_id: Option<&str>,
) -> Result<Option<VideoRecordingStatus>, String> {
    let finished = {
        let mut guard = recording_state()
            .lock()
            .map_err(|_| "Video recording state is unavailable.".to_string())?;
        let Some(active) = guard.as_mut() else {
            return Ok(None);
        };
        if active.status.workspace_id != workspace_id {
            return Ok(None);
        }
        if project_id.is_some_and(|id| active.status.project_id != id) {
            return Ok(None);
        }
        if active
            .child
            .try_wait()
            .map_err(|error| format!("Could not inspect screen recording: {error}"))?
            .is_some()
        {
            guard.take()
        } else {
            return Ok(Some(active.status.clone()));
        }
    };

    Ok(finished.map(|active| {
        finish_recording(
            active,
            "Recording reached its configured duration and was saved.",
        )
    }))
}

pub(crate) fn stop_recording(
    workspace_id: &str,
    project_id: &str,
) -> Result<VideoRecordingStatus, String> {
    let active = {
        let mut guard = recording_state()
            .lock()
            .map_err(|_| "Video recording state is unavailable.".to_string())?;
        let active = guard
            .as_ref()
            .ok_or_else(|| "No RepoTunnel video recording is active.".to_string())?;
        if active.status.workspace_id != workspace_id || active.status.project_id != project_id {
            return Err("The active recording belongs to a different Video Project.".to_string());
        }
        guard
            .take()
            .ok_or_else(|| "No RepoTunnel video recording is active.".to_string())?
    };
    Ok(finish_recording(
        active,
        "Recording stopped and saved to the Video Project.",
    ))
}

pub(crate) fn stop_all_activity() {
    let active = recording_state()
        .lock()
        .ok()
        .and_then(|mut guard| guard.take());
    if let Some(active) = active {
        let _ = finish_recording(
            active,
            "Recording stopped because RepoTunnel paused or exited.",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use crate::{
        access::AccessOperation,
        models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
    };

    use super::{
        build_ai_workspace_capture_command, create_project, delete_project, import_folder,
        list_project_files, list_projects, project_root as resolve_project_root, read_document,
        set_project_pinned, write_document, AiWorkspaceCapture, PROJECT_DIRECTORIES,
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_workspace(read_only: bool) -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repotunnel-video-production-{}-{nonce}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            id: format!("test-{counter}"),
            name: "Video test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: if read_only {
                WorkspaceAccessMode::ReadOnly
            } else {
                WorkspaceAccessMode::ReadWrite
            },
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        (root, workspace)
    }

    #[test]
    fn creates_durable_project_tree_and_manifest() {
        let (root, workspace) = temp_workspace(false);
        let project = create_project(&workspace, "MCP Explained", None, None, None, None).unwrap();

        assert_eq!(project.name, "MCP Explained");
        assert_eq!(project.slug, "mcp-explained");
        assert_eq!(project.aspect_ratio, "16:9");
        assert_eq!(project.width, 1920);
        assert_eq!(project.height, 1080);
        assert_eq!(project.fps, 30);

        let project_root =
            resolve_project_root(&workspace, &project, AccessOperation::Read).unwrap();
        assert_eq!(
            project_root,
            root.join(".repotunnel-video-test-home")
                .join("Projects")
                .join("mcp-explained")
        );
        assert!(!root.join(&project.relative_path).exists());
        assert!(project_root.join("video-project.json").is_file());
        for directory in PROJECT_DIRECTORIES {
            assert!(project_root.join(directory).is_dir(), "{directory} missing");
        }

        let listed = list_projects(&workspace).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, project.id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn supports_neutral_four_by_five_projects() {
        let (root, workspace) = temp_workspace(false);
        let project =
            create_project(&workspace, "Portrait Feed", Some("4:5"), None, None, None).unwrap();
        assert_eq!(project.aspect_ratio, "4:5");
        assert_eq!((project.width, project.height), (1080, 1350));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pinned_projects_sort_first_and_project_files_are_listed() {
        let (root, workspace) = temp_workspace(false);
        let first = create_project(&workspace, "First", None, None, None, None).unwrap();
        let second = create_project(&workspace, "Second", None, None, None, None).unwrap();
        set_project_pinned(&workspace, &first.id, true).unwrap();

        let listed = list_projects(&workspace).unwrap();
        assert_eq!(
            listed.first().map(|item| item.id.as_str()),
            Some(first.id.as_str())
        );
        assert!(listed[0].pinned);
        assert!(listed.iter().any(|item| item.id == second.id));

        let files = list_project_files(&workspace, &first.id).unwrap();
        assert!(files
            .iter()
            .any(|file| file.relative_path.ends_with("script/script.md")));
        assert!(files
            .iter()
            .any(|file| file.relative_path.ends_with("storyboard/storyboard.json")));
        assert!(files
            .iter()
            .any(|file| file.relative_path.ends_with("timeline/timeline.json")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_names_get_distinct_project_folders() {
        let (root, workspace) = temp_workspace(false);
        let first = create_project(&workspace, "Install Docker", None, None, None, None).unwrap();
        let second = create_project(&workspace, "Install Docker", None, None, None, None).unwrap();
        assert_eq!(first.slug, "install-docker");
        assert_eq!(second.slug, "install-docker-2");
        assert_ne!(first.id, second.id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn normal_projects_with_unrelated_video_manifest_do_not_break_listing() {
        let (root, workspace) = temp_workspace(false);
        let project = create_project(&workspace, "Real Video", None, None, None, None).unwrap();
        let normal = root
            .join(".repotunnel-video-test-home")
            .join("Projects")
            .join("ordinary-project");
        fs::create_dir_all(&normal).unwrap();
        fs::write(normal.join("video-project.json"), b"{ not our manifest }").unwrap();

        let listed = list_projects(&workspace).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, project.id);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn standalone_project_slug_cannot_escape_projects_root() {
        let (root, workspace) = temp_workspace(false);
        let mut project = create_project(&workspace, "Safe Video", None, None, None, None).unwrap();
        project.slug = "../outside".to_string();
        assert!(resolve_project_root(&workspace, &project, AccessOperation::Write).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn script_storyboard_and_timeline_are_persistent() {
        let (root, workspace) = temp_workspace(false);
        let project = create_project(&workspace, "Persistence", None, None, None, None).unwrap();

        write_document(&workspace, &project.id, "script", "# Hello\nNarration").unwrap();
        write_document(
            &workspace,
            &project.id,
            "storyboard",
            r#"{"scenes":[{"id":"scene-1","visual":"diagram"}]}"#,
        )
        .unwrap();
        write_document(&workspace, &project.id, "timeline", r#"{"tracks":[]}"#).unwrap();

        assert_eq!(
            read_document(&workspace, &project.id, "script")
                .unwrap()
                .content,
            "# Hello\nNarration"
        );
        let reloaded = list_projects(&workspace).unwrap().remove(0);
        assert!(reloaded.script_path.is_some());
        assert!(reloaded.storyboard_path.is_some());
        assert!(reloaded.timeline_path.is_some());
        fs::remove_dir_all(root).unwrap();
    }

    fn program_on_path(name: &str) -> Option<PathBuf> {
        env::var_os("PATH")
            .into_iter()
            .flat_map(|value| env::split_paths(&value).collect::<Vec<_>>())
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
    }

    #[test]
    fn read_only_workspace_cannot_create_video_project() {
        let (root, workspace) = temp_workspace(true);
        assert!(create_project(&workspace, "Blocked", None, None, None, None).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imports_audio_only_folder_as_preview() {
        let (root, workspace) = temp_workspace(false);
        let source = root.join("audio-project");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("narration.mp3"), b"fake-mp3-for-import-test").unwrap();

        let project = import_folder(&workspace, source.to_str().unwrap()).unwrap();
        assert!(project
            .current_preview
            .as_deref()
            .is_some_and(|value| value.ends_with("narration.mp3")));
        assert!(project
            .assets
            .iter()
            .any(|asset| asset.kind == "imported-audio"));

        delete_project(&workspace, &project.id).unwrap();
        assert!(source.join("narration.mp3").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imports_images_and_non_native_video_as_project_media() {
        let (root, workspace) = temp_workspace(false);
        let source = root.join("mixed-media");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("camera.mov"), b"fake-mov-for-import-test").unwrap();
        fs::write(source.join("poster.png"), b"fake-png-for-import-test").unwrap();

        let project = import_folder(&workspace, source.to_str().unwrap()).unwrap();
        assert!(project
            .current_preview
            .as_deref()
            .is_some_and(|value| value.ends_with("camera.mov")));
        assert!(project
            .assets
            .iter()
            .any(|asset| asset.kind == "imported-image"));
        assert!(list_project_files(&workspace, &project.id)
            .unwrap()
            .iter()
            .any(|file| file.kind == "image" && file.name == "poster.png"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imports_media_folder_and_deletes_only_project_copy() {
        let (root, workspace) = temp_workspace(false);
        let source = root.join("outside-media");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("tutorial.mp4"), b"fake-mp4-for-import-test").unwrap();
        fs::write(
            source.join("nested/captions.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHello\n",
        )
        .unwrap();
        fs::write(source.join("notes.txt"), b"ignore me").unwrap();

        let project = import_folder(&workspace, source.to_str().unwrap()).unwrap();
        assert_eq!(project.name, "outside-media");
        assert!(project
            .current_preview
            .as_deref()
            .is_some_and(|value| value.ends_with("tutorial.mp4")));
        assert!(project
            .current_subtitle
            .as_deref()
            .is_some_and(|value| value.ends_with(".vtt")));
        assert_eq!(
            project
                .assets
                .iter()
                .filter(|asset| asset.kind == "imported-video")
                .count(),
            1
        );
        assert!(project
            .assets
            .iter()
            .any(|asset| asset.kind == "imported-subtitle"));

        let copied_root =
            resolve_project_root(&workspace, &project, AccessOperation::Read).unwrap();
        assert!(copied_root.is_dir());
        assert!(source.join("tutorial.mp4").is_file());

        delete_project(&workspace, &project.id).unwrap();
        assert!(!copied_root.exists());
        assert!(source.join("tutorial.mp4").is_file());
        assert!(source.join("nested/captions.srt").is_file());

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ai_workspace_capture_command_records_real_x11_video_when_tools_exist() {
        let Some(xvfb) = program_on_path("Xvfb") else {
            return;
        };
        let Some(ffmpeg) = program_on_path("ffmpeg") else {
            return;
        };
        let Some(ffprobe) = program_on_path("ffprobe") else {
            return;
        };

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "repotunnel-video-record-smoke-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let xauth = root.join("xauth");
        fs::write(&xauth, b"").unwrap();
        let output = root.join("capture.mkv");
        let display = format!(":{}", 200 + (nonce % 2000));

        let mut xvfb_child = Command::new(xvfb)
            .args([
                &display,
                "-screen",
                "0",
                "640x360x24",
                "-nolisten",
                "tcp",
                "-ac",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(350));
        assert!(xvfb_child.try_wait().unwrap().is_none());

        let capture_settings = AiWorkspaceCapture {
            display: &display,
            xauth_path: &xauth,
            width: 640,
            height: 360,
            fps: 12,
            max_seconds: 1,
            output: &output,
        };
        let mut capture =
            build_ai_workspace_capture_command(Path::new(&ffmpeg), &capture_settings).unwrap();
        let status = capture.status().unwrap();

        let _ = xvfb_child.kill();
        let _ = xvfb_child.wait();

        assert!(status.success());
        assert!(fs::metadata(&output).unwrap().len() > 1_000);

        let probe = Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(probe.status.success());
        let duration = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse::<f64>()
            .unwrap();
        assert!((0.7..=1.4).contains(&duration));

        fs::remove_dir_all(root).unwrap();
    }
}
