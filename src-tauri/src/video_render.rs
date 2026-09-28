use std::{
    collections::{HashMap, HashSet},
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
use sha2::{Digest, Sha256};
use tauri::AppHandle;

use crate::{
    access::AccessOperation, models::Workspace, video, video_director, video_production, video_qa,
};

const MAX_TIMELINE_CLIPS: usize = 240;
const MAX_CLIP_SECONDS: f64 = 60.0 * 60.0;
const MAX_TIMELINE_SECONDS: f64 = 6.0 * 60.0 * 60.0;
const MAX_RENDER_JOBS: usize = 64;
static RENDER_JOB_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static RENDER_JOBS: OnceLock<Mutex<HashMap<String, VideoRenderRuntime>>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoTimelineClip {
    pub(crate) source_path: String,
    #[serde(default)]
    pub(crate) start_seconds: Option<f64>,
    #[serde(default)]
    pub(crate) end_seconds: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoRenderRequest {
    #[serde(default = "timeline_version")]
    #[schemars(skip)]
    pub(crate) version: u32,
    pub(crate) clips: Vec<VideoTimelineClip>,
    #[serde(default)]
    pub(crate) narration_path: Option<String>,
    #[serde(default)]
    pub(crate) subtitle_path: Option<String>,
    #[serde(default = "default_caption_delivery")]
    pub(crate) caption_delivery: String,
    #[serde(default)]
    pub(crate) music_path: Option<String>,
    #[serde(default)]
    pub(crate) music_volume: Option<f64>,
    #[serde(default = "default_audio_mix_preset")]
    pub(crate) audio_mix_preset: String,
    #[serde(default)]
    pub(crate) preserve_source_audio: bool,
    #[serde(default)]
    pub(crate) final_render: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoRenderResult {
    pub(crate) project_id: String,
    pub(crate) output_path: String,
    pub(crate) subtitle_path: Option<String>,
    pub(crate) caption_delivery: String,
    pub(crate) clip_count: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) final_render: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoRenderJob {
    pub(crate) id: String,
    pub(crate) project_id: String,
    pub(crate) request_hash: String,
    pub(crate) status: String,
    pub(crate) phase: String,
    pub(crate) progress: u8,
    pub(crate) message: String,
    pub(crate) result: Option<VideoRenderResult>,
    pub(crate) error: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

struct VideoRenderRuntime {
    job: VideoRenderJob,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoCleanupRequest {
    #[serde(default)]
    pub(crate) apply: bool,
    #[serde(default = "default_true")]
    pub(crate) remove_temp: bool,
    #[serde(default = "default_true")]
    pub(crate) remove_obsolete_renders: bool,
    #[serde(default)]
    pub(crate) keep_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoCleanupCandidate {
    pub(crate) relative_path: String,
    pub(crate) kind: String,
    pub(crate) size_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoCleanupReport {
    pub(crate) project_id: String,
    pub(crate) applied: bool,
    pub(crate) candidates: Vec<VideoCleanupCandidate>,
    pub(crate) total_bytes: u64,
    pub(crate) deleted_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoPipelineStage {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) status: String,
    pub(crate) progress: u8,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoPipelineStatus {
    pub(crate) project_id: String,
    pub(crate) overall_progress: u8,
    pub(crate) current_stage: String,
    pub(crate) stages: Vec<VideoPipelineStage>,
}

fn default_true() -> bool {
    true
}

fn timeline_version() -> u32 {
    1
}

fn default_caption_delivery() -> String {
    "sidecar".to_string()
}

fn default_audio_mix_preset() -> String {
    "simple".to_string()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn validate_seconds(value: Option<f64>, label: &str) -> Result<Option<f64>, String> {
    match value {
        None => Ok(None),
        Some(value) if value.is_finite() && (0.0..=MAX_TIMELINE_SECONDS).contains(&value) => {
            Ok(Some(value))
        }
        Some(_) => Err(format!(
            "{label} is outside RepoTunnel's supported timeline range."
        )),
    }
}

fn validate_request(request: &VideoRenderRequest) -> Result<(), String> {
    if request.version != 1 {
        return Err("Unsupported Video Project timeline version.".to_string());
    }
    if request.clips.is_empty() {
        return Err("Video timeline must contain at least one clip.".to_string());
    }
    if request.clips.len() > MAX_TIMELINE_CLIPS {
        return Err(format!(
            "Video timeline is limited to {MAX_TIMELINE_CLIPS} clips."
        ));
    }
    for clip in &request.clips {
        let start = validate_seconds(clip.start_seconds, "Clip start")?.unwrap_or(0.0);
        let end = validate_seconds(clip.end_seconds, "Clip end")?;
        if let Some(end) = end {
            if end <= start {
                return Err("Clip end time must be after its start time.".to_string());
            }
            if end - start > MAX_CLIP_SECONDS {
                return Err("A single timeline clip cannot exceed one hour.".to_string());
            }
        }
    }
    if let Some(volume) = request.music_volume {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err("Background music volume must be between 0 and 1.".to_string());
        }
    }
    match request.caption_delivery.as_str() {
        "none" | "sidecar" | "embedded" | "burned" | "burned+sidecar" => {}
        _ => {
            return Err(
                "Caption delivery must be none, sidecar, embedded, burned, or burned+sidecar."
                    .to_string(),
            )
        }
    }
    if !matches!(
        request.audio_mix_preset.as_str(),
        "simple" | "voice-priority"
    ) {
        return Err("Audio mix preset must be simple or voice-priority.".to_string());
    }
    if matches!(
        request.caption_delivery.as_str(),
        "embedded" | "burned" | "burned+sidecar"
    ) && request.subtitle_path.is_none()
    {
        return Err(format!(
            "Caption delivery '{}' requires subtitlePath.",
            request.caption_delivery
        ));
    }
    Ok(())
}

fn run_ffmpeg_command(
    command: &mut Command,
    cancel: Option<&AtomicBool>,
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
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_end(&mut bytes);
        }
        bytes
    });

    let status = loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            video::terminate_child(&mut child);
            let _ = stderr_reader.join();
            return Err("Video render cancelled.".to_string());
        }
        match child
            .try_wait()
            .map_err(|error| format!("Could not inspect {label}: {error}"))?
        {
            Some(status) => break status,
            None => thread::sleep(Duration::from_millis(120)),
        }
    };

    let stderr = stderr_reader.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let mut detail = String::from_utf8_lossy(&stderr).trim().to_string();
    if detail.len() > 3000 {
        detail.truncate(3000);
    }
    Err(if detail.is_empty() {
        format!("{label} exited with status {status}.")
    } else {
        format!("{label} failed: {detail}")
    })
}

fn project_owned_path(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    relative: &str,
) -> Result<PathBuf, String> {
    let relative = relative.trim().replace('\\', "/");
    let prefix = format!("{}/", project.relative_path);
    if !relative.starts_with(&prefix) {
        return Err("Video timeline inputs must belong to the selected Video Project.".to_string());
    }
    let path = video_production::resolve_project_path(
        workspace,
        project,
        &relative,
        AccessOperation::Read,
        true,
    )?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect Video Project media: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Video timeline input must be a regular project-owned file.".to_string());
    }
    Ok(path)
}

#[derive(Clone, Copy)]
struct ClipNormalizeOptions {
    width: u32,
    height: u32,
    fps: u32,
    preserve_source_audio: bool,
}

fn normalize_clip(
    ffmpeg: &Path,
    source: &Path,
    output: &Path,
    clip: &VideoTimelineClip,
    options: ClipNormalizeOptions,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-nostats", "-loglevel", "error", "-y"]);
    if let Some(start) = clip.start_seconds {
        command.args(["-ss", &format!("{start:.3}")]);
    }
    command.arg("-i").arg(source);
    if let Some(end) = clip.end_seconds {
        let start = clip.start_seconds.unwrap_or(0.0);
        command.args(["-t", &format!("{:.3}", end - start)]);
    }
    let filter = format!(
        "scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black,fps={},setsar=1",
        options.width, options.height, options.width, options.height, options.fps
    );
    command.args(["-map", "0:v:0", "-vf", &filter, "-c:v", "libx264"]);
    if options.preserve_source_audio {
        command
            .args(["-map", "0:a:0?"])
            .args(["-c:a", "aac", "-b:a", "192k", "-ar", "48000"]);
    } else {
        command.arg("-an");
    }
    command
        .args(["-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p"])
        .args(["-movflags", "+faststart"])
        .arg(output);
    video::configure_background_command(&mut command);
    run_ffmpeg_command(&mut command, cancel, "Clip normalization")?;
    if !output.is_file()
        || fs::metadata(output)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true)
    {
        return Err("Clip normalization produced no usable video.".to_string());
    }
    Ok(())
}

fn concat_clips(
    ffmpeg: &Path,
    render_dir: &Path,
    count: usize,
    output: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let concat_path = render_dir.join("concat.txt");
    let mut concat = String::new();
    for index in 0..count {
        concat.push_str(&format!("file 'clip-{index:04}.mp4'\n"));
    }
    fs::write(&concat_path, concat)
        .map_err(|error| format!("Could not write Video Project concat list: {error}"))?;

    let mut command = Command::new(ffmpeg);
    command
        .current_dir(render_dir)
        .args([
            "-hide_banner",
            "-nostats",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "concat",
            "-safe",
            "1",
            "-i",
            "concat.txt",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
        ])
        .arg(output);
    video::configure_background_command(&mut command);
    run_ffmpeg_command(&mut command, cancel, "Video concatenation")
}

#[allow(clippy::too_many_arguments)]
fn mux_audio(
    ffmpeg: &Path,
    video_path: &Path,
    narration: Option<&Path>,
    music: Option<&Path>,
    music_volume: f64,
    audio_mix_preset: &str,
    preserve_source_audio: bool,
    output: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-nostats", "-loglevel", "error", "-y"]);
    command.arg("-i").arg(video_path);

    match (narration, music) {
        (Some(narration), Some(music)) => {
            command.arg("-i").arg(narration);
            command.arg("-stream_loop").arg("-1").arg("-i").arg(music);
            let filter = if audio_mix_preset == "voice-priority" {
                format!(
                    "[1:a]aresample=48000,volume=1.0,asplit=2[voice_mix][voice_side];[2:a]aresample=48000,volume={music_volume:.4}[music];[music][voice_side]sidechaincompress=threshold=0.03:ratio=8:attack=20:release=350[ducked];[voice_mix][ducked]amix=inputs=2:duration=first:dropout_transition=2:normalize=0,loudnorm=I=-16:TP=-1.5:LRA=11[aout]"
                )
            } else {
                format!(
                    "[1:a]aresample=48000,volume=1.0[voice];[2:a]aresample=48000,volume={music_volume:.4}[music];[voice][music]amix=inputs=2:duration=first:dropout_transition=2:normalize=0[aout]"
                )
            };
            command
                .args([
                    "-filter_complex",
                    &filter,
                    "-map",
                    "0:v:0",
                    "-map",
                    "[aout]",
                ])
                .args(["-c:v", "copy", "-c:a", "aac", "-b:a", "192k", "-shortest"]);
        }
        (Some(narration), None) => {
            command.arg("-i").arg(narration);
            if audio_mix_preset == "voice-priority" {
                command
                    .args([
                        "-filter_complex",
                        "[1:a]aresample=48000,loudnorm=I=-16:TP=-1.5:LRA=11[aout]",
                        "-map",
                        "0:v:0",
                        "-map",
                        "[aout]",
                    ])
                    .args(["-c:v", "copy", "-c:a", "aac", "-b:a", "192k", "-shortest"]);
            } else {
                command.args(["-map", "0:v:0", "-map", "1:a:0"]).args([
                    "-c:v",
                    "copy",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "192k",
                    "-shortest",
                ]);
            }
        }
        (None, Some(music)) => {
            command.arg("-stream_loop").arg("-1").arg("-i").arg(music);
            command.args(["-map", "0:v:0", "-map", "1:a:0"]).args([
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-shortest",
            ]);
        }
        (None, None) => {
            if preserve_source_audio {
                command.args([
                    "-map", "0:v:0", "-map", "0:a:0?", "-c:v", "copy", "-c:a", "copy",
                ]);
            } else {
                command.args(["-map", "0:v:0", "-c:v", "copy", "-an"]);
            }
        }
    }
    command.args(["-movflags", "+faststart"]).arg(output);
    video::configure_background_command(&mut command);
    run_ffmpeg_command(&mut command, cancel, "Video/audio assembly")?;
    if !output.is_file()
        || fs::metadata(output)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true)
    {
        return Err("Video assembly produced no usable output.".to_string());
    }
    Ok(())
}

fn move_render_output(source: &Path, output: &Path) -> Result<(), String> {
    match fs::rename(source, output) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(source, output)
                .map_err(|error| format!("Could not finalize Video Project output: {error}"))?;
            fs::remove_file(source).map_err(|error| {
                format!("Could not remove temporary Video Project output: {error}")
            })?;
            Ok(())
        }
    }
}

fn subtitle_filter_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace(':', "\\:")
        .replace('\'', "\\'")
        .replace(',', "\\,")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

fn finalize_subtitle_delivery(
    ffmpeg: &Path,
    source: &Path,
    subtitle: Option<&Path>,
    delivery: &str,
    output: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<(), String> {
    if subtitle.is_none() || matches!(delivery, "none" | "sidecar") {
        return move_render_output(source, output);
    }
    let subtitle = subtitle.ok_or_else(|| "Subtitle input is unavailable.".to_string())?;
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-nostats", "-loglevel", "error", "-y"]);
    command.arg("-i").arg(source);

    match delivery {
        "embedded" => {
            command.arg("-i").arg(subtitle).args([
                "-map",
                "0:v:0",
                "-map",
                "0:a:0?",
                "-map",
                "1:0",
                "-c:v",
                "copy",
                "-c:a",
                "copy",
                "-c:s",
                "mov_text",
                "-metadata:s:s:0",
                "handler_name=RepoTunnel captions",
            ]);
        }
        "burned" | "burned+sidecar" => {
            let filter = format!("subtitles='{}'", subtitle_filter_path(subtitle));
            command.args(["-map", "0:v:0", "-map", "0:a:0?"]);
            command
                .args([
                    "-vf", &filter, "-c:v", "libx264", "-preset", "veryfast", "-crf", "18",
                ])
                .args(["-pix_fmt", "yuv420p", "-c:a", "copy"]);
        }
        _ => {
            return Err(
                "Caption delivery must be none, sidecar, embedded, burned, or burned+sidecar."
                    .to_string(),
            )
        }
    }

    command.args(["-movflags", "+faststart"]).arg(output);
    video::configure_background_command(&mut command);
    run_ffmpeg_command(&mut command, cancel, "Subtitle finalization")?;
    if !output.is_file()
        || fs::metadata(output)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true)
    {
        return Err("Subtitle finalization produced no usable output.".to_string());
    }
    Ok(())
}

#[cfg(test)]
fn render_with_ffmpeg(
    ffmpeg: &Path,
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    request: &VideoRenderRequest,
    output: &Path,
    render_dir: &Path,
) -> Result<(), String> {
    render_with_ffmpeg_controlled(
        ffmpeg,
        workspace,
        project,
        request,
        output,
        render_dir,
        None,
        |_, _| {},
    )
}

#[allow(clippy::too_many_arguments)]
fn render_with_ffmpeg_controlled<F>(
    ffmpeg: &Path,
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    request: &VideoRenderRequest,
    output: &Path,
    render_dir: &Path,
    cancel: Option<&AtomicBool>,
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(u8, &str),
{
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err("Video render cancelled.".to_string());
    }
    fs::create_dir(render_dir).map_err(|error| {
        format!("Could not create temporary Video Project render directory: {error}")
    })?;

    let clip_count = request.clips.len().max(1);
    for (index, clip) in request.clips.iter().enumerate() {
        let source = project_owned_path(workspace, project, &clip.source_path)?;
        let progress = 5 + (((index + 1) * 55) / clip_count) as u8;
        on_progress(progress.min(60), "normalizing");
        normalize_clip(
            ffmpeg,
            &source,
            &render_dir.join(format!("clip-{index:04}.mp4")),
            clip,
            ClipNormalizeOptions {
                width: project.width,
                height: project.height,
                fps: project.fps.clamp(12, 60),
                preserve_source_audio: request.preserve_source_audio,
            },
            cancel,
        )?;
    }

    on_progress(66, "assembling");
    let joined = render_dir.join("joined.mp4");
    concat_clips(ffmpeg, render_dir, request.clips.len(), &joined, cancel)?;

    let narration = request
        .narration_path
        .as_deref()
        .map(|path| project_owned_path(workspace, project, path))
        .transpose()?;
    let music = request
        .music_path
        .as_deref()
        .map(|path| project_owned_path(workspace, project, path))
        .transpose()?;

    on_progress(78, "audio");
    let subtitle = request
        .subtitle_path
        .as_deref()
        .map(|path| project_owned_path(workspace, project, path))
        .transpose()?;
    let muxed = render_dir.join("muxed.mp4");
    mux_audio(
        ffmpeg,
        &joined,
        narration.as_deref(),
        music.as_deref(),
        request.music_volume.unwrap_or(0.16).clamp(0.0, 1.0),
        &request.audio_mix_preset,
        request.preserve_source_audio,
        &muxed,
        cancel,
    )?;

    on_progress(90, "captions");
    finalize_subtitle_delivery(
        ffmpeg,
        &muxed,
        subtitle.as_deref(),
        &request.caption_delivery,
        output,
        cancel,
    )?;
    on_progress(96, "finalizing");
    Ok(())
}

pub(crate) fn render_project(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    request: VideoRenderRequest,
) -> Result<VideoRenderResult, String> {
    render_project_controlled(app, workspace, project_id, request, None, |_, _| {})
}

fn render_project_controlled<F>(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    request: VideoRenderRequest,
    cancel: Option<&AtomicBool>,
    mut on_progress: F,
) -> Result<VideoRenderResult, String>
where
    F: FnMut(u8, &str),
{
    validate_request(&request)?;
    let project = video_production::get_project(workspace, project_id)?;
    let subtitle_path = request
        .subtitle_path
        .as_deref()
        .map(|path| {
            project_owned_path(workspace, &project, path)?;
            Ok::<String, String>(path.to_string())
        })
        .transpose()?;

    on_progress(2, "preparing");
    let ffmpeg =
        video::ffmpeg_program(app, project.resource_policy.allow_automatic_package_install)?;
    let stamp = now_millis();
    let kind = if request.final_render {
        "final"
    } else {
        "drafts"
    };
    let filename = if request.final_render {
        format!("final-{stamp}.mp4")
    } else {
        format!("draft-{stamp}.mp4")
    };
    let output_relative = format!("{}/renders/{kind}/{filename}", project.relative_path);
    let output = video_production::resolve_project_path(
        workspace,
        &project,
        &output_relative,
        AccessOperation::Write,
        false,
    )?;
    if output.exists() {
        return Err("Video render output unexpectedly already exists.".to_string());
    }
    let render_dir_relative = format!("{}/timeline/.render-{stamp}", project.relative_path);
    let render_dir = video_production::resolve_project_path(
        workspace,
        &project,
        &render_dir_relative,
        AccessOperation::Write,
        false,
    )?;

    video_production::update_project_status(
        workspace,
        project_id,
        "editing",
        Some("Assembling Video Project timeline."),
    )?;

    let result = render_with_ffmpeg_controlled(
        &ffmpeg,
        workspace,
        &project,
        &request,
        &output,
        &render_dir,
        cancel,
        |progress, phase| on_progress(progress, phase),
    );
    let _ = fs::remove_dir_all(&render_dir);

    if let Err(error) = result {
        let _ = fs::remove_file(&output);
        return Err(error);
    }

    on_progress(98, "registering");
    let prefix = format!("{}/", project.relative_path);
    let output_asset = output_relative
        .strip_prefix(&prefix)
        .ok_or_else(|| "Video render output escaped its Video Project.".to_string())?;
    video_production::register_asset(
        workspace,
        project_id,
        if request.final_render {
            "final-video"
        } else {
            "draft-video"
        },
        output_asset,
        Some(if request.final_render {
            "Final Video Project render"
        } else {
            "Video Project draft render"
        }),
    )?;

    let preview_subtitle = if matches!(
        request.caption_delivery.as_str(),
        "sidecar" | "burned+sidecar"
    ) {
        subtitle_path
            .as_deref()
            .and_then(|value| value.strip_prefix(&prefix))
    } else {
        None
    };
    video_production::set_render_outputs(
        workspace,
        project_id,
        Some(output_asset),
        (!request.final_render).then_some(output_asset),
        request.final_render.then_some(output_asset),
        preview_subtitle,
    )?;

    if !request.final_render {
        video_production::update_project_status(
            workspace,
            project_id,
            "review",
            Some("Draft render completed and is ready for review."),
        )?;
    }

    let timeline_json = serde_json::to_string_pretty(&request)
        .map_err(|error| format!("Could not serialize rendered timeline: {error}"))?;
    video_production::write_document(workspace, project_id, "timeline", &timeline_json)?;

    on_progress(100, "ready");
    Ok(VideoRenderResult {
        project_id: project.id,
        output_path: output_relative,
        subtitle_path,
        caption_delivery: request.caption_delivery.clone(),
        clip_count: request.clips.len(),
        width: project.width,
        height: project.height,
        fps: project.fps.clamp(12, 60),
        final_render: request.final_render,
    })
}

fn render_jobs() -> &'static Mutex<HashMap<String, VideoRenderRuntime>> {
    RENDER_JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn new_render_job_id() -> String {
    format!(
        "video-render-{}-{:x}",
        now_millis(),
        RENDER_JOB_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn render_job_directory(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    operation: AccessOperation,
) -> Result<PathBuf, String> {
    let relative = format!("{}/qa/render-jobs", project.relative_path);
    video_production::resolve_project_path(workspace, project, &relative, operation, false)
}

fn persist_render_job(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    job: &VideoRenderJob,
) -> Result<(), String> {
    let directory = render_job_directory(workspace, project, AccessOperation::Write)?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not prepare Video Project render jobs: {error}"))?;
    let path = directory.join(format!("{}.json", job.id));
    let temp = directory.join(format!(".{}.tmp", job.id));
    let bytes = serde_json::to_vec_pretty(job)
        .map_err(|error| format!("Could not serialize Video Project render job: {error}"))?;
    fs::write(&temp, bytes)
        .map_err(|error| format!("Could not persist Video Project render job: {error}"))?;
    fs::rename(&temp, &path)
        .map_err(|error| format!("Could not finalize Video Project render job: {error}"))
}

fn load_persisted_render_jobs(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
) -> Result<Vec<VideoRenderJob>, String> {
    let directory = render_job_directory(workspace, project, AccessOperation::Read)?;
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let metadata = fs::symlink_metadata(&directory)
        .map_err(|error| format!("Could not inspect Video Project render jobs: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Video Project render-job storage is not a regular directory.".to_string());
    }
    let mut jobs = Vec::new();
    for entry in fs::read_dir(&directory)
        .map_err(|error| format!("Could not list Video Project render jobs: {error}"))?
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > 2 * 1024 * 1024
        {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(job) = serde_json::from_str::<VideoRenderJob>(&contents) {
            if job.project_id == project.id {
                jobs.push(job);
            }
        }
    }
    jobs.sort_by_key(|job| std::cmp::Reverse(job.updated_at));
    Ok(jobs)
}

fn output_for_job_exists(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    job: &VideoRenderJob,
) -> bool {
    job.result
        .as_ref()
        .and_then(|result| {
            video_production::resolve_project_path(
                workspace,
                project,
                &result.output_path,
                AccessOperation::Read,
                true,
            )
            .ok()
        })
        .is_some_and(|path| path.is_file())
}

fn hash_project_input(
    hasher: &mut Sha256,
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    relative: &str,
) -> Result<(), String> {
    let path = project_owned_path(workspace, project, relative)?;
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Could not inspect Video Project render input: {error}"))?;
    hasher.update(relative.as_bytes());
    hasher.update(metadata.len().to_le_bytes());
    if let Ok(modified) = metadata.modified() {
        if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
            hasher.update(duration.as_nanos().to_le_bytes());
        }
    }
    Ok(())
}

fn render_request_hash(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    request: &VideoRenderRequest,
) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hasher.update(project.id.as_bytes());
    hasher.update(project.width.to_le_bytes());
    hasher.update(project.height.to_le_bytes());
    hasher.update(project.fps.to_le_bytes());
    hasher.update(
        serde_json::to_vec(request)
            .map_err(|error| format!("Could not hash Video Project render request: {error}"))?,
    );
    for clip in &request.clips {
        hash_project_input(&mut hasher, workspace, project, &clip.source_path)?;
    }
    for path in [
        request.narration_path.as_deref(),
        request.subtitle_path.as_deref(),
        request.music_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        hash_project_input(&mut hasher, workspace, project, path)?;
    }
    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn update_render_job(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    job_id: &str,
    mutate: impl FnOnce(&mut VideoRenderJob),
) -> Result<VideoRenderJob, String> {
    let job = {
        let mut guard = render_jobs()
            .lock()
            .map_err(|_| "Video render job state is unavailable.".to_string())?;
        let runtime = guard.get_mut(job_id).ok_or_else(|| {
            "Video render job is no longer active in this RepoTunnel session.".to_string()
        })?;
        mutate(&mut runtime.job);
        runtime.job.updated_at = now_millis();
        runtime.job.clone()
    };
    persist_render_job(workspace, project, &job)?;
    Ok(job)
}

fn prune_render_jobs(guard: &mut HashMap<String, VideoRenderRuntime>) {
    if guard.len() < MAX_RENDER_JOBS {
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
    removable.sort_by_key(|(_, updated_at)| *updated_at);
    let count = guard.len().saturating_sub(MAX_RENDER_JOBS - 1);
    for (id, _) in removable.into_iter().take(count) {
        guard.remove(&id);
    }
}

pub(crate) fn start_render_job(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    request: VideoRenderRequest,
) -> Result<VideoRenderJob, String> {
    validate_request(&request)?;
    let project = video_production::get_project(workspace, project_id)?;
    let request_hash = render_request_hash(workspace, &project, &request)?;

    {
        let guard = render_jobs()
            .lock()
            .map_err(|_| "Video render job state is unavailable.".to_string())?;
        if let Some(existing) = guard.values().find(|runtime| {
            runtime.job.project_id == project.id
                && runtime.job.request_hash == request_hash
                && matches!(runtime.job.status.as_str(), "queued" | "running")
        }) {
            return Ok(existing.job.clone());
        }
        if let Some(existing) = guard.values().find(|runtime| {
            runtime.job.project_id == project.id
                && runtime.job.request_hash == request_hash
                && runtime.job.status == "completed"
                && output_for_job_exists(workspace, &project, &runtime.job)
        }) {
            return Ok(existing.job.clone());
        }
    }

    for mut existing in load_persisted_render_jobs(workspace, &project)? {
        if existing.request_hash != request_hash {
            continue;
        }
        if existing.status == "completed" && output_for_job_exists(workspace, &project, &existing) {
            return Ok(existing);
        }
        if matches!(existing.status.as_str(), "queued" | "running") {
            existing.status = "failed".to_string();
            existing.phase = "interrupted".to_string();
            existing.message =
                "Previous RepoTunnel session ended before this render job completed; a retry may start safely."
                    .to_string();
            existing.error =
                Some("Render job was interrupted by a RepoTunnel restart.".to_string());
            existing.updated_at = now_millis();
            let _ = persist_render_job(workspace, &project, &existing);
        }
        break;
    }

    let now = now_millis();
    let job = VideoRenderJob {
        id: new_render_job_id(),
        project_id: project.id.clone(),
        request_hash,
        status: "queued".to_string(),
        phase: "queued".to_string(),
        progress: 0,
        message: "Video render queued.".to_string(),
        result: None,
        error: None,
        created_at: now,
        updated_at: now,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut guard = render_jobs()
            .lock()
            .map_err(|_| "Video render job state is unavailable.".to_string())?;
        prune_render_jobs(&mut guard);
        guard.insert(
            job.id.clone(),
            VideoRenderRuntime {
                job: job.clone(),
                cancel: Arc::clone(&cancel),
            },
        );
    }
    persist_render_job(workspace, &project, &job)?;

    let app = app.clone();
    let workspace = workspace.clone();
    let project_id = project_id.to_string();
    let thread_project = project.clone();
    let job_id = job.id.clone();
    thread::spawn(move || {
        let _ = update_render_job(&workspace, &thread_project, &job_id, |state| {
            state.status = "running".to_string();
            state.phase = "preparing".to_string();
            state.progress = 1;
            state.message = "Video render started in the background.".to_string();
            state.error = None;
        });

        let result = render_project_controlled(
            &app,
            &workspace,
            &project_id,
            request,
            Some(cancel.as_ref()),
            |progress, phase| {
                let _ = update_render_job(&workspace, &thread_project, &job_id, |state| {
                    state.status = "running".to_string();
                    state.phase = phase.to_string();
                    state.progress = state.progress.max(progress);
                    state.message = format!("Video render: {phase}.");
                });
            },
        );

        match result {
            Ok(result) => {
                let final_render = result.final_render;
                let output_path = result.output_path.clone();
                let _ = update_render_job(&workspace, &thread_project, &job_id, |state| {
                    state.status = "completed".to_string();
                    state.phase = if final_render {
                        "qa".to_string()
                    } else {
                        "ready".to_string()
                    };
                    state.progress = 100;
                    state.message = if final_render {
                        "Final render completed; running QA automatically.".to_string()
                    } else {
                        "Draft render completed.".to_string()
                    };
                    state.result = Some(result);
                    state.error = None;
                });

                if final_render {
                    let qa_result =
                        video_qa::qa_project(&app, &workspace, &project_id, Some(&output_path));
                    let _ = update_render_job(&workspace, &thread_project, &job_id, |state| {
                        state.phase = "ready".to_string();
                        state.message = match qa_result {
                            Ok(report) if report.passed => {
                                "Final render completed and QA passed.".to_string()
                            }
                            Ok(_) => "Final render completed; QA found items that need attention."
                                .to_string(),
                            Err(error) => {
                                format!("Final render completed; automatic QA could not finish: {error}")
                            }
                        };
                    });
                }
            }
            Err(error) if cancel.load(Ordering::Relaxed) || error == "Video render cancelled." => {
                let _ = update_render_job(&workspace, &thread_project, &job_id, |state| {
                    state.status = "cancelled".to_string();
                    state.phase = "cancelled".to_string();
                    state.message = "Video render cancelled.".to_string();
                    state.error = None;
                });
            }
            Err(error) => {
                let message = error.clone();
                let _ = update_render_job(&workspace, &thread_project, &job_id, |state| {
                    state.status = "failed".to_string();
                    state.phase = "failed".to_string();
                    state.message = "Video render failed.".to_string();
                    state.error = Some(message);
                });
            }
        }
    });

    Ok(job)
}

pub(crate) fn get_render_job(
    workspace: &Workspace,
    project_id: &str,
    job_id: &str,
) -> Result<VideoRenderJob, String> {
    let project = video_production::get_project(workspace, project_id)?;
    {
        let guard = render_jobs()
            .lock()
            .map_err(|_| "Video render job state is unavailable.".to_string())?;
        if let Some(runtime) = guard.get(job_id) {
            if runtime.job.project_id != project.id {
                return Err("Video render job belongs to another Video Project.".to_string());
            }
            return Ok(runtime.job.clone());
        }
    }

    let mut job = load_persisted_render_jobs(workspace, &project)?
        .into_iter()
        .find(|job| job.id == job_id)
        .ok_or_else(|| "Video render job was not found.".to_string())?;
    if matches!(job.status.as_str(), "queued" | "running") {
        job.status = "interrupted".to_string();
        job.phase = "interrupted".to_string();
        job.message =
            "This persisted job is not active in the current RepoTunnel session; retry the same render request safely."
                .to_string();
    }
    Ok(job)
}

pub(crate) fn list_render_jobs(
    workspace: &Workspace,
    project_id: &str,
) -> Result<Vec<VideoRenderJob>, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let mut jobs = load_persisted_render_jobs(workspace, &project)?;
    let guard = render_jobs()
        .lock()
        .map_err(|_| "Video render job state is unavailable.".to_string())?;
    for job in &mut jobs {
        if matches!(job.status.as_str(), "queued" | "running") && !guard.contains_key(&job.id) {
            job.status = "interrupted".to_string();
            job.phase = "interrupted".to_string();
            job.message =
                "This persisted job is not active in the current RepoTunnel session; retry the same render request safely."
                    .to_string();
        }
    }
    for runtime in guard
        .values()
        .filter(|runtime| runtime.job.project_id == project.id)
    {
        if let Some(existing) = jobs.iter_mut().find(|job| job.id == runtime.job.id) {
            *existing = runtime.job.clone();
        } else {
            jobs.push(runtime.job.clone());
        }
    }
    jobs.sort_by_key(|job| std::cmp::Reverse(job.updated_at));
    Ok(jobs)
}

pub(crate) fn cancel_render_job(
    workspace: &Workspace,
    project_id: &str,
    job_id: &str,
) -> Result<VideoRenderJob, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let (cancel, terminal) = {
        let mut guard = render_jobs()
            .lock()
            .map_err(|_| "Video render job state is unavailable.".to_string())?;
        let runtime = guard.get_mut(job_id).ok_or_else(|| {
            "Video render job is not active in this RepoTunnel session.".to_string()
        })?;
        if runtime.job.project_id != project.id {
            return Err("Video render job belongs to another Video Project.".to_string());
        }
        let terminal = matches!(
            runtime.job.status.as_str(),
            "completed" | "failed" | "cancelled"
        );
        if !terminal {
            runtime.job.message = "Cancelling Video Project render…".to_string();
            runtime.job.updated_at = now_millis();
        }
        (Arc::clone(&runtime.cancel), terminal)
    };
    if !terminal {
        cancel.store(true, Ordering::Relaxed);
    }
    let job = get_render_job(workspace, project_id, job_id)?;
    let _ = persist_render_job(workspace, &project, &job);
    Ok(job)
}

fn bounded_tree_size(root: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    let mut visited = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        visited += 1;
        if visited > 20_000 {
            return Err("Video cleanup candidate contains too many entries.".to_string());
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Could not inspect Video cleanup candidate: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("Video cleanup refuses symbolic links.".to_string());
        }
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
            continue;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)
                .map_err(|error| format!("Could not inspect Video cleanup directory: {error}"))?
            {
                let entry = entry
                    .map_err(|error| format!("Could not inspect Video cleanup entry: {error}"))?;
                stack.push(entry.path());
            }
        }
    }
    Ok(total)
}

fn cleanup_candidate(
    project: &video_production::VideoProductionProject,
    inside: &str,
    kind: &str,
    path: &Path,
) -> Result<VideoCleanupCandidate, String> {
    Ok(VideoCleanupCandidate {
        relative_path: format!("{}/{}", project.relative_path, inside),
        kind: kind.to_string(),
        size_bytes: if kind == "directory" {
            bounded_tree_size(path)?
        } else {
            fs::metadata(path)
                .map_err(|error| format!("Could not inspect Video cleanup file: {error}"))?
                .len()
        },
    })
}

pub(crate) fn clean_project(
    workspace: &Workspace,
    project_id: &str,
    request: VideoCleanupRequest,
) -> Result<VideoCleanupReport, String> {
    let project = video_production::get_project(workspace, project_id)?;
    if request.apply
        && list_render_jobs(workspace, project_id)?
            .iter()
            .any(|job| matches!(job.status.as_str(), "queued" | "running"))
    {
        return Err(
            "Video Project cleanup is blocked while a render job is active. Cancel or finish the render first."
                .to_string(),
        );
    }

    let root = video_production::project_root(workspace, &project, AccessOperation::Write)?;
    let mut protected = HashSet::<String>::new();
    for value in [
        project.current_preview.as_deref(),
        project.latest_draft.as_deref(),
        project.final_export.as_deref(),
        project.current_subtitle.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        protected.insert(value.to_string());
    }
    for value in &request.keep_paths {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        video_production::resolve_project_path(
            workspace,
            &project,
            value,
            AccessOperation::Read,
            true,
        )?;
        protected.insert(value.to_string());
    }
    for scene in video_production::list_scenes(workspace, project_id).unwrap_or_default() {
        for value in [
            scene.animation_source.as_deref(),
            scene.rendered_clip.as_deref(),
            scene.narration_audio.as_deref(),
            scene.subtitle_path.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            protected.insert(value.to_string());
        }
    }

    let mut candidates = Vec::<VideoCleanupCandidate>::new();
    if request.remove_obsolete_renders {
        for bucket in ["renders/drafts", "renders/final"] {
            let directory = root.join(bucket);
            if !directory.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&directory)
                .map_err(|error| format!("Could not list Video render versions: {error}"))?
                .filter_map(Result::ok)
            {
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                    continue;
                };
                let inside = format!("{bucket}/{name}");
                let relative = format!("{}/{}", project.relative_path, inside);
                if protected.contains(&relative) {
                    continue;
                }
                candidates.push(cleanup_candidate(&project, &inside, "file", &path)?);
            }
        }
    }

    if request.remove_temp {
        for (parent, prefix) in [
            ("timeline", ".render-"),
            ("animations/generated", ".frames-"),
        ] {
            let directory = root.join(parent);
            if !directory.is_dir() {
                continue;
            }
            for entry in fs::read_dir(&directory)
                .map_err(|error| format!("Could not list Video temporary files: {error}"))?
                .filter_map(Result::ok)
            {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                    continue;
                };
                if !name.starts_with(prefix) {
                    continue;
                }
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    continue;
                }
                let inside = format!("{parent}/{name}");
                candidates.push(cleanup_candidate(&project, &inside, "directory", &path)?);
            }
        }
    }

    candidates.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let total_bytes = candidates.iter().fold(0u64, |total, candidate| {
        total.saturating_add(candidate.size_bytes)
    });

    let mut deleted_count = 0usize;
    if request.apply {
        let mut removed_files = Vec::<String>::new();
        for candidate in &candidates {
            let path = video_production::resolve_project_path(
                workspace,
                &project,
                &candidate.relative_path,
                AccessOperation::Write,
                true,
            )?;
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                format!("Could not revalidate Video cleanup candidate: {error}")
            })?;
            if metadata.file_type().is_symlink() {
                return Err("Video cleanup refuses symbolic links.".to_string());
            }
            match candidate.kind.as_str() {
                "file" if metadata.is_file() => {
                    fs::remove_file(&path).map_err(|error| {
                        format!("Could not remove obsolete Video render: {error}")
                    })?;
                    removed_files.push(candidate.relative_path.clone());
                    deleted_count += 1;
                }
                "directory" if metadata.is_dir() => {
                    fs::remove_dir_all(&path).map_err(|error| {
                        format!("Could not remove Video temporary directory: {error}")
                    })?;
                    deleted_count += 1;
                }
                _ => return Err(
                    "Video cleanup candidate changed type during revalidation; cleanup stopped."
                        .to_string(),
                ),
            }
        }
        if !removed_files.is_empty() {
            video_production::prune_asset_records(workspace, project_id, &removed_files)?;
        }
    }

    Ok(VideoCleanupReport {
        project_id: project.id,
        applied: request.apply,
        candidates,
        total_bytes,
        deleted_count,
    })
}

fn pipeline_stage(
    id: &str,
    label: &str,
    progress: u8,
    message: impl Into<String>,
    attention: bool,
) -> VideoPipelineStage {
    VideoPipelineStage {
        id: id.to_string(),
        label: label.to_string(),
        status: if attention {
            "attention".to_string()
        } else if progress >= 100 {
            "completed".to_string()
        } else if progress > 0 {
            "inProgress".to_string()
        } else {
            "pending".to_string()
        },
        progress,
        message: message.into(),
    }
}

fn story_pipeline_status(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
) -> Result<VideoPipelineStatus, String> {
    let project_id = project.id.as_str();
    let script = video_production::read_document(workspace, project_id, "script")?;
    let script_progress = u8::from(!script.content.trim().is_empty()) * 100;

    let plan = video_director::get_plan(workspace, project_id).ok();
    let animatic = video_director::get_animatic_plan(workspace, project_id).ok();
    let narrative_qa = video_director::get_narrative_qa(workspace, project_id).ok();

    let director_progress = u8::from(plan.is_some()) * 100;
    let cast_progress = plan
        .as_ref()
        .map(|plan| {
            u8::from(
                !plan.characters.is_empty()
                    || plan.shots.iter().all(|shot| shot.shot.actors.is_empty()),
            ) * 100
        })
        .unwrap_or(0);
    let sets_progress = plan
        .as_ref()
        .map(|plan| u8::from(!plan.locations.is_empty()) * 100)
        .unwrap_or(0);

    let (speaking_characters, voiced_speaking_characters) = plan
        .as_ref()
        .map(|plan| {
            let speakers = plan
                .shots
                .iter()
                .flat_map(|shot| shot.shot.actors.iter())
                .filter(|actor| {
                    actor
                        .dialogue
                        .as_deref()
                        .is_some_and(|text| !text.trim().is_empty())
                })
                .map(|actor| actor.character_id.as_str())
                .collect::<HashSet<_>>();
            let cast = plan
                .voice_cast
                .iter()
                .map(|entry| entry.character_id.as_str())
                .collect::<HashSet<_>>();
            let voiced = speakers
                .iter()
                .filter(|speaker| cast.contains(**speaker))
                .count();
            (speakers.len(), voiced)
        })
        .unwrap_or((0, 0));
    let voice_progress = if plan.is_none() {
        0
    } else {
        voiced_speaking_characters
            .saturating_mul(100)
            .checked_div(speaking_characters)
            .unwrap_or(100) as u8
    };

    let animatic_progress = u8::from(plan.as_ref().zip(animatic.as_ref()).is_some_and(
        |(plan, animatic)| {
            animatic.content_hash == plan.content_hash && animatic.shots.len() == plan.shots.len()
        },
    )) * 100;

    let planned_shots = plan.as_ref().map(|plan| plan.shots.len()).unwrap_or(0);
    let render_queue = video_director::get_render_queue(workspace, project_id).ok();
    let rendered_count = render_queue
        .as_ref()
        .map(|queue| {
            queue
                .entries
                .iter()
                .filter(|entry| entry.status == "ready" && entry.output_path.is_some())
                .count()
        })
        .unwrap_or(0);
    let shot_progress = (rendered_count * 100)
        .checked_div(planned_shots)
        .unwrap_or(0)
        .min(100) as u8;

    let render_jobs = list_render_jobs(workspace, project_id)?;
    let active_render = render_jobs
        .iter()
        .find(|job| matches!(job.status.as_str(), "queued" | "running"));
    let assembly_progress = if project.latest_draft.is_some() || project.final_export.is_some() {
        100
    } else {
        active_render.map(|job| job.progress).unwrap_or(0)
    };

    let narrative_qa_progress = narrative_qa
        .as_ref()
        .map(|report| if report.passed { 100 } else { 50 })
        .unwrap_or(0);
    let narrative_attention = narrative_qa.as_ref().is_some_and(|report| !report.passed);

    let final_qa_progress = if project.status == "completed" {
        100
    } else if project.final_export.is_some() {
        50
    } else {
        0
    };
    let export_progress = u8::from(project.final_export.is_some()) * 100;

    let stages = vec![
        pipeline_stage(
            "script",
            "Script",
            script_progress,
            if script_progress == 100 {
                "Story script is ready."
            } else {
                "Story script is still empty."
            },
            false,
        ),
        pipeline_stage(
            "director",
            "Scene Director",
            director_progress,
            plan.as_ref()
                .map(|plan| format!("{} shot(s) compiled with per-shot engine routing.", plan.shots.len()))
                .unwrap_or_else(|| "Story has not been compiled into shots yet.".to_string()),
            false,
        ),
        pipeline_stage(
            "cast",
            "Characters",
            cast_progress,
            plan.as_ref()
                .map(|plan| format!("{} reusable character(s) registered.", plan.characters.len()))
                .unwrap_or_else(|| "Character casting waits for the Scene Director plan.".to_string()),
            false,
        ),
        pipeline_stage(
            "sets",
            "Locations",
            sets_progress,
            plan.as_ref()
                .map(|plan| format!("{} reusable location set(s) registered.", plan.locations.len()))
                .unwrap_or_else(|| "Location sets wait for the Scene Director plan.".to_string()),
            false,
        ),
        pipeline_stage(
            "voice",
            "Voice Cast",
            voice_progress,
            format!(
                "{voiced_speaking_characters}/{speaking_characters} speaking character(s) have persistent voice assignments."
            ),
            false,
        ),
        pipeline_stage(
            "animatic",
            "Animatic",
            animatic_progress,
            if animatic_progress == 100 {
                "480p/12fps animatic timing plan matches the current shot plan.".to_string()
            } else {
                "Compile the current story plan before expensive rendering.".to_string()
            },
            false,
        ),
        pipeline_stage(
            "shots",
            "Shot Renders",
            shot_progress,
            format!("{rendered_count}/{planned_shots} planned shot render(s) are registered."),
            false,
        ),
        pipeline_stage(
            "assembly",
            "Assembly",
            assembly_progress,
            active_render
                .map(|job| job.message.clone())
                .unwrap_or_else(|| {
                    if assembly_progress == 100 {
                        "A story draft/final assembly is registered.".to_string()
                    } else {
                        "Story assembly has not started.".to_string()
                    }
                }),
            false,
        ),
        pipeline_stage(
            "narrativeQa",
            "Narrative QA",
            narrative_qa_progress,
            narrative_qa
                .as_ref()
                .map(|report| {
                    if report.passed {
                        "Story plan passes deterministic narrative QA.".to_string()
                    } else {
                        format!(
                            "Narrative QA found {} issue(s); blocking errors must be fixed before final completion.",
                            report.issues.len()
                        )
                    }
                })
                .unwrap_or_else(|| "Narrative QA waits for a compiled Scene Director plan.".to_string()),
            narrative_attention,
        ),
        pipeline_stage(
            "qa",
            "Final QA",
            final_qa_progress,
            if project.status == "completed" {
                "Final technical and narrative gates passed.".to_string()
            } else if project.attention_required {
                "Final QA requires attention.".to_string()
            } else if project.final_export.is_some() {
                "Final export exists and is awaiting completion gates.".to_string()
            } else {
                "Final QA begins after a final export is registered.".to_string()
            },
            project.attention_required,
        ),
        pipeline_stage(
            "export",
            "Export",
            export_progress,
            if project.final_export.is_some() {
                "Final export is registered.".to_string()
            } else {
                "No final export is registered yet.".to_string()
            },
            false,
        ),
    ];

    let overall_progress = (stages
        .iter()
        .map(|stage| usize::from(stage.progress))
        .sum::<usize>()
        / stages.len()) as u8;
    let current_stage = stages
        .iter()
        .find(|stage| stage.progress < 100 || stage.status == "attention")
        .map(|stage| stage.id.clone())
        .unwrap_or_else(|| "completed".to_string());

    Ok(VideoPipelineStatus {
        project_id: project.id.clone(),
        overall_progress,
        current_stage,
        stages,
    })
}

pub(crate) fn pipeline_status(
    workspace: &Workspace,
    project_id: &str,
) -> Result<VideoPipelineStatus, String> {
    let project = video_production::get_project(workspace, project_id)?;
    if project.production_mode == "story" {
        return story_pipeline_status(workspace, &project);
    }
    let script = video_production::read_document(workspace, project_id, "script")?;
    let scenes = video_production::list_scenes(workspace, project_id)?;

    let script_progress = u8::from(!script.content.trim().is_empty()) * 100;
    let scene_count = scenes.len();
    let storyboard_progress = u8::from(scene_count > 0) * 100;
    let voice_count = scenes
        .iter()
        .filter(|scene| scene.narration_audio.is_some())
        .count();
    let rendered_count = scenes
        .iter()
        .filter(|scene| scene.rendered_clip.is_some())
        .count();
    let voice_progress = (voice_count * 100).checked_div(scene_count).unwrap_or(0) as u8;
    let scene_progress = (rendered_count * 100).checked_div(scene_count).unwrap_or(0) as u8;

    let render_jobs = list_render_jobs(workspace, project_id)?;
    let active_render = render_jobs
        .iter()
        .find(|job| matches!(job.status.as_str(), "queued" | "running"));
    let assembly_progress = if project.latest_draft.is_some() || project.final_export.is_some() {
        100
    } else {
        active_render.map(|job| job.progress).unwrap_or(0)
    };

    let qa_progress = if project.status == "completed" {
        100
    } else if project.final_export.is_some() {
        50
    } else {
        0
    };
    let export_progress = u8::from(project.final_export.is_some()) * 100;

    let stages = vec![
        pipeline_stage(
            "script",
            "Script",
            script_progress,
            if script_progress == 100 {
                "Script is ready."
            } else {
                "Script is still empty."
            },
            false,
        ),
        pipeline_stage(
            "storyboard",
            "Storyboard",
            storyboard_progress,
            if scene_count > 0 {
                format!("{scene_count} scene record(s) planned.")
            } else {
                "No scene-centric production records yet.".to_string()
            },
            false,
        ),
        pipeline_stage(
            "voice",
            "Voice",
            voice_progress,
            format!("{voice_count}/{scene_count} scene narration asset(s) ready."),
            false,
        ),
        pipeline_stage(
            "scenes",
            "Scenes",
            scene_progress,
            format!("{rendered_count}/{scene_count} scene render(s) ready."),
            false,
        ),
        pipeline_stage(
            "assembly",
            "Assembly",
            assembly_progress,
            active_render
                .map(|job| job.message.clone())
                .unwrap_or_else(|| {
                    if assembly_progress == 100 {
                        "A draft/final timeline render is registered.".to_string()
                    } else {
                        "Timeline assembly has not started.".to_string()
                    }
                }),
            false,
        ),
        pipeline_stage(
            "qa",
            "QA",
            qa_progress,
            if project.status == "completed" {
                "Final deterministic QA passed.".to_string()
            } else if project.attention_required {
                "Final QA requires attention.".to_string()
            } else if project.final_export.is_some() {
                "Final export exists and is awaiting QA.".to_string()
            } else {
                "QA begins after a final export is registered.".to_string()
            },
            project.attention_required,
        ),
        pipeline_stage(
            "export",
            "Export",
            export_progress,
            if project.final_export.is_some() {
                "Final export is registered.".to_string()
            } else {
                "No final export is registered yet.".to_string()
            },
            false,
        ),
    ];

    let overall_progress = (stages
        .iter()
        .map(|stage| usize::from(stage.progress))
        .sum::<usize>()
        / stages.len()) as u8;
    let current_stage = stages
        .iter()
        .find(|stage| stage.progress < 100 || stage.status == "attention")
        .map(|stage| stage.id.clone())
        .unwrap_or_else(|| "completed".to_string());

    Ok(VideoPipelineStatus {
        project_id: project.id,
        overall_progress,
        current_stage,
        stages,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process::{Command, Stdio},
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        access::AccessOperation,
        models::{CommandPolicy, Workspace, WorkspaceAccessMode, WorkspaceChangePolicy},
        video_production,
    };

    use super::{
        clean_project, list_render_jobs, load_persisted_render_jobs, persist_render_job,
        pipeline_status, render_request_hash, render_with_ffmpeg, validate_request,
        VideoCleanupRequest, VideoRenderJob, VideoRenderRequest, VideoTimelineClip,
    };

    fn request() -> VideoRenderRequest {
        VideoRenderRequest {
            version: 1,
            clips: vec![VideoTimelineClip {
                source_path: "video-projects/demo/animations/generated/scene.mp4".to_string(),
                start_seconds: Some(0.0),
                end_seconds: Some(3.0),
            }],
            narration_path: None,
            subtitle_path: None,
            caption_delivery: "sidecar".to_string(),
            music_path: None,
            music_volume: Some(0.15),
            audio_mix_preset: "simple".to_string(),
            preserve_source_audio: false,
            final_render: false,
        }
    }

    #[test]
    fn pipeline_status_uses_durable_scene_state() {
        let (root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "Pipeline",
            None,
            Some(640),
            Some(360),
            Some(30),
        )
        .unwrap();
        video_production::write_document(
            &workspace,
            &project.id,
            "script",
            "# Tutorial\nExplain one concept clearly.",
        )
        .unwrap();
        video_production::upsert_scene(
            &workspace,
            &project.id,
            video_production::VideoProductionSceneInput {
                id: "scene-01".to_string(),
                order: 1,
                purpose: "Explain the concept".to_string(),
                teaching_point: "One clear teaching point".to_string(),
                narration: "This is the narration.".to_string(),
                captions_enabled: true,
                duration_seconds: Some(4.0),
                sources: Vec::new(),
            },
        )
        .unwrap();

        let status = pipeline_status(&workspace, &project.id).unwrap();
        assert_eq!(status.current_stage, "voice");
        assert_eq!(
            status
                .stages
                .iter()
                .find(|stage| stage.id == "script")
                .map(|stage| stage.progress),
            Some(100)
        );
        assert_eq!(
            status
                .stages
                .iter()
                .find(|stage| stage.id == "storyboard")
                .map(|stage| stage.progress),
            Some(100)
        );
        assert_eq!(
            status
                .stages
                .iter()
                .find(|stage| stage.id == "voice")
                .map(|stage| stage.progress),
            Some(0)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn timeline_validation_accepts_bounded_project_timeline() {
        validate_request(&request()).unwrap();
    }

    #[test]
    fn timeline_validation_rejects_reverse_trim() {
        let mut value = request();
        value.clips[0].start_seconds = Some(5.0);
        value.clips[0].end_seconds = Some(2.0);
        assert!(validate_request(&value).is_err());
    }

    #[test]
    fn timeline_validation_rejects_invalid_music_volume() {
        let mut value = request();
        value.music_volume = Some(1.5);
        assert!(validate_request(&value).is_err());
    }

    #[test]
    fn audio_mix_preset_is_explicit_and_bounded() {
        let mut value = request();
        value.audio_mix_preset = "voice-priority".to_string();
        assert!(validate_request(&value).is_ok());
        value.audio_mix_preset = "surprise-cloud-mastering".to_string();
        assert!(validate_request(&value).is_err());
    }

    #[test]
    fn caption_delivery_is_explicit_and_never_implies_burned_plus_embedded() {
        let mut value = request();
        value.caption_delivery = "embedded".to_string();
        assert!(validate_request(&value).is_err());

        value.subtitle_path = Some("video-projects/demo/subtitles/en-US.srt".to_string());
        assert!(validate_request(&value).is_ok());

        value.caption_delivery = "burned".to_string();
        assert!(validate_request(&value).is_ok());

        value.caption_delivery = "burned+embedded".to_string();
        assert!(validate_request(&value).is_err());
    }

    fn program_on_path(name: &str) -> Option<PathBuf> {
        env::var_os("PATH")
            .into_iter()
            .flat_map(|value| env::split_paths(&value).collect::<Vec<_>>())
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
    }

    fn temp_workspace() -> (PathBuf, Workspace) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "repotunnel-video-render-smoke-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            id: format!("render-{nonce}"),
            name: "Render smoke".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        (root, workspace)
    }

    #[test]
    fn render_hash_tracks_request_and_source_file_state() {
        let (root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "Hash",
            None,
            Some(640),
            Some(360),
            Some(12),
        )
        .unwrap();
        let project_root =
            video_production::project_root(&workspace, &project, AccessOperation::Write).unwrap();
        let source = project_root.join("animations/generated/source.mp4");
        fs::write(&source, b"first").unwrap();

        let mut value = request();
        value.clips[0].source_path =
            format!("{}/animations/generated/source.mp4", project.relative_path);
        let first = render_request_hash(&workspace, &project, &value).unwrap();
        let second = render_request_hash(&workspace, &project, &value).unwrap();
        assert_eq!(first, second);

        fs::write(&source, b"second-version").unwrap();
        let changed = render_request_hash(&workspace, &project, &value).unwrap();
        assert_ne!(first, changed);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn render_job_state_persists_inside_video_project() {
        let (root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "Jobs",
            None,
            Some(640),
            Some(360),
            Some(12),
        )
        .unwrap();
        let job = VideoRenderJob {
            id: "video-render-test".to_string(),
            project_id: project.id.clone(),
            request_hash: "abc123".to_string(),
            status: "running".to_string(),
            phase: "normalizing".to_string(),
            progress: 42,
            message: "Video render: normalizing.".to_string(),
            result: None,
            error: None,
            created_at: 1,
            updated_at: 2,
        };
        persist_render_job(&workspace, &project, &job).unwrap();
        let loaded = load_persisted_render_jobs(&workspace, &project).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, job.id);
        assert_eq!(loaded[0].progress, 42);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_dry_run_preserves_current_outputs_and_apply_removes_only_stale_versions() {
        let (root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "Cleanup",
            None,
            Some(640),
            Some(360),
            Some(12),
        )
        .unwrap();
        let project_root =
            video_production::project_root(&workspace, &project, AccessOperation::Write).unwrap();

        for relative in [
            "renders/final/current.mp4",
            "renders/final/old.mp4",
            "renders/drafts/current-draft.mp4",
            "renders/drafts/old-draft.mp4",
        ] {
            fs::write(project_root.join(relative), b"video").unwrap();
            video_production::register_asset(
                &workspace,
                &project.id,
                if relative.contains("/final/") {
                    "final-video"
                } else {
                    "draft-video"
                },
                relative,
                None,
            )
            .unwrap();
        }
        fs::create_dir_all(project_root.join("timeline/.render-old")).unwrap();
        fs::write(project_root.join("timeline/.render-old/chunk.mp4"), b"temp").unwrap();
        fs::create_dir_all(project_root.join("animations/generated/.frames-old")).unwrap();
        fs::write(
            project_root.join("animations/generated/.frames-old/frame.png"),
            b"temp",
        )
        .unwrap();

        video_production::set_render_outputs(
            &workspace,
            &project.id,
            Some("renders/final/current.mp4"),
            Some("renders/drafts/current-draft.mp4"),
            Some("renders/final/current.mp4"),
            None,
        )
        .unwrap();

        let dry = clean_project(
            &workspace,
            &project.id,
            VideoCleanupRequest {
                apply: false,
                remove_temp: true,
                remove_obsolete_renders: true,
                keep_paths: Vec::new(),
            },
        )
        .unwrap();
        assert!(!dry.applied);
        assert_eq!(dry.deleted_count, 0);
        assert!(dry
            .candidates
            .iter()
            .any(|item| item.relative_path.ends_with("renders/final/old.mp4")));
        assert!(dry
            .candidates
            .iter()
            .any(|item| item.relative_path.ends_with("renders/drafts/old-draft.mp4")));
        assert!(!dry
            .candidates
            .iter()
            .any(|item| item.relative_path.ends_with("renders/final/current.mp4")));
        assert!(project_root.join("renders/final/old.mp4").is_file());

        let applied = clean_project(
            &workspace,
            &project.id,
            VideoCleanupRequest {
                apply: true,
                remove_temp: true,
                remove_obsolete_renders: true,
                keep_paths: Vec::new(),
            },
        )
        .unwrap();
        assert!(applied.applied);
        assert_eq!(applied.deleted_count, 4);
        assert!(project_root.join("renders/final/current.mp4").is_file());
        assert!(project_root
            .join("renders/drafts/current-draft.mp4")
            .is_file());
        assert!(!project_root.join("renders/final/old.mp4").exists());
        assert!(!project_root.join("renders/drafts/old-draft.mp4").exists());
        assert!(!project_root.join("timeline/.render-old").exists());
        assert!(!project_root
            .join("animations/generated/.frames-old")
            .exists());

        let refreshed = video_production::get_project(&workspace, &project.id).unwrap();
        assert!(refreshed
            .assets
            .iter()
            .all(|asset| !asset.relative_path.ends_with("renders/final/old.mp4")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persisted_active_job_is_reported_interrupted_after_restart() {
        let (root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "Restart Jobs",
            None,
            Some(640),
            Some(360),
            Some(12),
        )
        .unwrap();
        let job = VideoRenderJob {
            id: "video-render-restart-test".to_string(),
            project_id: project.id.clone(),
            request_hash: "restart".to_string(),
            status: "running".to_string(),
            phase: "normalizing".to_string(),
            progress: 33,
            message: "Video render: normalizing.".to_string(),
            result: None,
            error: None,
            created_at: 1,
            updated_at: 2,
        };
        persist_render_job(&workspace, &project, &job).unwrap();

        let listed = list_render_jobs(&workspace, &project.id).unwrap();
        let recovered = listed
            .into_iter()
            .find(|candidate| candidate.id == job.id)
            .unwrap();
        assert_eq!(recovered.status, "interrupted");
        assert_eq!(recovered.phase, "interrupted");
        assert!(recovered.message.contains("retry"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn timeline_validation_requires_video() {
        let mut value = request();
        value.clips.clear();
        assert!(validate_request(&value).is_err());
    }

    #[test]
    fn ffmpeg_timeline_assembles_real_video_and_narration_when_tools_exist() {
        let Some(ffmpeg) = program_on_path("ffmpeg") else {
            return;
        };
        let Some(ffprobe) = program_on_path("ffprobe") else {
            return;
        };
        let (root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "Assembly smoke",
            None,
            Some(640),
            Some(360),
            Some(12),
        )
        .unwrap();

        let project_root =
            video_production::project_root(&workspace, &project, AccessOperation::Read).unwrap();
        let first = project_root.join("recordings/raw/first.mp4");
        let second = project_root.join("animations/generated/second.mp4");
        let narration = project_root.join("narration/test.wav");
        let music = project_root.join("assets/audio/music.wav");

        for (path, source) in [
            (&first, "testsrc2=size=640x360:rate=12"),
            (&second, "smptebars=size=640x360:rate=12"),
        ] {
            let status = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    source,
                    "-t",
                    "1",
                    "-an",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }

        let status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "2",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&narration)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        let music_status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=220:sample_rate=48000",
                "-t",
                "2",
                "-filter:a",
                "volume=0.7",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&music)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(music_status.success());

        let request = VideoRenderRequest {
            version: 1,
            clips: vec![
                VideoTimelineClip {
                    source_path: format!("{}/recordings/raw/first.mp4", project.relative_path),
                    start_seconds: None,
                    end_seconds: None,
                },
                VideoTimelineClip {
                    source_path: format!(
                        "{}/animations/generated/second.mp4",
                        project.relative_path
                    ),
                    start_seconds: None,
                    end_seconds: None,
                },
            ],
            narration_path: Some(format!("{}/narration/test.wav", project.relative_path)),
            subtitle_path: None,
            caption_delivery: "sidecar".to_string(),
            music_path: Some(format!("{}/assets/audio/music.wav", project.relative_path)),
            music_volume: Some(0.15),
            audio_mix_preset: "voice-priority".to_string(),
            preserve_source_audio: false,
            final_render: false,
        };
        let output = project_root.join("renders/drafts/smoke.mp4");
        let render_dir = project_root.join("timeline/.render-smoke");

        render_with_ffmpeg(
            &ffmpeg,
            &workspace,
            &project,
            &request,
            &output,
            &render_dir,
        )
        .unwrap();

        assert!(fs::metadata(&output).unwrap().len() > 10_000);
        let probe = Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "csv=p=0",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(probe.status.success());
        let streams = String::from_utf8_lossy(&probe.stdout);
        assert!(streams.lines().any(|line| line.trim() == "video"));
        assert!(streams.lines().any(|line| line.trim() == "audio"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn video_project_end_to_end_tutorial_pipeline_when_ffmpeg_exists() {
        let Some(ffmpeg) = program_on_path("ffmpeg") else {
            return;
        };
        let Some(ffprobe) = program_on_path("ffprobe") else {
            return;
        };

        let (root, workspace) = temp_workspace();
        let project = video_production::create_project(
            &workspace,
            "MCP vs API end-to-end",
            Some("16:9"),
            Some(640),
            Some(360),
            Some(12),
        )
        .unwrap();

        video_production::write_document(
            &workspace,
            &project.id,
            "script",
            "# MCP vs API\nAn API exposes operations. MCP standardizes how AI tools discover and call capabilities.",
        )
        .unwrap();
        video_production::write_document(
            &workspace,
            &project.id,
            "storyboard",
            r#"{"version":1,"scenes":[{"id":"concept","visual":"progressive technical diagram"}]}"#,
        )
        .unwrap();

        let project_root =
            video_production::project_root(&workspace, &project, AccessOperation::Read).unwrap();
        let scene = project_root.join("animations/generated/concept.mp4");
        let narration = project_root.join("narration/en-us-e2e.wav");

        let scene_status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=0x101820:size=640x360:rate=12",
                "-vf",
                "drawbox=x=70:y=120:w=180:h=100:color=0x4c9dff:t=4,drawbox=x=390:y=120:w=180:h=100:color=0x4bd28a:t=4,drawbox=x=250:y=165:w=140:h=10:color=white:t=fill",
                "-t",
                "2",
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&scene)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(scene_status.success());

        let narration_status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=330:sample_rate=48000",
                "-t",
                "2",
                "-c:a",
                "pcm_s16le",
            ])
            .arg(&narration)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(narration_status.success());

        video_production::register_asset(
            &workspace,
            &project.id,
            "generated-scene",
            "animations/generated/concept.mp4",
            Some("End-to-end concept scene"),
        )
        .unwrap();
        video_production::register_asset(
            &workspace,
            &project.id,
            "narration",
            "narration/en-us-e2e.wav",
            Some("End-to-end narration"),
        )
        .unwrap();

        let request = VideoRenderRequest {
            version: 1,
            clips: vec![VideoTimelineClip {
                source_path: format!("{}/animations/generated/concept.mp4", project.relative_path),
                start_seconds: None,
                end_seconds: None,
            }],
            narration_path: Some(format!("{}/narration/en-us-e2e.wav", project.relative_path)),
            subtitle_path: None,
            caption_delivery: "sidecar".to_string(),
            music_path: None,
            music_volume: None,
            audio_mix_preset: "simple".to_string(),
            preserve_source_audio: false,
            final_render: true,
        };
        let timeline_json = serde_json::to_string_pretty(&request).unwrap();
        video_production::write_document(&workspace, &project.id, "timeline", &timeline_json)
            .unwrap();

        let output = project_root.join("renders/final/e2e-final.mp4");
        let render_dir = project_root.join("timeline/.render-e2e");
        render_with_ffmpeg(
            &ffmpeg,
            &workspace,
            &project,
            &request,
            &output,
            &render_dir,
        )
        .unwrap();

        video_production::register_asset(
            &workspace,
            &project.id,
            "final-video",
            "renders/final/e2e-final.mp4",
            Some("End-to-end final tutorial render"),
        )
        .unwrap();
        video_production::set_render_outputs(
            &workspace,
            &project.id,
            Some("renders/final/e2e-final.mp4"),
            None,
            Some("renders/final/e2e-final.mp4"),
            None,
        )
        .unwrap();
        video_production::update_project_status(
            &workspace,
            &project.id,
            "completed",
            Some("End-to-end tutorial production validated."),
        )
        .unwrap();

        let reloaded = video_production::get_project(&workspace, &project.id).unwrap();
        assert_eq!(reloaded.status, "completed");
        let expected_output = format!("{}/renders/final/e2e-final.mp4", project.relative_path);
        assert_eq!(
            reloaded.current_preview.as_deref(),
            Some(expected_output.as_str())
        );
        assert_eq!(
            reloaded.final_export.as_deref(),
            Some(expected_output.as_str())
        );
        assert!(reloaded
            .assets
            .iter()
            .any(|asset| asset.kind == "final-video"));

        let probe = Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "csv=p=0",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(probe.status.success());
        let streams = String::from_utf8_lossy(&probe.stdout);
        assert!(streams.lines().any(|line| line.trim() == "video"));
        assert!(streams.lines().any(|line| line.trim() == "audio"));
        assert!(fs::metadata(&output).unwrap().len() > 10_000);

        fs::remove_dir_all(root).unwrap();
    }
}
