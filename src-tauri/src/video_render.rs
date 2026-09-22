use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::{access::AccessOperation, models::Workspace, video, video_production};

const MAX_TIMELINE_CLIPS: usize = 240;
const MAX_CLIP_SECONDS: f64 = 60.0 * 60.0;
const MAX_TIMELINE_SECONDS: f64 = 6.0 * 60.0 * 60.0;

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
    pub(crate) version: u32,
    pub(crate) clips: Vec<VideoTimelineClip>,
    #[serde(default)]
    pub(crate) narration_path: Option<String>,
    #[serde(default)]
    pub(crate) subtitle_path: Option<String>,
    #[serde(default)]
    pub(crate) music_path: Option<String>,
    #[serde(default)]
    pub(crate) music_volume: Option<f64>,
    #[serde(default)]
    pub(crate) preserve_source_audio: bool,
    #[serde(default)]
    pub(crate) final_render: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoRenderResult {
    pub(crate) project_id: String,
    pub(crate) output_path: String,
    pub(crate) subtitle_path: Option<String>,
    pub(crate) clip_count: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) final_render: bool,
}

fn timeline_version() -> u32 {
    1
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
    Ok(())
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
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let result = command
        .output()
        .map_err(|error| format!("Could not normalize Video Project clip: {error}"))?;
    if !result.status.success() {
        let mut detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
        if detail.len() > 3000 {
            detail.truncate(3000);
        }
        return Err(if detail.is_empty() {
            format!("Clip normalization exited with status {}.", result.status)
        } else {
            format!("Clip normalization failed: {detail}")
        });
    }
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
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let result = command
        .output()
        .map_err(|error| format!("Could not concatenate Video Project clips: {error}"))?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("Video concatenation exited with status {}.", result.status)
        } else {
            format!("Video concatenation failed: {detail}")
        });
    }
    Ok(())
}

fn mux_audio(
    ffmpeg: &Path,
    video_path: &Path,
    narration: Option<&Path>,
    music: Option<&Path>,
    music_volume: f64,
    preserve_source_audio: bool,
    output: &Path,
) -> Result<(), String> {
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-nostats", "-loglevel", "error", "-y"]);
    command.arg("-i").arg(video_path);

    match (narration, music) {
        (Some(narration), Some(music)) => {
            command.arg("-i").arg(narration);
            command.arg("-stream_loop").arg("-1").arg("-i").arg(music);
            let filter = format!(
                "[1:a]aresample=48000,volume=1.0[voice];[2:a]aresample=48000,volume={music_volume:.4}[music];[voice][music]amix=inputs=2:duration=first:dropout_transition=2:normalize=0[aout]"
            );
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
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let result = command
        .output()
        .map_err(|error| format!("Could not assemble Video Project audio: {error}"))?;
    if !result.status.success() {
        let mut detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
        if detail.len() > 3000 {
            detail.truncate(3000);
        }
        return Err(if detail.is_empty() {
            format!("Video/audio assembly exited with status {}.", result.status)
        } else {
            format!("Video/audio assembly failed: {detail}")
        });
    }
    if !output.is_file()
        || fs::metadata(output)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true)
    {
        return Err("Video assembly produced no usable output.".to_string());
    }
    Ok(())
}

fn render_with_ffmpeg(
    ffmpeg: &Path,
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    request: &VideoRenderRequest,
    output: &Path,
    render_dir: &Path,
) -> Result<(), String> {
    fs::create_dir(render_dir).map_err(|error| {
        format!("Could not create temporary Video Project render directory: {error}")
    })?;

    for (index, clip) in request.clips.iter().enumerate() {
        let source = project_owned_path(workspace, project, &clip.source_path)?;
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
        )?;
    }

    let joined = render_dir.join("joined.mp4");
    concat_clips(ffmpeg, render_dir, request.clips.len(), &joined)?;

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

    mux_audio(
        ffmpeg,
        &joined,
        narration.as_deref(),
        music.as_deref(),
        request.music_volume.unwrap_or(0.16).clamp(0.0, 1.0),
        request.preserve_source_audio,
        output,
    )
}

pub(crate) fn render_project(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    request: VideoRenderRequest,
) -> Result<VideoRenderResult, String> {
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

    let ffmpeg = video::ensure_ffmpeg_program(app)?;
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

    let result = render_with_ffmpeg(&ffmpeg, workspace, &project, &request, &output, &render_dir);
    let _ = fs::remove_dir_all(&render_dir);

    if let Err(error) = result {
        let _ = fs::remove_file(&output);
        let _ = video_production::update_project_status(
            workspace,
            project_id,
            "failed",
            Some(&format!("Video timeline render failed: {error}")),
        );
        return Err(error);
    }

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

    video_production::set_render_outputs(
        workspace,
        project_id,
        Some(output_asset),
        (!request.final_render).then_some(output_asset),
        request.final_render.then_some(output_asset),
        subtitle_path
            .as_deref()
            .and_then(|value| value.strip_prefix(&prefix)),
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

    Ok(VideoRenderResult {
        project_id: project.id,
        output_path: output_relative,
        subtitle_path,
        clip_count: request.clips.len(),
        width: project.width,
        height: project.height,
        fps: project.fps.clamp(12, 60),
        final_render: request.final_render,
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

    use super::{render_with_ffmpeg, validate_request, VideoRenderRequest, VideoTimelineClip};

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
            music_path: None,
            music_volume: Some(0.15),
            preserve_source_audio: false,
            final_render: false,
        }
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
            music_path: None,
            music_volume: Some(0.15),
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
            music_path: None,
            music_volume: None,
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
