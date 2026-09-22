use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    fs,
    hash::{Hash, Hasher},
    io::{self, Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{access::AccessOperation, models::Workspace, video, video_production};

const PREVIEW_CACHE_DIR: &str = "video-preview";
const PREVIEW_PROFILE_VERSION: u8 = 2;
const MAX_PREVIEW_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

fn preview_locks() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn preview_lock(path: &Path) -> Result<Arc<Mutex<()>>, String> {
    let mut locks = preview_locks()
        .lock()
        .map_err(|_| "Video preview coordination lock was poisoned.".to_string())?;
    Ok(locks
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

#[derive(Clone)]
struct PreviewHttpEntry {
    path: PathBuf,
    mime_type: String,
}

struct PreviewHttpServer {
    port: u16,
    entries: Arc<Mutex<HashMap<String, PreviewHttpEntry>>>,
}

fn preview_http_server_slot() -> &'static Mutex<Option<Arc<PreviewHttpServer>>> {
    static SERVER: OnceLock<Mutex<Option<Arc<PreviewHttpServer>>>> = OnceLock::new();
    SERVER.get_or_init(|| Mutex::new(None))
}

fn random_preview_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Could not create private video preview token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?.trim();
    let (start_text, end_text) = value.split_once('-')?;
    if size == 0 {
        return None;
    }
    if start_text.is_empty() {
        let suffix = end_text.parse::<u64>().ok()?.min(size);
        return Some((size.saturating_sub(suffix), size - 1));
    }
    let start = start_text.parse::<u64>().ok()?;
    if start >= size {
        return None;
    }
    let end = if end_text.is_empty() {
        size - 1
    } else {
        end_text.parse::<u64>().ok()?.min(size - 1)
    };
    (end >= start).then_some((start, end))
}

fn write_http_error(stream: &mut TcpStream, status: &str, extra: &str) {
    let body = status.as_bytes();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\nCross-Origin-Resource-Policy: cross-origin\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body);
}

fn serve_preview_request(
    mut stream: TcpStream,
    entries: Arc<Mutex<HashMap<String, PreviewHttpEntry>>>,
) {
    let mut request = [0_u8; 16 * 1024];
    let Ok(read) = stream.read(&mut request) else {
        return;
    };
    if read == 0 {
        return;
    }
    let request = String::from_utf8_lossy(&request[..read]);
    let mut lines = request.split("\r\n");
    let Some(request_line) = lines.next() else {
        return;
    };
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default();
    let path = request_parts.next().unwrap_or_default();

    if method == "OPTIONS" {
        let response = concat!(
            "HTTP/1.1 204 No Content\r\n",
            "Access-Control-Allow-Origin: *\r\n",
            "Access-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n",
            "Access-Control-Allow-Headers: Range\r\n",
            "Access-Control-Expose-Headers: Accept-Ranges, Content-Length, Content-Range\r\n",
            "Cross-Origin-Resource-Policy: cross-origin\r\n",
            "Connection: close\r\n\r\n"
        );
        let _ = stream.write_all(response.as_bytes());
        return;
    }

    if method != "GET" && method != "HEAD" {
        write_http_error(
            &mut stream,
            "405 Method Not Allowed",
            "Allow: GET, HEAD, OPTIONS\r\n",
        );
        return;
    }

    let Some(token) = path.strip_prefix("/media/") else {
        write_http_error(&mut stream, "404 Not Found", "");
        return;
    };
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        write_http_error(&mut stream, "404 Not Found", "");
        return;
    }

    let entry = {
        let Ok(entries) = entries.lock() else {
            write_http_error(&mut stream, "500 Internal Server Error", "");
            return;
        };
        entries.get(token).cloned()
    };
    let Some(entry) = entry else {
        write_http_error(&mut stream, "404 Not Found", "");
        return;
    };

    let Ok(metadata) = fs::metadata(&entry.path) else {
        write_http_error(&mut stream, "404 Not Found", "");
        return;
    };
    if !metadata.is_file() {
        write_http_error(&mut stream, "404 Not Found", "");
        return;
    }
    let size = metadata.len();

    let range_header = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("range")
            .then(|| value.trim().to_string())
    });

    let (status, start, end) = match range_header.as_deref() {
        Some(value) => match parse_range(value, size) {
            Some((start, end)) => ("206 Partial Content", start, end),
            None => {
                write_http_error(
                    &mut stream,
                    "416 Range Not Satisfiable",
                    &format!("Content-Range: bytes */{size}\r\n"),
                );
                return;
            }
        },
        None if size > 0 => ("200 OK", 0, size - 1),
        None => ("200 OK", 0, 0),
    };

    let content_length = if size == 0 { 0 } else { end - start + 1 };
    let mut headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {content_length}\r\nAccept-Ranges: bytes\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: Accept-Ranges, Content-Length, Content-Range\r\nCross-Origin-Resource-Policy: cross-origin\r\nCache-Control: no-store\r\nConnection: close\r\n",
        entry.mime_type
    );
    if status.starts_with("206") {
        headers.push_str(&format!("Content-Range: bytes {start}-{end}/{size}\r\n"));
    }
    headers.push_str("\r\n");
    if stream.write_all(headers.as_bytes()).is_err() || method == "HEAD" || size == 0 {
        return;
    }

    let Ok(mut file) = fs::File::open(&entry.path) else {
        return;
    };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return;
    }
    let mut limited = file.take(content_length);
    let _ = io::copy(&mut limited, &mut stream);
}

fn preview_http_server() -> Result<Arc<PreviewHttpServer>, String> {
    let slot = preview_http_server_slot();
    let mut server = slot
        .lock()
        .map_err(|_| "Video preview HTTP server lock was poisoned.".to_string())?;
    if let Some(server) = server.as_ref() {
        return Ok(server.clone());
    }

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("Could not bind private video preview server: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("Could not inspect private video preview server: {error}"))?
        .port();
    let entries = Arc::new(Mutex::new(HashMap::<String, PreviewHttpEntry>::new()));
    let entries_for_thread = entries.clone();
    thread::Builder::new()
        .name("repotunnel-video-preview-http".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let entries = entries_for_thread.clone();
                        let _ = thread::Builder::new()
                            .name("repotunnel-video-preview-client".to_string())
                            .spawn(move || serve_preview_request(stream, entries));
                    }
                    Err(_) => break,
                }
            }
        })
        .map_err(|error| format!("Could not start private video preview server: {error}"))?;

    let next = Arc::new(PreviewHttpServer { port, entries });
    *server = Some(next.clone());
    Ok(next)
}

fn register_preview_http_url(
    app: &AppHandle,
    path: &Path,
    mime_type: &str,
) -> Result<String, String> {
    let validated = validated_cached_preview(app, &path.to_string_lossy())?;
    let server = preview_http_server()?;
    let token = random_preview_token()?;
    server
        .entries
        .lock()
        .map_err(|_| "Video preview HTTP registry lock was poisoned.".to_string())?
        .insert(
            token.clone(),
            PreviewHttpEntry {
                path: validated,
                mime_type: mime_type.to_string(),
            },
        );
    Ok(format!("http://127.0.0.1:{}/media/{token}", server.port))
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoPreviewSource {
    pub(crate) project_id: String,
    pub(crate) video_path: String,
    pub(crate) playback_url: Option<String>,
    pub(crate) subtitle_path: Option<String>,
    pub(crate) subtitle_url: Option<String>,
    pub(crate) subtitle_language: Option<String>,
    pub(crate) mime_type: String,
    pub(crate) size_bytes: u64,
    pub(crate) created_at: u64,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn safe_component(value: &str) -> String {
    let value = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .take(96)
        .collect::<String>();
    if value.is_empty() {
        "item".to_string()
    } else {
        value
    }
}

fn preview_root(app: &AppHandle, workspace_id: &str, project_id: &str) -> Result<PathBuf, String> {
    let relative = format!(
        "{PREVIEW_CACHE_DIR}/{}/{}",
        safe_component(workspace_id),
        safe_component(project_id)
    );
    app.path()
        .resolve(relative, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve private video preview cache: {error}"))
}

fn protect_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create private video preview cache: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect private video preview cache: {error}"))?;
    }
    Ok(())
}

fn protect_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not protect private video preview file: {error}"))?;
    }
    Ok(())
}

fn copy_regular_file(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("Could not inspect Video Project preview source: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Video Project preview source must be a regular file.".to_string());
    }
    fs::copy(source, destination)
        .map_err(|error| format!("Could not prepare Video Project preview: {error}"))?;
    protect_file(destination)
}

fn project_media_path(
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    relative: &str,
) -> Result<PathBuf, String> {
    let prefix = format!("{}/", project.relative_path);
    if !relative.starts_with(&prefix) {
        return Err("Video Project preview path escaped its project root.".to_string());
    }
    video_production::resolve_project_path(
        workspace,
        project,
        relative,
        AccessOperation::Read,
        true,
    )
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn webview_safe_mp4(source: &Path) -> bool {
    let video = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name,pix_fmt",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(source)
        .output();
    let Ok(video) = video else {
        return false;
    };
    if !video.status.success() {
        return false;
    }
    let video_text = String::from_utf8_lossy(&video.stdout);
    let video_fields = video_text
        .lines()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if !video_fields.contains(&"h264") || !video_fields.contains(&"yuv420p") {
        return false;
    }

    let audio = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(source)
        .output();
    match audio {
        Ok(output) if output.status.success() => {
            let codec = String::from_utf8_lossy(&output.stdout).trim().to_string();
            codec.is_empty() || codec == "aac"
        }
        _ => false,
    }
}

fn run_ffmpeg_program(
    ffmpeg: &Path,
    source: &Path,
    output: &Path,
    kind: &str,
) -> Result<(), String> {
    let mut command = Command::new(ffmpeg);
    command.args(["-hide_banner", "-nostats", "-loglevel", "error", "-y"]);
    command.arg("-i").arg(source);
    match kind {
        "video-copy" => {
            command
                .args(["-map", "0:v:0", "-map", "0:a:0?"])
                .args(["-c", "copy", "-avoid_negative_ts", "make_zero"])
                .args(["-movflags", "+faststart"]);
        }
        "video" => {
            command
                .args(["-map", "0:v:0", "-map", "0:a:0?"])
                .args([
                    "-fflags",
                    "+genpts",
                    "-avoid_negative_ts",
                    "make_zero",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-crf",
                    "28",
                    "-profile:v",
                    "baseline",
                    "-level:v",
                    "4.1",
                ])
                .args([
                    "-vf",
                    "scale=w='min(1280,iw)':h='min(720,ih)':force_original_aspect_ratio=decrease,scale=trunc(iw/2)*2:trunc(ih/2)*2",
                    "-pix_fmt",
                    "yuv420p",
                    "-tag:v",
                    "avc1",
                    "-force_key_frames",
                    "expr:gte(t,n_forced*1)",
                    "-x264-params",
                    "keyint=30:min-keyint=1:scenecut=0:bframes=0:ref=1",
                ])
                .args(["-c:a", "aac", "-b:a", "96k", "-ar", "48000"])
                .args(["-max_muxing_queue_size", "2048", "-movflags", "+faststart"]);
        }
        "audio" => {
            command
                .args(["-vn", "-map", "0:a:0", "-c:a", "aac", "-b:a", "192k"])
                .args(["-movflags", "+faststart"]);
        }
        "subtitle" => {
            command.args(["-map", "0:s:0?", "-f", "webvtt"]);
        }
        _ => return Err("Unsupported preview conversion request.".to_string()),
    }
    command.arg(output);
    video::configure_background_command(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let result = command
        .output()
        .map_err(|error| format!("Could not convert Video Project preview: {error}"))?;
    if !result.status.success() {
        let mut detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
        if detail.len() > 2500 {
            detail.truncate(2500);
        }
        return Err(if detail.is_empty() {
            format!(
                "Video preview conversion exited with status {}.",
                result.status
            )
        } else {
            format!("Video preview conversion failed: {detail}")
        });
    }
    if !output.is_file()
        || fs::metadata(output)
            .map(|item| item.len() == 0)
            .unwrap_or(true)
    {
        return Err("Video preview conversion produced no usable output.".to_string());
    }
    protect_file(output)
}

fn run_ffmpeg(app: &AppHandle, source: &Path, output: &Path, kind: &str) -> Result<(), String> {
    let ffmpeg = video::ensure_ffmpeg_program(app)?;
    run_ffmpeg_program(&ffmpeg, source, output, kind)
}

fn preview_fingerprint(source: &Path) -> Result<u64, String> {
    let metadata = fs::metadata(source)
        .map_err(|error| format!("Could not inspect Video Project preview source: {error}"))?;
    let mut hasher = DefaultHasher::new();
    PREVIEW_PROFILE_VERSION.hash(&mut hasher);
    source.to_string_lossy().hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    Ok(hasher.finish())
}

fn preview_media(app: &AppHandle, source: &Path, root: &Path) -> Result<(PathBuf, String), String> {
    let ext = extension(source);
    let (kind, native, destination_ext, mime) = match ext.as_str() {
        "mp4" | "m4v" if webview_safe_mp4(source) => ("video-copy", false, "mp4", "video/mp4"),
        "mp4" | "m4v" | "webm" | "mov" | "mkv" | "avi" | "mpg" | "mpeg" | "wmv" | "3gp" | "ts"
        | "mts" | "m2ts" => ("video", false, "mp4", "video/mp4"),
        "wav" => ("audio", true, "wav", "audio/wav"),
        "mp3" => ("audio", true, "mp3", "audio/mpeg"),
        "m4a" => ("audio", true, "m4a", "audio/mp4"),
        "ogg" => ("audio", true, "ogg", "audio/ogg"),
        "opus" => ("audio", true, "opus", "audio/ogg"),
        "aac" | "flac" | "wma" => ("audio", false, "m4a", "audio/mp4"),
        "png" => ("image", true, "png", "image/png"),
        "jpg" | "jpeg" => ("image", true, "jpg", "image/jpeg"),
        "webp" => ("image", true, "webp", "image/webp"),
        "gif" => ("image", true, "gif", "image/gif"),
        "bmp" => ("image", true, "bmp", "image/bmp"),
        _ => {
            return Err(
                "This Video Project file does not have a supported built-in preview format."
                    .to_string(),
            )
        }
    };
    let fingerprint = preview_fingerprint(source)?;
    let destination = root.join(format!("preview-{fingerprint:016x}.{destination_ext}"));
    let lock = preview_lock(&destination)?;
    let _guard = lock
        .lock()
        .map_err(|_| "Video preview file lock was poisoned.".to_string())?;

    if !destination.is_file()
        || fs::metadata(&destination)
            .map(|item| item.len() == 0)
            .unwrap_or(true)
    {
        if native {
            copy_regular_file(source, &destination)?;
        } else {
            let temporary = root.join(format!(
                ".preview-{fingerprint:016x}.part.{destination_ext}"
            ));
            let _ = fs::remove_file(&temporary);
            run_ffmpeg(app, source, &temporary, kind)?;
            fs::rename(&temporary, &destination)
                .map_err(|error| format!("Could not finalize Video Project preview: {error}"))?;
            protect_file(&destination)?;
        }
    }
    Ok((destination, mime.to_string()))
}

fn subtitle_language(relative: &str) -> Option<String> {
    let stem = Path::new(relative).file_stem()?.to_str()?;
    let language = stem
        .rsplit_once('-')
        .map(|(value, _)| value)
        .unwrap_or(stem)
        .replace('_', "-");
    (!language.is_empty()).then_some(language)
}

fn prepare_subtitle(
    app: &AppHandle,
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    root: &Path,
) -> Result<(Option<PathBuf>, Option<String>), String> {
    let Some(relative) = project.current_subtitle.as_deref() else {
        return Ok((None, None));
    };
    let source = project_media_path(workspace, project, relative)?;
    let ext = extension(&source);
    let destination = root.join("captions.vtt");
    if ext == "vtt" {
        copy_regular_file(&source, &destination)?;
    } else if matches!(ext.as_str(), "srt" | "ass" | "ssa") {
        run_ffmpeg(app, &source, &destination, "subtitle")?;
    } else {
        return Ok((None, None));
    }
    Ok((Some(destination), subtitle_language(relative)))
}

fn prepare_relative(
    app: &AppHandle,
    workspace: &Workspace,
    project: &video_production::VideoProductionProject,
    relative: &str,
    include_subtitle: bool,
) -> Result<VideoPreviewSource, String> {
    let source = project_media_path(workspace, project, relative)?;
    let root = preview_root(app, &workspace.id, &project.id)?;
    protect_directory(&root)?;

    let (destination, mime_type) = preview_media(app, &source, &root)?;
    let (subtitle_path, subtitle_language) = if include_subtitle && mime_type.starts_with("video/")
    {
        prepare_subtitle(app, workspace, project, &root)?
    } else {
        (None, None)
    };

    let size_bytes = fs::metadata(&destination)
        .map_err(|error| format!("Could not inspect prepared Video Project preview: {error}"))?
        .len();

    let playback_url = if mime_type.starts_with("video/") || mime_type.starts_with("audio/") {
        Some(register_preview_http_url(app, &destination, &mime_type)?)
    } else {
        None
    };
    let subtitle_url = subtitle_path
        .as_ref()
        .map(|path| register_preview_http_url(app, path, "text/vtt"))
        .transpose()?;

    Ok(VideoPreviewSource {
        project_id: project.id.clone(),
        video_path: destination.to_string_lossy().into_owned(),
        playback_url,
        subtitle_path: subtitle_path.map(|path| path.to_string_lossy().into_owned()),
        subtitle_url,
        subtitle_language,
        mime_type,
        size_bytes,
        created_at: now_millis(),
    })
}

pub(crate) fn prepare_preview(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
) -> Result<VideoPreviewSource, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let relative = project
        .current_preview
        .as_deref()
        .or(project.final_export.as_deref())
        .or(project.latest_draft.as_deref())
        .ok_or_else(|| "Video Project does not have a media preview yet.".to_string())?;
    prepare_relative(app, workspace, &project, relative, true)
}

pub(crate) fn prepare_file_preview(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    relative_path: &str,
) -> Result<VideoPreviewSource, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let include_subtitle = project.current_preview.as_deref() == Some(relative_path);
    prepare_relative(app, workspace, &project, relative_path, include_subtitle)
}

fn validated_cached_preview(app: &AppHandle, preview_path: &str) -> Result<PathBuf, String> {
    let cache_root = app
        .path()
        .resolve(PREVIEW_CACHE_DIR, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve private video preview cache: {error}"))?;
    let cache_root = cache_root
        .canonicalize()
        .map_err(|error| format!("Could not inspect private video preview cache: {error}"))?;
    let candidate = PathBuf::from(preview_path);
    let metadata = fs::symlink_metadata(&candidate)
        .map_err(|error| format!("Could not inspect prepared video preview: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Prepared video preview must be a regular file.".to_string());
    }
    let candidate = candidate
        .canonicalize()
        .map_err(|error| format!("Could not resolve prepared video preview: {error}"))?;
    if !candidate.starts_with(&cache_root) {
        return Err(
            "Prepared video preview escaped RepoTunnel's private preview cache.".to_string(),
        );
    }
    Ok(candidate)
}

pub(crate) fn read_preview_chunk(
    app: &AppHandle,
    preview_path: &str,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, String> {
    if length == 0 || length > MAX_PREVIEW_CHUNK_BYTES {
        return Err(format!(
            "Video preview chunk size must be between 1 byte and {MAX_PREVIEW_CHUNK_BYTES} bytes."
        ));
    }

    let path = validated_cached_preview(app, preview_path)?;
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Could not inspect prepared video preview: {error}"))?;
    if offset >= metadata.len() {
        return Ok(Vec::new());
    }

    let remaining = metadata.len() - offset;
    let read_len = usize::try_from(length.min(remaining))
        .map_err(|_| "Video preview chunk is too large for this system.".to_string())?;
    let mut file = fs::File::open(&path)
        .map_err(|error| format!("Could not open prepared video preview: {error}"))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("Could not seek prepared video preview: {error}"))?;
    let mut buffer = vec![0_u8; read_len];
    file.read_exact(&mut buffer)
        .map_err(|error| format!("Could not read prepared video preview: {error}"))?;
    Ok(buffer)
}

pub(crate) fn clear_all(app: &AppHandle) {
    let Ok(root) = app
        .path()
        .resolve(PREVIEW_CACHE_DIR, BaseDirectory::AppData)
    else {
        return;
    };
    if root.is_dir() {
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::TcpStream,
        path::PathBuf,
        process::{Command, Stdio},
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::{
        parse_range, preview_http_server, preview_lock, random_preview_token, run_ffmpeg_program,
        safe_component, webview_safe_mp4, PreviewHttpEntry,
    };

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "repotunnel-video-preview-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn program_available(name: &str) -> bool {
        Command::new(name)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[test]
    fn preview_cache_components_cannot_escape_app_data() {
        assert_eq!(safe_component("../../project"), "project");
        assert_eq!(safe_component("workspace-123_A"), "workspace-123_A");
        assert_eq!(safe_component(""), "item");
    }

    #[test]
    fn preview_lock_is_shared_for_the_same_cached_output() {
        let root = temp_dir("lock");
        let path = root.join("preview.mp4");
        let first = preview_lock(&path).unwrap();
        let second = preview_lock(&path).unwrap();
        let other = preview_lock(&root.join("other.mp4")).unwrap();
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert!(!std::sync::Arc::ptr_eq(&first, &other));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn http_range_parser_handles_normal_open_and_suffix_ranges() {
        assert_eq!(parse_range("bytes=0-3", 10), Some((0, 3)));
        assert_eq!(parse_range("bytes=4-", 10), Some((4, 9)));
        assert_eq!(parse_range("bytes=-4", 10), Some((6, 9)));
        assert_eq!(parse_range("bytes=10-", 10), None);
        assert_eq!(parse_range("items=0-3", 10), None);
    }

    #[test]
    fn private_http_preview_server_supports_byte_ranges() {
        let root = temp_dir("http-range");
        let path = root.join("preview.mp4");
        fs::write(&path, b"0123456789abcdef").unwrap();

        let server = preview_http_server().unwrap();
        let token = random_preview_token().unwrap();
        server.entries.lock().unwrap().insert(
            token.clone(),
            PreviewHttpEntry {
                path: path.clone(),
                mime_type: "video/mp4".to_string(),
            },
        );

        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let request = format!(
            "GET /media/{token} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nRange: bytes=4-7\r\nConnection: close\r\n\r\n",
            server.port
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(response.contains("Accept-Ranges: bytes\r\n"));
        assert!(response.contains("Content-Range: bytes 4-7/16\r\n"));
        assert!(response.ends_with("\r\n\r\n4567"));

        thread::sleep(Duration::from_millis(20));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn webview_safe_mp4_is_remuxed_without_reencoding_when_tools_exist() {
        if !program_available("ffmpeg") || !program_available("ffprobe") {
            return;
        }

        let root = temp_dir("webview-safe-copy");
        let input = root.join("source.mp4");
        let output = root.join("preview.mp4");

        let generated = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=640x360:rate=30",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&input)
            .status()
            .unwrap();
        assert!(generated.success());
        assert!(webview_safe_mp4(&input));

        run_ffmpeg_program(
            PathBuf::from("ffmpeg").as_path(),
            &input,
            &output,
            "video-copy",
        )
        .unwrap();

        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=index,codec_name,codec_type,pix_fmt,width,height",
                "-show_entries",
                "format=duration",
                "-of",
                "json",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(probe.status.success());
        let text = String::from_utf8_lossy(&probe.stdout);
        assert!(text.contains("\"codec_name\": \"h264\""));
        assert!(text.contains("\"codec_name\": \"aac\""));
        assert!(text.contains("\"width\": 640"));
        assert!(text.contains("\"height\": 360"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn video_preview_is_transcoded_to_webview_safe_h264_aac_when_tools_exist() {
        if !program_available("ffmpeg") || !program_available("ffprobe") {
            return;
        }

        let root = temp_dir("webview-safe");
        let input = root.join("source.mp4");
        let output = root.join("preview.mp4");

        let generated = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=321x181:rate=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=44100",
                "-t",
                "3",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&input)
            .status()
            .unwrap();
        assert!(generated.success());

        run_ffmpeg_program(PathBuf::from("ffmpeg").as_path(), &input, &output, "video").unwrap();
        assert!(fs::metadata(&output).unwrap().len() > 1_000);

        let video_probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name,profile,pix_fmt,width,height",
                "-of",
                "default=nw=1",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(video_probe.status.success());
        let video = String::from_utf8_lossy(&video_probe.stdout);
        assert!(video.contains("codec_name=h264"));
        assert!(
            video.contains("profile=Constrained Baseline") || video.contains("profile=Baseline")
        );
        assert!(video.contains("pix_fmt=yuv420p"));
        assert!(video.contains("width=320"));
        assert!(video.contains("height=180"));

        let audio_probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=codec_name,sample_rate",
                "-of",
                "default=nw=1",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(audio_probe.status.success());
        let audio = String::from_utf8_lossy(&audio_probe.stdout);
        assert!(audio.contains("codec_name=aac"));
        assert!(audio.contains("sample_rate=48000"));

        let keyframe_probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-skip_frame",
                "nokey",
                "-show_entries",
                "frame=best_effort_timestamp_time",
                "-of",
                "csv=p=0",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(keyframe_probe.status.success());
        let keyframes = String::from_utf8_lossy(&keyframe_probe.stdout)
            .lines()
            .filter_map(|line| line.trim().trim_end_matches(',').parse::<f64>().ok())
            .collect::<Vec<_>>();
        assert!(
            keyframes.len() >= 3,
            "expected frequent preview keyframes: {keyframes:?}"
        );
        assert!(
            keyframes.windows(2).all(|pair| pair[1] - pair[0] <= 1.1),
            "preview keyframe gaps were too large: {keyframes:?}"
        );

        fs::remove_dir_all(root).unwrap();
    }
}
