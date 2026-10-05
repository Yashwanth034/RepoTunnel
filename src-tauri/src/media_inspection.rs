use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use tauri::AppHandle;

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::{
        MediaDecodeValidation, MediaFrameExtraction, MediaInspection, MediaStreamInfo, Workspace,
    },
    temp_workspace, video,
};

static FRAME_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0)
}

fn regular_workspace_file(
    workspace: &Workspace,
    relative_path: &str,
) -> Result<(PathBuf, u64), String> {
    let path = resolve_workspace_path(workspace, relative_path, AccessOperation::Read, true)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect media file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Media inspection requires a regular non-symlink file.".to_string());
    }
    Ok((path, metadata.len()))
}

fn value_f64(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if let Some(number) = value.as_f64() {
        return number.is_finite().then_some(number);
    }
    value
        .as_str()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

fn value_u32(value: Option<&Value>) -> Option<u32> {
    let value = value?;
    if let Some(number) = value.as_u64() {
        return u32::try_from(number).ok();
    }
    value.as_str()?.trim().parse::<u32>().ok()
}

fn rational_fps(value: Option<&Value>) -> Option<f64> {
    let raw = value?.as_str()?.trim();
    if raw.is_empty() || raw == "0/0" {
        return None;
    }
    if let Some((numerator, denominator)) = raw.split_once('/') {
        let numerator = numerator.parse::<f64>().ok()?;
        let denominator = denominator.parse::<f64>().ok()?;
        if denominator == 0.0 {
            return None;
        }
        let fps = numerator / denominator;
        return fps.is_finite().then_some(fps);
    }
    raw.parse::<f64>().ok().filter(|fps| fps.is_finite())
}

fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn mime_type_for(path: &str, has_video: bool, has_audio: bool) -> String {
    match extension(path).as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "srt" => "application/x-subrip",
        "vtt" => "text/vtt",
        _ if has_video => "video/*",
        _ if has_audio => "audio/*",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn media_kind_for(path: &str, has_video: bool, has_audio: bool, has_subtitle: bool) -> String {
    if matches!(
        extension(path).as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "svg"
    ) {
        return "image".to_string();
    }
    if has_video {
        "video".to_string()
    } else if has_audio {
        "audio".to_string()
    } else if has_subtitle {
        "subtitle".to_string()
    } else {
        "other".to_string()
    }
}

fn stream_info(stream: &Value) -> MediaStreamInfo {
    let fps = rational_fps(stream.get("avg_frame_rate"))
        .or_else(|| rational_fps(stream.get("r_frame_rate")));
    MediaStreamInfo {
        index: value_u32(stream.get("index")).unwrap_or(0),
        codec_type: stream
            .get("codec_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        codec_name: stream
            .get("codec_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        width: value_u32(stream.get("width")),
        height: value_u32(stream.get("height")),
        fps,
        sample_rate: value_u32(stream.get("sample_rate")),
        channels: value_u32(stream.get("channels")),
        language: stream
            .get("tags")
            .and_then(|value| value.get("language"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

pub(crate) fn inspect(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: &str,
) -> Result<MediaInspection, String> {
    let (path, size_bytes) = regular_workspace_file(workspace, relative_path)?;
    let ffprobe = video::available_ffprobe_program(app).ok_or_else(|| {
        "FFprobe is not currently available. RepoTunnel will not install it automatically for generic media inspection."
            .to_string()
    })?;

    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-print_format",
            "json",
        ])
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Could not start FFprobe: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "FFprobe could not inspect this media file: {}",
            detail.trim().chars().take(2_000).collect::<String>()
        ));
    }
    let payload: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("FFprobe returned invalid JSON: {error}"))?;
    let raw_streams = payload
        .get("streams")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut video_streams = Vec::new();
    let mut audio_streams = Vec::new();
    let mut subtitle_streams = Vec::new();
    for stream in &raw_streams {
        let info = stream_info(stream);
        match info.codec_type.as_str() {
            "video" => video_streams.push(info),
            "audio" => audio_streams.push(info),
            "subtitle" => subtitle_streams.push(info),
            _ => {}
        }
    }

    let format = payload.get("format");
    let duration_seconds = format
        .and_then(|format| value_f64(format.get("duration")))
        .or_else(|| {
            raw_streams
                .iter()
                .filter_map(|stream| value_f64(stream.get("duration")))
                .fold(None, |best, value| {
                    Some(best.map_or(value, |existing: f64| existing.max(value)))
                })
        });
    let width = video_streams.first().and_then(|stream| stream.width);
    let height = video_streams.first().and_then(|stream| stream.height);
    let fps = video_streams.first().and_then(|stream| stream.fps);
    let has_video = !video_streams.is_empty();
    let has_audio = !audio_streams.is_empty();
    let has_subtitle = !subtitle_streams.is_empty();

    Ok(MediaInspection {
        relative_path: relative_path.to_string(),
        size_bytes,
        mime_type: mime_type_for(relative_path, has_video, has_audio),
        media_kind: media_kind_for(relative_path, has_video, has_audio, has_subtitle),
        format_name: format
            .and_then(|value| value.get("format_name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        duration_seconds,
        width,
        height,
        fps,
        video_streams,
        audio_streams,
        subtitle_streams,
    })
}

pub(crate) fn extract_frame(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: &str,
    task_id: &str,
    timestamp_seconds: f64,
) -> Result<MediaFrameExtraction, String> {
    if !timestamp_seconds.is_finite() || timestamp_seconds < 0.0 {
        return Err("Frame timestamp must be a finite value >= 0 seconds.".to_string());
    }
    let (source, _) = regular_workspace_file(workspace, relative_path)?;
    let ffmpeg = video::available_ffmpeg_program(app).ok_or_else(|| {
        "FFmpeg is not currently available. RepoTunnel will not install it automatically for generic frame extraction."
            .to_string()
    })?;
    let (directory, relative_directory) =
        temp_workspace::prepare_subdirectory(workspace, task_id, "frames")?;
    let file_name = format!(
        "frame-{}-{}.png",
        now_millis(),
        FRAME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let output_path = directory.join(&file_name);

    let output = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss"])
        .arg(format!("{timestamp_seconds:.6}"))
        .arg("-i")
        .arg(&source)
        .args(["-frames:v", "1", "-y"])
        .arg(&output_path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Could not start FFmpeg frame extraction: {error}"))?;
    if !output.status.success() || !output_path.is_file() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&output_path);
        return Err(format!(
            "FFmpeg could not extract the requested frame: {}",
            detail.trim().chars().take(2_000).collect::<String>()
        ));
    }
    let size_bytes = fs::metadata(&output_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    Ok(MediaFrameExtraction {
        source_relative_path: relative_path.to_string(),
        timestamp_seconds,
        relative_path: format!("{relative_directory}/{file_name}"),
        size_bytes,
    })
}

pub(crate) fn validate_decode(
    app: &AppHandle,
    workspace: &Workspace,
    relative_path: &str,
    check_seconds: Option<f64>,
) -> Result<MediaDecodeValidation, String> {
    let (path, _) = regular_workspace_file(workspace, relative_path)?;
    let ffmpeg = video::available_ffmpeg_program(app).ok_or_else(|| {
        "FFmpeg is not currently available. RepoTunnel will not install it automatically for generic decode validation."
            .to_string()
    })?;

    let checked_seconds = match check_seconds {
        Some(value) if !value.is_finite() || value <= 0.0 => {
            return Err("checkSeconds must be a finite value greater than 0.".to_string())
        }
        Some(value) => Some(value.min(600.0)),
        None => None,
    };
    let mut command = Command::new(ffmpeg);
    command
        .args(["-hide_banner", "-v", "error", "-xerror", "-nostdin", "-i"])
        .arg(&path);
    if let Some(seconds) = checked_seconds {
        command.arg("-t").arg(format!("{seconds:.6}"));
    }
    command
        .args(["-map", "0:v?", "-map", "0:a?", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .map_err(|error| format!("Could not start FFmpeg decode validation: {error}"))?;
    let error_excerpt = if output.status.success() {
        None
    } else {
        let detail = String::from_utf8_lossy(&output.stderr);
        Some(detail.trim().chars().take(4_000).collect::<String>())
    };

    Ok(MediaDecodeValidation {
        relative_path: relative_path.to_string(),
        passed: output.status.success(),
        checked_seconds,
        full_decode: checked_seconds.is_none(),
        error_excerpt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rational_frame_rate() {
        let value = Value::String("30000/1001".to_string());
        let fps = rational_fps(Some(&value)).unwrap();
        assert!((fps - 29.970_029).abs() < 0.001);
        assert!(rational_fps(Some(&Value::String("0/0".to_string()))).is_none());
    }

    #[test]
    fn media_kind_and_mime_are_extension_and_stream_aware() {
        assert_eq!(media_kind_for("frame.png", true, false, false), "image");
        assert_eq!(mime_type_for("frame.png", true, false), "image/png");
        assert_eq!(media_kind_for("clip.bin", true, true, false), "video");
        assert_eq!(mime_type_for("clip.bin", true, true), "video/*");
        assert_eq!(media_kind_for("voice.bin", false, true, false), "audio");
    }
}
