use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::AppHandle;

use crate::{
    access::AccessOperation,
    models::Workspace,
    video,
    video_director::{self, StoryCompiledShot, StoryShotRenderInput},
    video_narration::{self, NarrationRequest},
    video_production::{self, VideoProductionProject},
    video_scene::{self, VideoSceneElement, VideoSceneSpec},
};

const MAX_STORY_RENDER_JOBS: usize = 128;
const BLENDER_PRODUCTION_SCRIPT: &str = include_str!("../resources/video_story_blender.py");
static STORY_RENDER_JOB_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static STORY_RENDER_JOBS: OnceLock<Mutex<HashMap<String, StoryShotRenderRuntime>>> =
    OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryShotRenderResult {
    pub(crate) project_id: String,
    pub(crate) shot_id: String,
    pub(crate) render_key: String,
    pub(crate) selected_engine: String,
    pub(crate) output_path: String,
    pub(crate) reused: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryAnimaticRender {
    pub(crate) project_id: String,
    pub(crate) output_path: String,
    pub(crate) shot_count: usize,
    pub(crate) duration_seconds: f64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoryShotRenderJob {
    pub(crate) id: String,
    pub(crate) project_id: String,
    pub(crate) shot_id: String,
    pub(crate) render_key: String,
    pub(crate) selected_engine: String,
    pub(crate) status: String,
    pub(crate) phase: String,
    pub(crate) progress: u8,
    pub(crate) message: String,
    pub(crate) result: Option<StoryShotRenderResult>,
    pub(crate) error: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

struct StoryShotRenderRuntime {
    job: StoryShotRenderJob,
    cancel: Arc<AtomicBool>,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn jobs() -> &'static Mutex<HashMap<String, StoryShotRenderRuntime>> {
    STORY_RENDER_JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn new_job_id() -> String {
    format!(
        "story-shot-{}-{}",
        now_millis(),
        STORY_RENDER_JOB_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn render_key_tag(render_key: &str) -> &str {
    render_key.get(..16).unwrap_or(render_key)
}

fn inside_path(project: &VideoProductionProject, inside: &str) -> String {
    format!(
        "{}/{}",
        project.relative_path,
        inside.trim_start_matches('/')
    )
}

fn shot_output_relative(project: &VideoProductionProject, shot: &StoryCompiledShot) -> String {
    inside_path(
        project,
        &format!(
            "story/shots/{}-{}.mp4",
            shot.shot.id,
            render_key_tag(&shot.render_key)
        ),
    )
}

fn staged_output_relative(project: &VideoProductionProject, shot: &StoryCompiledShot) -> String {
    inside_path(
        project,
        &format!(
            "story/shots/.{}-{}.pending-{}-{}.mp4",
            shot.shot.id,
            render_key_tag(&shot.render_key),
            std::process::id(),
            now_millis()
        ),
    )
}

fn engine_dir_relative(project: &VideoProductionProject, shot: &StoryCompiledShot) -> String {
    inside_path(
        project,
        &format!(
            "story/engines/{}/{}-{}",
            shot.selected_engine,
            shot.shot.id,
            render_key_tag(&shot.render_key)
        ),
    )
}

fn job_path(
    workspace: &Workspace,
    project: &VideoProductionProject,
    job_id: &str,
    operation: AccessOperation,
    must_exist: bool,
) -> Result<PathBuf, String> {
    video_production::resolve_project_path(
        workspace,
        project,
        &inside_path(project, &format!("story/cache/{job_id}.json")),
        operation,
        must_exist,
    )
}

fn persist_job(
    workspace: &Workspace,
    project: &VideoProductionProject,
    job: &StoryShotRenderJob,
) -> Result<(), String> {
    let path = job_path(workspace, project, &job.id, AccessOperation::Write, false)?;
    let bytes = serde_json::to_vec_pretty(job)
        .map_err(|error| format!("Could not serialize story render job: {error}"))?;
    fs::write(path, bytes).map_err(|error| format!("Could not persist story render job: {error}"))
}

fn load_job(
    workspace: &Workspace,
    project: &VideoProductionProject,
    job_id: &str,
) -> Result<StoryShotRenderJob, String> {
    if job_id.is_empty()
        || job_id.len() > 160
        || !job_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Story render job ID is invalid.".to_string());
    }
    let path = job_path(workspace, project, job_id, AccessOperation::Read, true)?;
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read story render job: {error}"))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("Story render job state is invalid: {error}"))
}

fn update_job(
    workspace: &Workspace,
    project: &VideoProductionProject,
    job_id: &str,
    mutate: impl FnOnce(&mut StoryShotRenderJob),
) -> Result<StoryShotRenderJob, String> {
    let job = {
        let mut guard = jobs()
            .lock()
            .map_err(|_| "Story render job state is unavailable.".to_string())?;
        let runtime = guard.get_mut(job_id).ok_or_else(|| {
            "Story render job is no longer active in this RepoTunnel session.".to_string()
        })?;
        mutate(&mut runtime.job);
        runtime.job.updated_at = now_millis();
        runtime.job.clone()
    };
    persist_job(workspace, project, &job)?;
    Ok(job)
}

fn prune_jobs(guard: &mut HashMap<String, StoryShotRenderRuntime>) {
    if guard.len() < MAX_STORY_RENDER_JOBS {
        return;
    }
    let mut removable = guard
        .iter()
        .filter(|(_, runtime)| {
            matches!(
                runtime.job.status.as_str(),
                "completed" | "failed" | "cancelled"
            )
        })
        .map(|(id, runtime)| (id.clone(), runtime.job.updated_at))
        .collect::<Vec<_>>();
    removable.sort_by_key(|(_, updated)| *updated);
    let count = guard.len().saturating_sub(MAX_STORY_RENDER_JOBS - 1);
    for (id, _) in removable.into_iter().take(count) {
        guard.remove(&id);
    }
}

fn reusable_result(
    workspace: &Workspace,
    project: &VideoProductionProject,
    shot: &StoryCompiledShot,
) -> Option<StoryShotRenderResult> {
    let queue = video_director::get_render_queue(workspace, &project.id).ok()?;
    let entry = queue.entries.iter().find(|entry| {
        entry.shot_id == shot.shot.id
            && entry.render_key == shot.render_key
            && entry.selected_engine == shot.selected_engine
            && entry.status == "ready"
    })?;
    let output_path = entry.output_path.as_deref()?;
    let resolved = video_production::resolve_project_path(
        workspace,
        project,
        output_path,
        AccessOperation::Read,
        true,
    )
    .ok()?;
    let metadata = fs::symlink_metadata(resolved).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    Some(StoryShotRenderResult {
        project_id: project.id.clone(),
        shot_id: shot.shot.id.clone(),
        render_key: shot.render_key.clone(),
        selected_engine: shot.selected_engine.clone(),
        output_path: output_path.to_string(),
        reused: true,
    })
}

fn selected_engine_executable(engine: &str) -> Result<PathBuf, String> {
    let capability = video_director::production_capabilities()
        .engines
        .into_iter()
        .find(|item| item.id == engine)
        .ok_or_else(|| format!("Story engine '{engine}' is not registered."))?;
    if !capability.available {
        return Err(format!(
            "Story shot requires '{}', but that engine is not installed or detected. RepoTunnel will not silently install heavy software or switch engines.",
            engine
        ));
    }
    capability.executable.map(PathBuf::from).ok_or_else(|| {
        format!(
            "Story engine '{}' does not expose an executable adapter.",
            engine
        )
    })
}

fn run_engine_command(
    command: &mut Command,
    cancel: &AtomicBool,
    label: &str,
) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start {label}: {error}"))?;
    let stderr = child.stderr.take();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_end(&mut bytes);
        }
        bytes
    });

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            video::terminate_child(&mut child);
            let _ = reader.join();
            return Err("Story shot render cancelled.".to_string());
        }
        match child
            .try_wait()
            .map_err(|error| format!("Could not inspect {label}: {error}"))?
        {
            Some(status) => break status,
            None => thread::sleep(Duration::from_millis(120)),
        }
    };

    let stderr = reader.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let mut detail = String::from_utf8_lossy(&stderr).trim().to_string();
    if detail.len() > 4000 {
        detail.truncate(4000);
    }
    if detail.is_empty() {
        detail = format!("{label} exited with status {status}.");
    }
    Err(format!("{label} failed: {detail}"))
}

fn ensure_regular_output(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {label}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(format!("{label} did not produce a non-empty regular file."));
    }
    Ok(())
}

fn install_render_output(staged: &Path, final_path: &Path, label: &str) -> Result<(), String> {
    ensure_regular_output(staged, label)?;
    let Some(parent) = final_path.parent() else {
        return Err(format!("{label} destination has no parent directory."));
    };
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create {label} destination: {error}"))?;

    if !final_path.exists() {
        return fs::rename(staged, final_path)
            .map_err(|error| format!("Could not install {label}: {error}"));
    }

    let metadata = fs::symlink_metadata(final_path)
        .map_err(|error| format!("Could not inspect existing {label}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "Existing {label} destination is not a regular project-owned file."
        ));
    }

    let file_name = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("story-shot.mp4");
    let backup = parent.join(format!(
        ".{file_name}.previous-{}-{}",
        std::process::id(),
        now_millis()
    ));
    fs::rename(final_path, &backup)
        .map_err(|error| format!("Could not protect the previous {label}: {error}"))?;
    match fs::rename(staged, final_path) {
        Ok(()) => {
            let _ = fs::remove_file(backup);
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, final_path);
            Err(format!(
                "Could not install the new {label}; the previous output was restored: {error}"
            ))
        }
    }
}

fn actor_x(index: usize, total: usize, width: u32) -> f64 {
    if total <= 1 {
        return f64::from(width) * 0.5;
    }
    let margin = f64::from(width) * 0.18;
    let span = f64::from(width) - margin * 2.0;
    margin + span * index as f64 / (total - 1) as f64
}

fn native_scene(
    project: &VideoProductionProject,
    plan: &video_director::StoryDirectorPlan,
    shot: &StoryCompiledShot,
) -> VideoSceneSpec {
    let location_name = plan
        .locations
        .iter()
        .find(|location| location.id == shot.shot.location_id)
        .map(|location| location.name.clone())
        .unwrap_or_else(|| shot.shot.location_id.clone());
    let mut elements = vec![
        VideoSceneElement {
            kind: "text".to_string(),
            text: Some(location_name),
            x: f64::from(project.width) * 0.07,
            y: f64::from(project.height) * 0.12,
            width: f64::from(project.width) * 0.86,
            height: f64::from(project.height) * 0.1,
            x2: 0.0,
            y2: 0.0,
            radius: 0.0,
            corner_radius: 0.0,
            font_size: (f64::from(project.height) * 0.055).clamp(24.0, 72.0),
            stroke_width: 0.0,
            fill: Some("#f4f4f4".to_string()),
            stroke: None,
            font_family: None,
            start_seconds: 0.0,
            end_seconds: None,
            animation: Some("fade".to_string()),
        },
        VideoSceneElement {
            kind: "rect".to_string(),
            text: None,
            x: f64::from(project.width) * 0.08,
            y: f64::from(project.height) * 0.70,
            width: f64::from(project.width) * 0.84,
            height: f64::from(project.height) * 0.025,
            x2: 0.0,
            y2: 0.0,
            radius: 0.0,
            corner_radius: 8.0,
            font_size: 0.0,
            stroke_width: 0.0,
            fill: Some("#707070".to_string()),
            stroke: None,
            font_family: None,
            start_seconds: 0.0,
            end_seconds: None,
            animation: Some("draw".to_string()),
        },
    ];
    if !shot.shot.notes.trim().is_empty() {
        elements.push(VideoSceneElement {
            kind: "text".to_string(),
            text: Some(shot.shot.notes.clone()),
            x: f64::from(project.width) * 0.12,
            y: f64::from(project.height) * 0.39,
            width: f64::from(project.width) * 0.76,
            height: f64::from(project.height) * 0.2,
            x2: 0.0,
            y2: 0.0,
            radius: 0.0,
            corner_radius: 0.0,
            font_size: (f64::from(project.height) * 0.042).clamp(20.0, 54.0),
            stroke_width: 0.0,
            fill: Some("#e0e0e0".to_string()),
            stroke: None,
            font_family: None,
            start_seconds: 0.1,
            end_seconds: None,
            animation: Some("slideUp".to_string()),
        });
    }
    VideoSceneSpec {
        version: 1,
        id: format!(
            "story-{}-{}",
            shot.shot.id,
            render_key_tag(&shot.render_key)
        ),
        duration_seconds: shot.shot.duration_seconds.min(30.0),
        background: Some("#15171c".to_string()),
        elements,
    }
}

fn render_native(
    app: &AppHandle,
    workspace: &Workspace,
    project: &VideoProductionProject,
    plan: &video_director::StoryDirectorPlan,
    shot: &StoryCompiledShot,
    cancel: &AtomicBool,
) -> Result<String, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Story shot render cancelled.".to_string());
    }
    if !shot.shot.actors.is_empty() {
        return Err(
            "native-motion is reserved for actor-free story shots; character acting must use a story engine."
                .to_string(),
        );
    }
    let scene = native_scene(project, plan, shot);
    let render = video_scene::render_scene(app, workspace, &project.id, scene)?;
    if shot.shot.duration_seconds <= 30.0 {
        return Ok(render.output_path);
    }

    let source = video_production::resolve_project_path(
        workspace,
        project,
        &render.output_path,
        AccessOperation::Read,
        true,
    )?;
    let output_relative = shot_output_relative(project, shot);
    let output = video_production::resolve_project_path(
        workspace,
        project,
        &output_relative,
        AccessOperation::Write,
        false,
    )?;
    let staged_relative = staged_output_relative(project, shot);
    let staged = video_production::resolve_project_path(
        workspace,
        project,
        &staged_relative,
        AccessOperation::Write,
        false,
    )?;
    let ffmpeg =
        video::ffmpeg_program(app, project.resource_policy.allow_automatic_package_install)?;
    let mut command = Command::new(ffmpeg);
    command
        .arg("-y")
        .arg("-stream_loop")
        .arg("-1")
        .arg("-i")
        .arg(&source)
        .arg("-t")
        .arg(format!("{:.3}", shot.shot.duration_seconds))
        .arg("-an")
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&staged);
    let result = run_engine_command(&mut command, cancel, "native story duration extender")
        .and_then(|_| install_render_output(&staged, &output, "native story render"));
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result?;
    Ok(output_relative)
}

#[derive(Clone, Debug)]
struct StoryAudioTrack {
    path: PathBuf,
    start_seconds: f64,
    volume: f64,
}

fn absolute_project_asset(
    workspace: &Workspace,
    project: &VideoProductionProject,
    relative: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(relative) = relative.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let resolved = video_production::resolve_project_path(
        workspace,
        project,
        relative,
        AccessOperation::Read,
        true,
    )?;
    let metadata = fs::symlink_metadata(&resolved)
        .map_err(|error| format!("Could not inspect story asset '{relative}': {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "Story asset '{relative}' must be a regular project-owned file."
        ));
    }
    Ok(Some(resolved.to_string_lossy().into_owned()))
}

fn looks_like_audio_path(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.contains('/')
        || lower.ends_with(".wav")
        || lower.ends_with(".mp3")
        || lower.ends_with(".ogg")
        || lower.ends_with(".flac")
        || lower.ends_with(".m4a")
        || lower.ends_with(".aac")
}

fn resolve_story_audio_cue(
    workspace: &Workspace,
    project: &VideoProductionProject,
    cue: &str,
) -> Result<Option<PathBuf>, String> {
    let cue = cue.trim();
    if cue.is_empty() {
        return Ok(None);
    }

    let relative = if looks_like_audio_path(cue) {
        Some(cue.to_string())
    } else {
        project
            .assets
            .iter()
            .filter(|asset| {
                let kind = asset.kind.to_ascii_lowercase();
                kind.contains("audio")
                    || kind.contains("narration")
                    || kind.contains("sound")
                    || kind.contains("foley")
                    || kind.contains("ambience")
            })
            .find(|asset| {
                asset
                    .label
                    .as_deref()
                    .map(|label| label.trim().eq_ignore_ascii_case(cue))
                    .unwrap_or(false)
                    || Path::new(&asset.relative_path)
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .map(|stem| stem.eq_ignore_ascii_case(cue))
                        .unwrap_or(false)
            })
            .map(|asset| asset.relative_path.clone())
    };

    let Some(relative) = relative else {
        return Ok(None);
    };
    let resolved = video_production::resolve_project_path(
        workspace,
        project,
        &relative,
        AccessOperation::Read,
        true,
    )?;
    let metadata = fs::symlink_metadata(&resolved).map_err(|error| {
        format!("Could not inspect story audio cue '{cue}' resolved as '{relative}': {error}")
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "Story audio cue '{cue}' resolved as '{relative}' but is not a non-empty regular project-owned file."
        ));
    }
    Ok(Some(resolved))
}

fn prepare_lipsync_waveform(
    app: &AppHandle,
    workspace: &Workspace,
    project: &VideoProductionProject,
    shot: &StoryCompiledShot,
    character_id: &str,
    source_audio: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let relative = inside_path(
        project,
        &format!(
            "story/cache/lipsync-audio-{}-{}-{}.wav",
            shot.shot.id,
            character_id,
            render_key_tag(&shot.render_key)
        ),
    );
    let output = video_production::resolve_project_path(
        workspace,
        project,
        &relative,
        AccessOperation::Write,
        false,
    )?;
    if let Ok(metadata) = fs::symlink_metadata(&output) {
        if !metadata.file_type().is_symlink() && metadata.is_file() && metadata.len() > 0 {
            return Ok(output);
        }
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create lip-sync audio cache: {error}"))?;
    }

    let ffmpeg =
        video::ffmpeg_program(app, project.resource_policy.allow_automatic_package_install)?;
    let mut command = Command::new(ffmpeg);
    command
        .arg("-y")
        .arg("-i")
        .arg(source_audio)
        .arg("-vn")
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg("16000")
        .arg("-c:a")
        .arg("pcm_s16le")
        .arg(&output);
    let result = run_engine_command(&mut command, cancel, "story lip-sync waveform normalizer")
        .and_then(|_| ensure_regular_output(&output, "story lip-sync waveform"));
    if result.is_err() {
        let _ = fs::remove_file(&output);
    }
    result?;
    Ok(output)
}

fn rhubarb_cue_relative(
    project: &VideoProductionProject,
    shot: &StoryCompiledShot,
    character_id: &str,
) -> String {
    inside_path(
        project,
        &format!(
            "story/cache/lipsync-{}-{}-{}.json",
            shot.shot.id,
            character_id,
            render_key_tag(&shot.render_key)
        ),
    )
}

fn generate_rhubarb_cues(
    workspace: &Workspace,
    project: &VideoProductionProject,
    shot: &StoryCompiledShot,
    character_id: &str,
    audio: &Path,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let available = Command::new("rhubarb")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let Ok(status) = available else {
        return Ok(None);
    };
    if !status.success() {
        return Ok(None);
    }

    let relative = rhubarb_cue_relative(project, shot, character_id);
    let output = video_production::resolve_project_path(
        workspace,
        project,
        &relative,
        AccessOperation::Write,
        false,
    )?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create lip-sync cache directory: {error}"))?;
    }
    let mut command = Command::new("rhubarb");
    command
        .arg("-f")
        .arg("json")
        .arg("-o")
        .arg(&output)
        .arg(audio);
    run_engine_command(&mut command, cancel, "Rhubarb lip-sync analyzer")?;
    ensure_regular_output(&output, "Rhubarb lip-sync cue file")?;
    Ok(Some(output.to_string_lossy().into_owned()))
}

fn prepare_story_audio(
    app: &AppHandle,
    workspace: &Workspace,
    project: &VideoProductionProject,
    plan: &video_director::StoryDirectorPlan,
    shot: &StoryCompiledShot,
    cancel: &AtomicBool,
) -> Result<(HashMap<String, serde_json::Value>, Vec<StoryAudioTrack>), String> {
    let mut dialogue = HashMap::new();
    let mut tracks = Vec::new();

    for actor in &shot.shot.actors {
        let Some(text) = actor
            .dialogue
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if cancel.load(Ordering::Relaxed) {
            return Err("Story shot render cancelled.".to_string());
        }
        let asset = video_narration::synthesize(
            app,
            workspace,
            &project.id,
            NarrationRequest {
                text: text.to_string(),
                language: plan.language.clone(),
                scene_id: None,
                character_id: Some(actor.character_id.clone()),
                provider: None,
                voice: None,
                voice_model_path: None,
                rate: None,
                allow_managed_download: false,
            },
        )
        .map_err(|error| {
            format!(
                "Could not synthesize dialogue for story character '{}': {error}",
                actor.character_id
            )
        })?;
        let audio = video_production::resolve_project_path(
            workspace,
            project,
            &asset.audio_path,
            AccessOperation::Read,
            true,
        )?;
        ensure_regular_output(&audio, "story dialogue audio")?;
        let lip_sync_audio = if actor.lip_sync {
            Some(prepare_lipsync_waveform(
                app,
                workspace,
                project,
                shot,
                &actor.character_id,
                &audio,
                cancel,
            )?)
        } else {
            None
        };
        let cue_path = if let Some(lip_sync_audio) = lip_sync_audio.as_deref() {
            generate_rhubarb_cues(
                workspace,
                project,
                shot,
                &actor.character_id,
                lip_sync_audio,
                cancel,
            )?
        } else {
            None
        };
        dialogue.insert(
            actor.character_id.clone(),
            json!({
                "path": lip_sync_audio.as_deref().unwrap_or(audio.as_path()).to_string_lossy(),
                "duration": asset.duration_seconds,
                "provider": asset.provider,
                "voice": asset.voice,
                "lipSyncCuePath": cue_path,
            }),
        );
        tracks.push(StoryAudioTrack {
            path: audio,
            start_seconds: 0.0,
            volume: 1.0,
        });
    }

    for cue in &shot.shot.ambience {
        if let Some(path) = resolve_story_audio_cue(workspace, project, cue)? {
            tracks.push(StoryAudioTrack {
                path,
                start_seconds: 0.0,
                volume: 0.18,
            });
        }
    }
    let foley_count = shot.shot.foley.len();
    for (index, cue) in shot.shot.foley.iter().enumerate() {
        if let Some(path) = resolve_story_audio_cue(workspace, project, cue)? {
            let start_seconds = if foley_count == 0 {
                0.0
            } else {
                shot.shot.duration_seconds * (index + 1) as f64 / (foley_count + 1) as f64
            };
            tracks.push(StoryAudioTrack {
                path,
                start_seconds,
                volume: 0.70,
            });
        }
    }

    Ok((dialogue, tracks))
}

fn mix_story_audio(
    app: &AppHandle,
    project: &VideoProductionProject,
    visual: &Path,
    output: &Path,
    duration_seconds: f64,
    tracks: &[StoryAudioTrack],
    cancel: &AtomicBool,
) -> Result<(), String> {
    if tracks.is_empty() {
        fs::rename(visual, output)
            .map_err(|error| format!("Could not install silent story visual: {error}"))?;
        return Ok(());
    }

    let ffmpeg =
        video::ffmpeg_program(app, project.resource_policy.allow_automatic_package_install)?;
    let mut command = Command::new(ffmpeg);
    command.arg("-y").arg("-i").arg(visual);
    for track in tracks {
        command.arg("-i").arg(&track.path);
    }

    let mut filter = String::new();
    let mut labels = Vec::new();
    for (index, track) in tracks.iter().enumerate() {
        let label = format!("a{}", index + 1);
        let delay = (track.start_seconds.max(0.0) * 1000.0).round() as u64;
        filter.push_str(&format!(
            "[{}:a]volume={:.3},adelay={}|{},apad[{}];",
            index + 1,
            track.volume.clamp(0.0, 2.0),
            delay,
            delay,
            label
        ));
        labels.push(format!("[{label}]"));
    }
    filter.push_str(&format!(
        "{}amix=inputs={}:duration=longest:normalize=0,atrim=0:{:.3}[mix]",
        labels.join(""),
        labels.len(),
        duration_seconds.max(0.05)
    ));

    command
        .arg("-filter_complex")
        .arg(filter)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("[mix]")
        .arg("-vf")
        .arg(format!(
            "scale={}:{}:flags=lanczos,fps={}",
            project.width,
            project.height,
            project.fps.clamp(12, 60)
        ))
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-crf")
        .arg("18")
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("192k")
        .arg("-t")
        .arg(format!("{:.3}", duration_seconds))
        .arg("-movflags")
        .arg("+faststart")
        .arg(output);
    run_engine_command(&mut command, cancel, "story dialogue/Foley/ambience mixer")
}

fn animatic_text(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut truncated = compact
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn animatic_card_scene(
    project: &VideoProductionProject,
    plan: &video_director::StoryDirectorPlan,
    shot: &StoryCompiledShot,
    index: usize,
) -> VideoSceneSpec {
    let width = f64::from(project.width);
    let height = f64::from(project.height);
    let location_name = plan
        .locations
        .iter()
        .find(|location| location.id == shot.shot.location_id)
        .map(|location| location.name.as_str())
        .unwrap_or(shot.shot.location_id.as_str());

    let actor_summary = if shot.shot.actors.is_empty() {
        "No character blocking".to_string()
    } else {
        shot.shot
            .actors
            .iter()
            .map(|actor| {
                let name = plan
                    .characters
                    .iter()
                    .find(|character| character.id == actor.character_id)
                    .map(|character| character.name.as_str())
                    .unwrap_or(actor.character_id.as_str());
                format!("{name}: {}", actor.action)
            })
            .collect::<Vec<_>>()
            .join("   |   ")
    };
    let camera = format!(
        "{} · {} · {}",
        shot.shot.camera.shot_type, shot.shot.camera.movement, shot.shot.camera.angle
    );
    let notes = if shot.shot.notes.trim().is_empty() {
        "Blocking preview — approve timing, action order, camera and continuity before final rendering."
            .to_string()
    } else {
        shot.shot.notes.clone()
    };

    let elements = vec![
        VideoSceneElement {
            kind: "text".to_string(),
            text: Some(format!(
                "SHOT {:02}  ·  {}  ·  {:.1}s",
                index + 1,
                location_name,
                shot.shot.duration_seconds
            )),
            x: width * 0.06,
            y: height * 0.10,
            width: width * 0.88,
            height: height * 0.10,
            x2: 0.0,
            y2: 0.0,
            radius: 0.0,
            corner_radius: 0.0,
            font_size: (height * 0.050).clamp(22.0, 60.0),
            stroke_width: 0.0,
            fill: Some("#f4f4f4".to_string()),
            stroke: None,
            font_family: None,
            start_seconds: 0.0,
            end_seconds: None,
            animation: Some("fade".to_string()),
        },
        VideoSceneElement {
            kind: "rect".to_string(),
            text: None,
            x: width * 0.06,
            y: height * 0.25,
            width: width * 0.88,
            height: height * 0.12,
            x2: 0.0,
            y2: 0.0,
            radius: 0.0,
            corner_radius: 18.0,
            font_size: 0.0,
            stroke_width: 0.0,
            fill: Some("#232a36".to_string()),
            stroke: None,
            font_family: None,
            start_seconds: 0.0,
            end_seconds: None,
            animation: Some("draw".to_string()),
        },
        VideoSceneElement {
            kind: "text".to_string(),
            text: Some(format!("Camera: {}", animatic_text(&camera, 58))),
            x: width * 0.09,
            y: height * 0.28,
            width: width * 0.82,
            height: height * 0.07,
            x2: 0.0,
            y2: 0.0,
            radius: 0.0,
            corner_radius: 0.0,
            font_size: (height * 0.035).clamp(18.0, 44.0),
            stroke_width: 0.0,
            fill: Some("#d9e2ef".to_string()),
            stroke: None,
            font_family: None,
            start_seconds: 0.0,
            end_seconds: None,
            animation: Some("fade".to_string()),
        },
        VideoSceneElement {
            kind: "text".to_string(),
            text: Some(format!("Blocking: {}", animatic_text(&actor_summary, 62))),
            x: width * 0.09,
            y: height * 0.43,
            width: width * 0.82,
            height: height * 0.13,
            x2: 0.0,
            y2: 0.0,
            radius: 0.0,
            corner_radius: 0.0,
            font_size: (height * 0.034).clamp(18.0, 42.0),
            stroke_width: 0.0,
            fill: Some("#e9edf4".to_string()),
            stroke: None,
            font_family: None,
            start_seconds: 0.0,
            end_seconds: None,
            animation: Some("slideUp".to_string()),
        },
        VideoSceneElement {
            kind: "text".to_string(),
            text: Some(animatic_text(&notes, 62)),
            x: width * 0.09,
            y: height * 0.62,
            width: width * 0.82,
            height: height * 0.18,
            x2: 0.0,
            y2: 0.0,
            radius: 0.0,
            corner_radius: 0.0,
            font_size: (height * 0.034).clamp(18.0, 38.0),
            stroke_width: 0.0,
            fill: Some("#bcc6d6".to_string()),
            stroke: None,
            font_family: None,
            start_seconds: 0.0,
            end_seconds: None,
            animation: Some("fade".to_string()),
        },
    ];

    VideoSceneSpec {
        version: 1,
        id: format!(
            "story-animatic-{}-{}",
            shot.shot.id,
            render_key_tag(&shot.render_key)
        ),
        duration_seconds: 1.0,
        background: Some("#151922".to_string()),
        elements,
    }
}

pub(crate) fn render_animatic(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
) -> Result<StoryAnimaticRender, String> {
    let project = video_production::get_project(workspace, project_id)?;
    if project.production_mode != "story" {
        return Err(
            "Story animatic rendering is available only for story-mode Video Projects.".to_string(),
        );
    }
    let plan = video_director::get_plan(workspace, project_id)?;
    if plan.shots.is_empty() {
        return Err("Story animatic requires at least one compiled shot.".to_string());
    }

    let animatic_dir_relative = inside_path(&project, "story/animatic");
    let animatic_dir = video_production::resolve_project_path(
        workspace,
        &project,
        &animatic_dir_relative,
        AccessOperation::Write,
        false,
    )?;
    fs::create_dir_all(&animatic_dir)
        .map_err(|error| format!("Could not create story animatic directory: {error}"))?;

    let ffmpeg =
        video::ffmpeg_program(app, project.resource_policy.allow_automatic_package_install)?;
    let cancel = AtomicBool::new(false);
    let mut clips = Vec::new();

    for (index, shot) in plan.shots.iter().enumerate() {
        let scene = animatic_card_scene(&project, &plan, shot, index);
        let rendered = video_scene::render_scene(app, workspace, project_id, scene)?;
        let source = video_production::resolve_project_path(
            workspace,
            &project,
            &rendered.output_path,
            AccessOperation::Read,
            true,
        )?;
        ensure_regular_output(&source, "story animatic card")?;

        let clip = animatic_dir.join(format!(
            ".clip-{:03}-{}-{}.mp4",
            index + 1,
            shot.shot.id,
            render_key_tag(&shot.render_key)
        ));
        let mut command = Command::new(&ffmpeg);
        command
            .arg("-y")
            .arg("-stream_loop")
            .arg("-1")
            .arg("-i")
            .arg(&source)
            .arg("-t")
            .arg(format!("{:.3}", shot.shot.duration_seconds))
            .arg("-vf")
            .arg("scale=854:480:force_original_aspect_ratio=decrease,pad=854:480:(ow-iw)/2:(oh-ih)/2,fps=12")
            .arg("-an")
            .arg("-c:v")
            .arg("libx264")
            .arg("-preset")
            .arg("veryfast")
            .arg("-crf")
            .arg("24")
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg("-movflags")
            .arg("+faststart")
            .arg(&clip);
        run_engine_command(&mut command, &cancel, "story animatic shot renderer")?;
        ensure_regular_output(&clip, "story animatic shot")?;
        clips.push(clip);
    }

    let output_relative = inside_path(
        &project,
        &format!(
            "story/animatic/preview-{}.mp4",
            render_key_tag(&plan.content_hash)
        ),
    );
    let output = video_production::resolve_project_path(
        workspace,
        &project,
        &output_relative,
        AccessOperation::Write,
        false,
    )?;
    let staged = animatic_dir.join(format!(
        ".preview-{}.pending-{}.mp4",
        render_key_tag(&plan.content_hash),
        now_millis()
    ));

    let mut command = Command::new(ffmpeg);
    command.arg("-y");
    for clip in &clips {
        command.arg("-i").arg(clip);
    }
    let labels = (0..clips.len())
        .map(|index| format!("[{index}:v]"))
        .collect::<Vec<_>>()
        .join("");
    let filter = format!("{labels}concat=n={}:v=1:a=0[outv]", clips.len());
    command
        .arg("-filter_complex")
        .arg(filter)
        .arg("-map")
        .arg("[outv]")
        .arg("-an")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("veryfast")
        .arg("-crf")
        .arg("24")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+faststart")
        .arg(&staged);
    let result = run_engine_command(&mut command, &cancel, "story animatic assembler")
        .and_then(|_| install_render_output(&staged, &output, "story animatic preview"));

    for clip in clips {
        let _ = fs::remove_file(clip);
    }
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result?;

    let total_duration_seconds = plan
        .shots
        .iter()
        .map(|shot| shot.shot.duration_seconds)
        .sum::<f64>();
    Ok(StoryAnimaticRender {
        project_id: project.id,
        output_path: output_relative,
        shot_count: plan.shots.len(),
        duration_seconds: total_duration_seconds,
        width: 854,
        height: 480,
        fps: 12,
    })
}

fn story_payload(
    workspace: &Workspace,
    project: &VideoProductionProject,
    plan: &video_director::StoryDirectorPlan,
    shot: &StoryCompiledShot,
    output: &Path,
    dialogue_audio: &HashMap<String, serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut characters = Vec::new();
    for (index, beat) in shot.shot.actors.iter().enumerate() {
        let character = plan
            .characters
            .iter()
            .find(|character| character.id == beat.character_id);
        let asset_path = absolute_project_asset(
            workspace,
            project,
            character.and_then(|item| item.asset_path.as_deref()),
        )?;
        let audio = dialogue_audio.get(&beat.character_id);
        characters.push(json!({
            "id": beat.character_id,
            "name": character.map(|item| item.name.as_str()).unwrap_or(beat.character_id.as_str()),
            "rigProfile": character.map(|item| item.rig_profile.as_str()).unwrap_or("generic-humanoid"),
            "action": beat.action,
            "emotion": beat.emotion,
            "startAnchor": beat.start_anchor,
            "endAnchor": beat.end_anchor,
            "targetId": beat.target_id,
            "handTarget": beat.hand_target,
            "lookTarget": beat.look_target,
            "dialogue": beat.dialogue,
            "dialogueAudio": audio,
            "lipSyncCuePath": audio
                .and_then(|value| value.get("lipSyncCuePath"))
                .and_then(serde_json::Value::as_str),
            "lipSync": beat.lip_sync,
            "x": actor_x(index, shot.shot.actors.len(), project.width),
            "assetPath": asset_path,
        }));
    }

    let location = plan
        .locations
        .iter()
        .find(|location| location.id == shot.shot.location_id);
    let location_asset = absolute_project_asset(
        workspace,
        project,
        location.and_then(|item| item.asset_path.as_deref()),
    )?;

    let mut props = Vec::new();
    for prop_id in &shot.shot.props {
        let prop = plan
            .props
            .iter()
            .find(|item| item.id == *prop_id)
            .ok_or_else(|| format!("Story shot references unknown prop '{prop_id}'."))?;
        props.push(json!({
            "id": prop.id,
            "name": prop.name,
            "description": prop.description,
            "ownerCharacterId": prop.owner_character_id,
            "handling": prop.handling,
            "assetPath": absolute_project_asset(
                workspace,
                project,
                prop.asset_path.as_deref(),
            )?,
        }));
    }

    Ok(json!({
        "projectId": project.id,
        "shotId": shot.shot.id,
        "sceneId": shot.shot.scene_id,
        "renderKey": shot.render_key,
        "engine": shot.selected_engine,
        "duration": shot.shot.duration_seconds,
        "width": project.width,
        "height": project.height,
        "fps": project.fps.clamp(12, 60),
        "output": output.to_string_lossy(),
        "location": {
            "id": shot.shot.location_id,
            "name": location.map(|item| item.name.as_str()).unwrap_or(shot.shot.location_id.as_str()),
            "description": location.map(|item| item.description.as_str()).unwrap_or(""),
            "variant": shot.shot.location_variant,
            "entranceAnchors": location.map(|item| item.entrance_anchors.as_slice()).unwrap_or(&[]),
            "interactionAnchors": location.map(|item| item.interaction_anchors.as_slice()).unwrap_or(&[]),
            "cameraAnchors": location.map(|item| item.camera_anchors.as_slice()).unwrap_or(&[]),
            "walkableAreas": location.map(|item| item.walkable_areas.as_slice()).unwrap_or(&[]),
            "assetPath": location_asset,
        },
        "camera": shot.shot.camera,
        "actors": characters,
        "props": props,
        "ambience": shot.shot.ambience,
        "foley": shot.shot.foley,
        "transition": shot.shot.transition,
        "notes": shot.shot.notes,
    }))
}

fn write_engine_bundle(
    workspace: &Workspace,
    project: &VideoProductionProject,
    shot: &StoryCompiledShot,
    payload: &serde_json::Value,
) -> Result<PathBuf, String> {
    let relative = engine_dir_relative(project, shot);
    let directory = video_production::resolve_project_path(
        workspace,
        project,
        &relative,
        AccessOperation::Write,
        false,
    )?;
    if directory.exists() {
        let metadata = fs::symlink_metadata(&directory)
            .map_err(|error| format!("Could not inspect story engine bundle: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("Story engine bundle path is not a regular directory.".to_string());
        }
    } else {
        fs::create_dir_all(&directory)
            .map_err(|error| format!("Could not create story engine bundle: {error}"))?;
    }
    fs::write(
        directory.join("shot.json"),
        serde_json::to_vec_pretty(payload)
            .map_err(|error| format!("Could not serialize story engine input: {error}"))?,
    )
    .map_err(|error| format!("Could not write story engine input: {error}"))?;
    Ok(directory)
}

fn render_blender(
    app: &AppHandle,
    workspace: &Workspace,
    project: &VideoProductionProject,
    plan: &video_director::StoryDirectorPlan,
    shot: &StoryCompiledShot,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let executable = selected_engine_executable(&shot.selected_engine)?;
    let output_relative = shot_output_relative(project, shot);
    let output = video_production::resolve_project_path(
        workspace,
        project,
        &output_relative,
        AccessOperation::Write,
        false,
    )?;
    let staged_relative = staged_output_relative(project, shot);
    let staged = video_production::resolve_project_path(
        workspace,
        project,
        &staged_relative,
        AccessOperation::Write,
        false,
    )?;

    let (dialogue_audio, audio_tracks) =
        prepare_story_audio(app, workspace, project, plan, shot, cancel)?;
    let bundle_relative = engine_dir_relative(project, shot);
    let bundle = video_production::resolve_project_path(
        workspace,
        project,
        &bundle_relative,
        AccessOperation::Write,
        false,
    )?;
    if !bundle.exists() {
        fs::create_dir_all(&bundle)
            .map_err(|error| format!("Could not create Blender story bundle: {error}"))?;
    }
    let visual = bundle.join("visual.mp4");
    let payload = story_payload(workspace, project, plan, shot, &visual, &dialogue_audio)?;
    let bundle = write_engine_bundle(workspace, project, shot, &payload)?;
    let script = bundle.join("render.py");
    fs::write(&script, BLENDER_PRODUCTION_SCRIPT)
        .map_err(|error| format!("Could not write production Blender story adapter: {error}"))?;

    let mut command = Command::new(executable);
    command
        .arg("--background")
        .arg("--python")
        .arg(&script)
        .current_dir(&bundle);
    let result = run_engine_command(&mut command, cancel, "Blender story renderer")
        .and_then(|_| ensure_regular_output(&visual, "Blender story visual"))
        .and_then(|_| {
            mix_story_audio(
                app,
                project,
                &visual,
                &staged,
                shot.shot.duration_seconds,
                &audio_tracks,
                cancel,
            )
        })
        .and_then(|_| install_render_output(&staged, &output, "Blender story render"));
    let _ = fs::remove_file(&visual);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result?;
    Ok(output_relative)
}

const GODOT_PROJECT: &str = r#"[application]
config/name="RepoTunnel Story Shot"
run/main_scene="res://main.tscn"

[display]
window/size/viewport_width=854
window/size/viewport_height=480
window/size/window_width_override=854
window/size/window_height_override=480

[rendering]
renderer/rendering_method="gl_compatibility"
renderer/rendering_method.mobile="gl_compatibility"
"#;

const GODOT_SCENE: &str = r#"[gd_scene load_steps=2 format=3]

[ext_resource path="res://main.gd" type="Script" id="1"]

[node name="StoryShot" type="Node2D"]
script = ExtResource("1")
"#;

const GODOT_SCRIPT: &str = r##"extends Node2D

var data = {}
var elapsed := 0.0

func _ready():
    data = JSON.parse_string(FileAccess.get_file_as_string("res://shot.json"))
    if data == null:
        push_error("Invalid story shot JSON")
        get_tree().quit(2)
        return
    get_window().size = Vector2i(int(data["width"]), int(data["height"]))
    queue_redraw()

func _process(delta):
    elapsed += delta
    queue_redraw()
    if elapsed >= float(data["duration"]):
        get_tree().quit()

func _draw():
    if data == null or data.is_empty():
        return
    var width = float(data["width"])
    var height = float(data["height"])
    draw_rect(Rect2(0, 0, width, height), Color("#151922"))
    draw_rect(Rect2(0, height * 0.72, width, height * 0.28), Color("#30343c"))
    var font = ThemeDB.fallback_font
    var location = str(data["location"]["name"])
    draw_string(font, Vector2(width * 0.06, height * 0.10), location, HORIZONTAL_ALIGNMENT_LEFT, width * 0.8, int(height * 0.05), Color("#f2f2f2"))
    var actors = data.get("actors", [])
    var progress = clamp(elapsed / max(0.01, float(data["duration"])), 0.0, 1.0)
    for i in range(actors.size()):
        var actor = actors[i]
        var x = float(actor["x"])
        var end_shift = anchor_offset(actor.get("endAnchor"))
        x = lerp(x, x + end_shift, progress)
        var y = height * 0.64
        var body_color = [Color("#2f78d0"), Color("#df6c32"), Color("#55a865"), Color("#9f62c4")][i % 4]
        var action = str(actor.get("action", "idle"))
        var bob = 0.0
        if action == "walk":
            bob = sin(progress * TAU * 4.0) * height * 0.008
        elif action == "run":
            bob = sin(progress * TAU * 7.0) * height * 0.014
        elif action.begins_with("talk"):
            bob = sin(progress * TAU * 3.0) * height * 0.004
        draw_circle(Vector2(x, y - height * 0.10 + bob), height * 0.042, body_color)
        draw_rect(Rect2(x - height * 0.035, y - height * 0.06 + bob, height * 0.07, height * 0.13), body_color)
        draw_string(font, Vector2(x - height * 0.08, y + height * 0.11), str(actor["name"]), HORIZONTAL_ALIGNMENT_CENTER, height * 0.16, int(height * 0.026), Color("#ffffff"))
        if actor.get("dialogue") != null and str(actor.get("dialogue")) != "":
            draw_string(font, Vector2(x - width * 0.12, height * 0.24), str(actor.get("dialogue")), HORIZONTAL_ALIGNMENT_CENTER, width * 0.24, int(height * 0.028), Color("#e9edf4"))

func anchor_offset(value):
    if value == null or str(value) == "":
        return 0.0
    var total := 0
    for ch in str(value):
        total += ch.unicode_at(0)
    return ((float(total % 700) / 700.0) - 0.5) * float(data["width"]) * 0.28
"##;

fn transcode_to_mp4(
    app: &AppHandle,
    input: &Path,
    output: &Path,
    allow_install: bool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let ffmpeg = video::ffmpeg_program(app, allow_install)?;
    let mut command = Command::new(ffmpeg);
    command
        .arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-an")
        .arg("-c:v")
        .arg("libx264")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-movflags")
        .arg("+faststart")
        .arg(output);
    run_engine_command(&mut command, cancel, "story video transcoder")
}

fn render_godot(
    app: &AppHandle,
    workspace: &Workspace,
    project: &VideoProductionProject,
    plan: &video_director::StoryDirectorPlan,
    shot: &StoryCompiledShot,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let executable = selected_engine_executable(&shot.selected_engine)?;
    let version = Command::new(&executable)
        .arg("--version")
        .output()
        .map_err(|error| format!("Could not inspect Godot version: {error}"))?;
    let version_text = if version.stdout.is_empty() {
        String::from_utf8_lossy(&version.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&version.stdout).into_owned()
    };
    if !version.status.success() || !version_text.trim_start().starts_with('4') {
        return Err(format!(
            "Automatic godot-2d story rendering requires Godot 4; detected '{}'. RepoTunnel will not silently substitute another engine.",
            version_text.trim()
        ));
    }
    let output_relative = shot_output_relative(project, shot);
    let output = video_production::resolve_project_path(
        workspace,
        project,
        &output_relative,
        AccessOperation::Write,
        false,
    )?;
    let staged_relative = staged_output_relative(project, shot);
    let staged = video_production::resolve_project_path(
        workspace,
        project,
        &staged_relative,
        AccessOperation::Write,
        false,
    )?;
    let no_dialogue_audio = HashMap::new();
    let payload = story_payload(workspace, project, plan, shot, &staged, &no_dialogue_audio)?;
    let bundle = write_engine_bundle(workspace, project, shot, &payload)?;

    let project_text = GODOT_PROJECT
        .replace(
            "viewport_width=854",
            &format!("viewport_width={}", project.width),
        )
        .replace(
            "viewport_height=480",
            &format!("viewport_height={}", project.height),
        )
        .replace(
            "window_width_override=854",
            &format!("window_width_override={}", project.width),
        )
        .replace(
            "window_height_override=480",
            &format!("window_height_override={}", project.height),
        );
    fs::write(bundle.join("project.godot"), project_text)
        .map_err(|error| format!("Could not write Godot story project: {error}"))?;
    fs::write(bundle.join("main.tscn"), GODOT_SCENE)
        .map_err(|error| format!("Could not write Godot story scene: {error}"))?;
    fs::write(bundle.join("main.gd"), GODOT_SCRIPT)
        .map_err(|error| format!("Could not write Godot story adapter: {error}"))?;

    let movie = bundle.join("shot.avi");
    let mut command = Command::new(executable);
    command
        .arg("--headless")
        .arg("--path")
        .arg(&bundle)
        .arg("--write-movie")
        .arg(&movie)
        .arg("--fixed-fps")
        .arg(project.fps.clamp(12, 60).to_string())
        .current_dir(&bundle);
    let render_result = run_engine_command(&mut command, cancel, "Godot story renderer")
        .and_then(|_| ensure_regular_output(&movie, "Godot story movie"));
    if let Err(error) = render_result {
        let _ = fs::remove_file(&movie);
        return Err(error);
    }
    let result = transcode_to_mp4(
        app,
        &movie,
        &staged,
        project.resource_policy.allow_automatic_package_install,
        cancel,
    )
    .and_then(|_| install_render_output(&staged, &output, "Godot story render"));
    let _ = fs::remove_file(movie);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result?;
    Ok(output_relative)
}

fn render_external_optional_engine(engine: &str) -> Result<String, String> {
    Err(format!(
        "Story engine '{engine}' is available as an optional external adapter, but RepoTunnel does not synthesize a native scene file for it. Use that engine with the generated Scene Director plan and register the finished shot with record_video_story_shot_render, or choose Godot/Blender for automatic execution."
    ))
}

fn render_shot(
    app: &AppHandle,
    workspace: &Workspace,
    project: &VideoProductionProject,
    plan: &video_director::StoryDirectorPlan,
    shot: &StoryCompiledShot,
    cancel: &AtomicBool,
) -> Result<StoryShotRenderResult, String> {
    let output_path = match shot.selected_engine.as_str() {
        "native-motion" => render_native(app, workspace, project, plan, shot, cancel)?,
        "godot-2d" => render_godot(app, workspace, project, plan, shot, cancel)?,
        "blender-grease-pencil" | "blender-2.5d" | "blender-3d" => {
            render_blender(app, workspace, project, plan, shot, cancel)?
        }
        "opentoonz" | "synfig" => render_external_optional_engine(&shot.selected_engine)?,
        other => {
            return Err(format!(
                "Story engine '{other}' has no RepoTunnel execution adapter."
            ))
        }
    };

    if cancel.load(Ordering::Relaxed) {
        return Err("Story shot render cancelled.".to_string());
    }
    video_director::record_shot_render(
        workspace,
        &project.id,
        StoryShotRenderInput {
            shot_id: shot.shot.id.clone(),
            render_key: shot.render_key.clone(),
            output_path: output_path.clone(),
        },
    )?;

    Ok(StoryShotRenderResult {
        project_id: project.id.clone(),
        shot_id: shot.shot.id.clone(),
        render_key: shot.render_key.clone(),
        selected_engine: shot.selected_engine.clone(),
        output_path,
        reused: false,
    })
}

pub(crate) fn start_shot_render(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    shot_id: &str,
    force: bool,
) -> Result<StoryShotRenderJob, String> {
    let project = video_production::get_project(workspace, project_id)?;
    if project.production_mode != "story" {
        return Err(
            "Automatic story-shot rendering is available only for story-mode Video Projects."
                .to_string(),
        );
    }
    let plan = video_director::get_plan(workspace, project_id)?;
    let shot_id = shot_id.trim();
    let shot = plan
        .shots
        .iter()
        .find(|item| item.shot.id == shot_id)
        .cloned()
        .ok_or_else(|| {
            format!("Story shot '{shot_id}' is not present in the current Scene Director plan.")
        })?;

    if !force {
        if let Some(result) = reusable_result(workspace, &project, &shot) {
            let now = now_millis();
            let job = StoryShotRenderJob {
                id: new_job_id(),
                project_id: project.id.clone(),
                shot_id: shot.shot.id.clone(),
                render_key: shot.render_key.clone(),
                selected_engine: shot.selected_engine.clone(),
                status: "completed".to_string(),
                phase: "reused".to_string(),
                progress: 100,
                message: "Matching story shot render reused from the content-hash cache."
                    .to_string(),
                result: Some(result),
                error: None,
                created_at: now,
                updated_at: now,
            };
            persist_job(workspace, &project, &job)?;
            return Ok(job);
        }
    }

    {
        let guard = jobs()
            .lock()
            .map_err(|_| "Story render job state is unavailable.".to_string())?;
        if let Some(existing) = guard.values().find(|runtime| {
            runtime.job.project_id == project.id
                && runtime.job.shot_id == shot.shot.id
                && runtime.job.render_key == shot.render_key
                && matches!(runtime.job.status.as_str(), "queued" | "running")
        }) {
            return Ok(existing.job.clone());
        }
    }

    let now = now_millis();
    let job = StoryShotRenderJob {
        id: new_job_id(),
        project_id: project.id.clone(),
        shot_id: shot.shot.id.clone(),
        render_key: shot.render_key.clone(),
        selected_engine: shot.selected_engine.clone(),
        status: "queued".to_string(),
        phase: "queued".to_string(),
        progress: 0,
        message: format!("Story shot queued for {}.", shot.selected_engine),
        result: None,
        error: None,
        created_at: now,
        updated_at: now,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut guard = jobs()
            .lock()
            .map_err(|_| "Story render job state is unavailable.".to_string())?;
        prune_jobs(&mut guard);
        guard.insert(
            job.id.clone(),
            StoryShotRenderRuntime {
                job: job.clone(),
                cancel: Arc::clone(&cancel),
            },
        );
    }
    persist_job(workspace, &project, &job)?;

    let app = app.clone();
    let workspace = workspace.clone();
    let thread_project = project.clone();
    let thread_plan = plan.clone();
    let thread_shot = shot.clone();
    let job_id = job.id.clone();
    thread::spawn(move || {
        let _ = update_job(&workspace, &thread_project, &job_id, |state| {
            state.status = "running".to_string();
            state.phase = "rendering".to_string();
            state.progress = 10;
            state.message = format!("Rendering story shot with {}.", thread_shot.selected_engine);
            state.error = None;
        });

        let result = render_shot(
            &app,
            &workspace,
            &thread_project,
            &thread_plan,
            &thread_shot,
            cancel.as_ref(),
        );

        match result {
            Ok(result) => {
                let _ = update_job(&workspace, &thread_project, &job_id, |state| {
                    state.status = "completed".to_string();
                    state.phase = "ready".to_string();
                    state.progress = 100;
                    state.message =
                        "Story shot rendered and registered in the content-hash cache.".to_string();
                    state.result = Some(result);
                    state.error = None;
                });
            }
            Err(error)
                if cancel.load(Ordering::Relaxed) || error == "Story shot render cancelled." =>
            {
                let _ = update_job(&workspace, &thread_project, &job_id, |state| {
                    state.status = "cancelled".to_string();
                    state.phase = "cancelled".to_string();
                    state.message = "Story shot render cancelled.".to_string();
                    state.error = None;
                });
            }
            Err(error) => {
                let detail = error.clone();
                let _ = update_job(&workspace, &thread_project, &job_id, |state| {
                    state.status = "failed".to_string();
                    state.phase = "failed".to_string();
                    state.message = "Story shot render failed.".to_string();
                    state.error = Some(detail);
                });
            }
        }
    });

    Ok(job)
}

pub(crate) fn get_shot_render(
    workspace: &Workspace,
    project_id: &str,
    job_id: &str,
) -> Result<StoryShotRenderJob, String> {
    let project = video_production::get_project(workspace, project_id)?;
    {
        let guard = jobs()
            .lock()
            .map_err(|_| "Story render job state is unavailable.".to_string())?;
        if let Some(runtime) = guard.get(job_id) {
            if runtime.job.project_id != project.id {
                return Err("Story render job belongs to another Video Project.".to_string());
            }
            return Ok(runtime.job.clone());
        }
    }
    let mut job = load_job(workspace, &project, job_id)?;
    if matches!(job.status.as_str(), "queued" | "running") {
        job.status = "interrupted".to_string();
        job.phase = "interrupted".to_string();
        job.message =
            "This persisted story render job is not active in the current RepoTunnel session; start the same shot again safely."
                .to_string();
    }
    Ok(job)
}

pub(crate) fn cancel_shot_render(
    workspace: &Workspace,
    project_id: &str,
    job_id: &str,
) -> Result<StoryShotRenderJob, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let (cancel, terminal) = {
        let mut guard = jobs()
            .lock()
            .map_err(|_| "Story render job state is unavailable.".to_string())?;
        let runtime = guard.get_mut(job_id).ok_or_else(|| {
            "Story render job is not active in this RepoTunnel session.".to_string()
        })?;
        if runtime.job.project_id != project.id {
            return Err("Story render job belongs to another Video Project.".to_string());
        }
        let terminal = matches!(
            runtime.job.status.as_str(),
            "completed" | "failed" | "cancelled"
        );
        if !terminal {
            runtime.job.message = "Cancelling story shot render…".to_string();
            runtime.job.updated_at = now_millis();
        }
        (Arc::clone(&runtime.cancel), terminal)
    };
    if !terminal {
        cancel.store(true, Ordering::Relaxed);
    }
    let job = get_shot_render(workspace, project_id, job_id)?;
    let _ = persist_job(workspace, &project, &job);
    Ok(job)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::json;

    use crate::{
        models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
        video_director::{
            StoryCamera, StoryCompiledShot, StoryDirectorPlan, StoryLocation, StoryShotInput,
        },
        video_production, video_scene,
    };

    use super::{
        animatic_card_scene, install_render_output, resolve_story_audio_cue,
        selected_engine_executable, BLENDER_PRODUCTION_SCRIPT, GODOT_PROJECT, GODOT_SCENE,
        GODOT_SCRIPT,
    };

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn smoke_dir(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "repotunnel-story-render-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn temp_workspace() -> (std::path::PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "repotunnel-story-animatic-{}-{nonce}-{counter}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            id: format!("story-animatic-test-{counter}"),
            name: "Story animatic test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        (root, workspace)
    }

    #[test]
    fn completed_render_replaces_previous_output_only_after_staged_file_exists() {
        let root = smoke_dir("atomic-output");
        let final_path = root.join("shot.mp4");
        let staged = root.join("shot.pending.mp4");
        fs::write(&final_path, b"previous").unwrap();
        fs::write(&staged, b"new-render").unwrap();

        install_render_output(&staged, &final_path, "test story render").unwrap();

        assert_eq!(fs::read(&final_path).unwrap(), b"new-render");
        assert!(!staged.exists());
        assert_eq!(
            fs::read_dir(&root).unwrap().filter_map(Result::ok).count(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn animatic_storyboard_card_passes_real_layout_preflight() {
        let (root, workspace) = temp_workspace();
        let project = video_production::create_project_with_mode(
            &workspace,
            "Story animatic layout",
            Some("story"),
            None,
            Some(854),
            Some(480),
            Some(12),
        )
        .unwrap();
        let shot = StoryCompiledShot {
            shot: StoryShotInput {
                id: "shot-1".to_string(),
                scene_id: "scene-1".to_string(),
                order: 1,
                duration_seconds: 4.0,
                location_id: "village".to_string(),
                location_variant: None,
                camera: StoryCamera {
                    shot_type: "medium".to_string(),
                    movement: "tracking".to_string(),
                    angle: "eye-level".to_string(),
                    framing: "two-shot".to_string(),
                },
                actors: vec![],
                props: vec![],
                ambience: vec![],
                foley: vec![],
                requested_engine: None,
                transition: None,
                notes: "The hero crosses the village square while the camera tracks the planned blocking."
                    .to_string(),
            },
            selected_engine: "blender-2.5d".to_string(),
            engine_reason: "test".to_string(),
            render_key: "0123456789abcdef-layout-test".to_string(),
        };
        let plan = StoryDirectorPlan {
            version: 1,
            action_library_version: 1,
            project_id: project.id.clone(),
            language: "en-US".to_string(),
            visual_style: "2.5d".to_string(),
            characters: vec![],
            locations: vec![StoryLocation {
                id: "village".to_string(),
                name: "Village Square".to_string(),
                description: String::new(),
                variants: vec![],
                entrance_anchors: vec![],
                interaction_anchors: vec![],
                camera_anchors: vec![],
                walkable_areas: vec![],
                asset_path: None,
            }],
            props: vec![],
            voice_cast: vec![],
            shots: vec![shot.clone()],
            content_hash: "0123456789abcdef-plan".to_string(),
            updated_at: 1,
        };
        let scene = animatic_card_scene(&project, &plan, &shot, 0);
        let layout = video_scene::validate_scene_layout(&workspace, &project.id, &scene).unwrap();
        assert!(layout.passed, "{:?}", layout.issues);
        assert_eq!(scene.duration_seconds, 1.0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn logical_story_audio_cue_resolves_registered_project_asset() {
        let (root, workspace) = temp_workspace();
        let project = video_production::create_project_with_mode(
            &workspace,
            "Story audio cue",
            Some("story"),
            None,
            Some(854),
            Some(480),
            Some(12),
        )
        .unwrap();
        let project_root = video_production::project_root(
            &workspace,
            &project,
            crate::access::AccessOperation::Write,
        )
        .unwrap();
        let audio_dir = project_root.join("assets/audio");
        fs::create_dir_all(&audio_dir).unwrap();
        let audio = audio_dir.join("room-tone.wav");
        fs::write(&audio, b"non-empty-test-audio").unwrap();

        let project = video_production::register_asset(
            &workspace,
            &project.id,
            "audio",
            "assets/audio/room-tone.wav",
            Some("room-tone"),
        )
        .unwrap();
        let resolved = resolve_story_audio_cue(&workspace, &project, "room-tone")
            .unwrap()
            .expect("registered logical cue should resolve");
        assert_eq!(resolved, audio);
        assert!(resolve_story_audio_cue(&workspace, &project, "missing-cue")
            .unwrap()
            .is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn story_engine_templates_are_nonempty_and_headless_ready() {
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("bpy.ops.render.render(animation=True)"));
        assert!(GODOT_PROJECT.contains("run/main_scene"));
        assert!(GODOT_SCENE.contains("Node2D"));
        assert!(GODOT_SCRIPT.contains("get_tree().quit()"));
    }

    #[test]
    fn blender_adapter_executes_assets_rigging_lipsync_and_camera_motion() {
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("scene.frame_end"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("keyframe_insert"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("FFMPEG"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("def import_asset"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("bpy.ops.import_scene.gltf"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("constraints.new(\"IK\")"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("setup_foot_ik"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("animate_lipsync"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("mouth_amplitudes"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("rhubarb_cues"));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("movement == \"orbit\""));
        assert!(BLENDER_PRODUCTION_SCRIPT.contains("aim(camera, end_target)"));
    }

    #[test]
    #[ignore = "real Blender smoke validation; run explicitly when Blender is installed"]
    fn real_blender_story_adapter_renders_video_when_available() {
        let Ok(blender) = selected_engine_executable("blender-grease-pencil") else {
            return;
        };
        let root = smoke_dir("blender");
        let output = root.join("shot.mp4");
        fs::write(root.join("render.py"), BLENDER_PRODUCTION_SCRIPT).unwrap();
        fs::write(
            root.join("shot.json"),
            serde_json::to_vec_pretty(&json!({
                "width": 320,
                "height": 180,
                "fps": 12,
                "duration": 0.5,
                "output": output.to_string_lossy(),
                "engine": "blender-grease-pencil",
                "actors": [{
                    "id": "hero",
                    "name": "Hero",
                    "action": "walk",
                    "endAnchor": "right"
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let status = Command::new(blender)
            .arg("--background")
            .arg("--python")
            .arg(root.join("render.py"))
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(fs::metadata(&output).unwrap().len() > 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "real Blender imported-asset validation; run explicitly when Blender is installed"]
    fn real_blender_story_adapter_imports_character_set_and_prop_assets() {
        let Ok(blender) = selected_engine_executable("blender-3d") else {
            return;
        };
        let root = smoke_dir("blender-assets");
        let character = root.join("character.blend");
        let location = root.join("location.blend");
        let prop = root.join("prop.blend");
        let generator = root.join("make_assets.py");
        fs::write(
            &generator,
            format!(
                r#"import bpy
from mathutils import Vector

def reset():
    bpy.ops.wm.read_factory_settings(use_empty=True)

def save_cube(path, name, scale):
    reset()
    bpy.ops.mesh.primitive_cube_add(size=1, location=(0, 0, scale[2]))
    obj = bpy.context.object
    obj.name = name
    obj.scale = scale
    bpy.ops.wm.save_as_mainfile(filepath=path)

def save_character(path):
    reset()
    bpy.ops.mesh.primitive_cube_add(size=1, location=(0, 0, 1.4))
    mesh = bpy.context.object
    mesh.name = "HeroMesh"
    mesh.scale = (0.5, 0.3, 1.0)
    bpy.ops.object.armature_add(enter_editmode=True, location=(0, 0, 0))
    arm = bpy.context.object
    arm.name = "HeroRig"
    eb = arm.data.edit_bones
    root_bone = eb[0]
    root_bone.name = "hips"
    root_bone.head = (0, 0, 0.2)
    root_bone.tail = (0, 0, 1.0)
    def bone(name, head, tail, parent=None):
        b = eb.new(name)
        b.head = head
        b.tail = tail
        b.parent = parent
        return b
    shin_l = bone("lowerleg.L", (-0.25, 0, 1.0), (-0.25, 0, 0.35), root_bone)
    bone("foot.L", (-0.25, 0, 0.35), (-0.25, -0.25, 0.12), shin_l)
    shin_r = bone("lowerleg.R", (0.25, 0, 1.0), (0.25, 0, 0.35), root_bone)
    bone("foot.R", (0.25, 0, 0.35), (0.25, -0.25, 0.12), shin_r)
    forearm_r = bone("forearm.R", (0.35, 0, 1.65), (0.75, 0, 1.55), root_bone)
    bone("hand.R", (0.75, 0, 1.55), (1.05, 0, 1.55), forearm_r)
    bpy.ops.object.mode_set(mode="OBJECT")
    bpy.ops.wm.save_as_mainfile(filepath=path)

save_character({character:?})
save_cube({location:?}, "VillageSet", (5.0, 4.0, 0.15))
save_cube({prop:?}, "ToolProp", (0.35, 0.20, 0.60))
"#
            ),
        )
        .unwrap();
        let generated = Command::new(&blender)
            .arg("--background")
            .arg("--python")
            .arg(&generator)
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(generated.success());
        assert!(character.is_file());
        assert!(location.is_file());
        assert!(prop.is_file());

        let output = root.join("shot.mp4");
        fs::write(root.join("render.py"), BLENDER_PRODUCTION_SCRIPT).unwrap();
        fs::write(
            root.join("shot.json"),
            serde_json::to_vec_pretty(&json!({
                "width": 320,
                "height": 180,
                "fps": 12,
                "duration": 0.5,
                "output": output.to_string_lossy(),
                "engine": "blender-3d",
                "location": {
                    "id": "village",
                    "name": "Village",
                    "assetPath": location.to_string_lossy()
                },
                "camera": {
                    "shotType": "medium",
                    "movement": "tracking",
                    "angle": "eye-level"
                },
                "props": [{
                    "id": "tool",
                    "name": "Tool",
                    "assetPath": prop.to_string_lossy()
                }],
                "actors": [{
                    "id": "hero",
                    "name": "Hero",
                    "action": "pick-up",
                    "targetId": "tool",
                    "handTarget": "right",
                    "assetPath": character.to_string_lossy(),
                    "lipSync": false
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let status = Command::new(blender)
            .arg("--background")
            .arg("--python")
            .arg(root.join("render.py"))
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(fs::metadata(&output).unwrap().len() > 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "real Godot 4 smoke validation; run explicitly when Godot is installed"]
    fn real_godot_story_adapter_renders_movie_when_available() {
        let Ok(godot) = selected_engine_executable("godot-2d") else {
            return;
        };
        let version = Command::new(&godot).arg("--version").output().unwrap();
        let version = String::from_utf8_lossy(&version.stdout);
        if !version.trim_start().starts_with('4') {
            return;
        }

        let root = smoke_dir("godot");
        let movie = root.join("shot.avi");
        fs::write(root.join("project.godot"), GODOT_PROJECT).unwrap();
        fs::write(root.join("main.tscn"), GODOT_SCENE).unwrap();
        fs::write(root.join("main.gd"), GODOT_SCRIPT).unwrap();
        fs::write(
            root.join("shot.json"),
            serde_json::to_vec_pretty(&json!({
                "width": 854,
                "height": 480,
                "fps": 12,
                "duration": 0.5,
                "location": { "name": "Room" },
                "actors": [{
                    "id": "hero",
                    "name": "Hero",
                    "action": "walk",
                    "endAnchor": "right",
                    "dialogue": null
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let status = Command::new(godot)
            .arg("--headless")
            .arg("--path")
            .arg(&root)
            .arg("--write-movie")
            .arg(&movie)
            .arg("--fixed-fps")
            .arg("12")
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(fs::metadata(&movie).unwrap().len() > 0);
        fs::remove_dir_all(root).unwrap();
    }
}
