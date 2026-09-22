use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use url::{Host, Url};
use zip::ZipArchive;

use crate::{
    access::{resolve_workspace_path, AccessOperation},
    models::Workspace,
};

const VIDEO_CACHE_DIR: &str = "video-cache";
const VIDEO_TOOLS_DIR: &str = "video-tools";
const CACHE_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_HELPER_DOWNLOAD_BYTES: u64 = 260 * 1024 * 1024;
const MAX_LOCAL_MEDIA_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MAX_URL_LENGTH: usize = 8 * 1024;
const MAX_TRANSCRIPT_CHARS: usize = 240_000;
const MAX_FRAMES: usize = 18;
const DEFAULT_FRAMES: usize = 10;
const MAX_FRAME_BYTES: u64 = 3 * 1024 * 1024;
const MAX_AUDIO_TOTAL_BYTES: u64 = 24 * 1024 * 1024;
const MAX_AUDIO_FALLBACK_SECONDS: f64 = 60.0 * 60.0;
const MAX_FULL_VISUAL_SECONDS: f64 = 2.0 * 60.0 * 60.0;
const MAX_JOBS: usize = 50;
const COMMAND_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

static JOB_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static COMMAND_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static JOBS: OnceLock<Mutex<HashMap<String, VideoJobRuntime>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoToolState {
    pub(crate) available: bool,
    pub(crate) source: String,
    pub(crate) version: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoToolsStatus {
    pub(crate) ready: bool,
    pub(crate) yt_dlp: VideoToolState,
    pub(crate) ffmpeg: VideoToolState,
    pub(crate) cache_bytes: u64,
    pub(crate) cache_items: usize,
    pub(crate) cache_limit_bytes: u64,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoFrameInfo {
    pub(crate) index: usize,
    pub(crate) timestamp_seconds: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoAnalysisResult {
    pub(crate) job_id: String,
    pub(crate) cache_key: String,
    pub(crate) source: String,
    pub(crate) source_kind: String,
    pub(crate) mode: String,
    pub(crate) title: String,
    pub(crate) duration_seconds: Option<f64>,
    pub(crate) analysis_start_seconds: f64,
    pub(crate) analysis_end_seconds: Option<f64>,
    pub(crate) transcript: Option<String>,
    pub(crate) transcript_source: Option<String>,
    pub(crate) frames: Vec<VideoFrameInfo>,
    pub(crate) audio_chunk_count: usize,
    pub(crate) cache_hit: bool,
    pub(crate) completed_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoAnalysisJob {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) source: String,
    pub(crate) mode: String,
    pub(crate) status: String,
    pub(crate) phase: String,
    pub(crate) progress: u8,
    pub(crate) message: String,
    pub(crate) title: Option<String>,
    pub(crate) duration_seconds: Option<f64>,
    pub(crate) transcript_available: bool,
    pub(crate) frame_count: usize,
    pub(crate) audio_chunk_count: usize,
    pub(crate) cache_key: Option<String>,
    pub(crate) cache_hit: bool,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedVideoResult {
    result: VideoAnalysisResult,
    frame_files: Vec<String>,
    audio_files: Vec<String>,
}

pub(crate) struct VideoMcpPayload {
    pub(crate) result: VideoAnalysisResult,
    pub(crate) frames: Vec<(VideoFrameInfo, String, String)>,
    pub(crate) audio: Vec<(usize, String, String)>,
}

struct VideoJobRuntime {
    job: VideoAnalysisJob,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoMode {
    Transcript,
    Visual,
    Instruction,
    Full,
}

impl VideoMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "transcript" => Ok(Self::Transcript),
            "visual" => Ok(Self::Visual),
            "instruction" => Ok(Self::Instruction),
            "full" => Ok(Self::Full),
            _ => Err("Video mode must be transcript, visual, instruction, or full.".to_string()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::Visual => "visual",
            Self::Instruction => "instruction",
            Self::Full => "full",
        }
    }

    fn needs_frames(self) -> bool {
        true
    }

    fn needs_audio_fallback(self) -> bool {
        true
    }
}

enum VideoSource {
    Url(String),
    Local(PathBuf),
}

struct CommandCapture {
    success: bool,
    stdout: String,
    stderr: String,
}

#[derive(Clone)]
struct SourceMetadata {
    title: String,
    duration_seconds: Option<f64>,
}

struct YtDlpRuntime {
    path: PathBuf,
    refreshed_after_failure: bool,
}

#[derive(Clone, Copy)]
struct MediaRange {
    start: f64,
    end: Option<f64>,
}

#[derive(Clone, Copy)]
struct FrameExtractionPlan {
    source_pretrimmed: bool,
    duration_seconds: Option<f64>,
    max_frames: usize,
}

#[derive(Clone, Copy)]
struct AnalysisPlan {
    mode: VideoMode,
    range: MediaRange,
    max_frames: usize,
}

#[derive(Clone, Copy)]
struct UrlDownloadPlan {
    visual: bool,
    range: MediaRange,
}

fn jobs() -> &'static Mutex<HashMap<String, VideoJobRuntime>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn new_job_id() -> String {
    format!(
        "video-{:x}-{:x}",
        now_millis(),
        JOB_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn data_path(app: &AppHandle, relative: &str) -> Result<PathBuf, String> {
    app.path()
        .resolve(relative, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel video data: {error}"))
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create RepoTunnel video directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect RepoTunnel video directory: {error}"))?;
    }
    Ok(())
}

fn private_write(path: &Path, bytes: &[u8], executable: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel video helper directory.".to_string())?;
    ensure_private_dir(parent)?;
    let temporary = parent.join(format!(
        ".video-write-{:x}-{:x}.tmp",
        now_millis(),
        COMMAND_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Could not create a private video helper file: {error}"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("Could not save a video helper: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            &temporary,
            fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
        )
        .map_err(|error| format!("Could not protect a video helper: {error}"))?;
    }
    fs::rename(&temporary, path)
        .map_err(|error| format!("Could not install a video helper: {error}"))
}

fn cache_root(app: &AppHandle) -> Result<PathBuf, String> {
    let path = data_path(app, VIDEO_CACHE_DIR)?;
    ensure_private_dir(&path)?;
    Ok(path)
}

fn tools_root(app: &AppHandle) -> Result<PathBuf, String> {
    let path = data_path(app, VIDEO_TOOLS_DIR)?;
    ensure_private_dir(&path)?;
    Ok(path)
}

fn executable_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_value = env::var_os("PATH")?;
    let candidate_name = executable_name(name);
    env::split_paths(&path_value)
        .map(|directory| directory.join(&candidate_name))
        .find(|candidate| candidate.is_file())
}

fn managed_program(app: &AppHandle, name: &str) -> Option<PathBuf> {
    tools_root(app)
        .ok()
        .map(|root| root.join(executable_name(name)))
        .filter(|path| path.is_file())
}

fn program_path(app: &AppHandle, name: &str) -> Option<(PathBuf, &'static str)> {
    if let Some(path) = managed_program(app, name) {
        return Some((path, "managed"));
    }
    find_on_path(name).map(|path| (path, "system"))
}

fn program_version(path: &Path, name: &str) -> Option<String> {
    let arg = if name == "ffmpeg" {
        "-version"
    } else {
        "--version"
    };
    let output = Command::new(path)
        .arg(arg)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr)
    } else {
        String::from_utf8_lossy(&output.stdout)
    };
    text.lines().next().map(|line| line.trim().to_string())
}

fn tool_state(app: &AppHandle, name: &str) -> VideoToolState {
    if let Some((path, source)) = program_path(app, name) {
        VideoToolState {
            available: true,
            source: source.to_string(),
            version: program_version(&path, name),
        }
    } else {
        VideoToolState {
            available: false,
            source: "missing".to_string(),
            version: None,
        }
    }
}

fn directory_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => 0,
                Ok(metadata) if metadata.is_file() => metadata.len(),
                Ok(metadata) if metadata.is_dir() => directory_size(&path),
                _ => 0,
            }
        })
        .sum()
}

fn cache_summary(app: &AppHandle) -> Result<(u64, usize), String> {
    let root = cache_root(app)?;
    let mut bytes = 0_u64;
    let mut items = 0_usize;
    for entry in fs::read_dir(&root)
        .map_err(|error| format!("Could not inspect the video cache: {error}"))?
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.is_dir() {
            items = items.saturating_add(1);
            bytes = bytes.saturating_add(directory_size(&path));
        }
    }
    Ok((bytes, items))
}

pub(crate) fn tools_status(app: &AppHandle) -> Result<VideoToolsStatus, String> {
    let yt_dlp = tool_state(app, "yt-dlp");
    let ffmpeg = tool_state(app, "ffmpeg");
    let (cache_bytes, cache_items) = cache_summary(app)?;
    let ready = yt_dlp.available && ffmpeg.available;
    let message = if ready {
        "Video Intelligence is ready. RepoTunnel will use captions first, then smart frames/audio only when needed.".to_string()
    } else {
        "Missing video helpers will be downloaded privately and verified on first analysis. No terminal or admin install is required.".to_string()
    };
    Ok(VideoToolsStatus {
        ready,
        yt_dlp,
        ffmpeg,
        cache_bytes,
        cache_items,
        cache_limit_bytes: CACHE_LIMIT_BYTES,
        message,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn trusted_helper_host(host: &str) -> bool {
    matches!(
        host,
        "github.com"
            | "release-assets.githubusercontent.com"
            | "objects.githubusercontent.com"
            | "pypi.org"
            | "files.pythonhosted.org"
    )
}

fn download_client() -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().host_str().is_some_and(trusted_helper_host) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .user_agent("RepoTunnel/0.3.1 Video Intelligence")
        .build()
        .map_err(|error| format!("Could not initialize the video helper downloader: {error}"))
}

fn download_bytes(
    client: &Client,
    url: &str,
    limit: u64,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<u8>, String> {
    let parsed =
        Url::parse(url).map_err(|_| "Video helper download URL is invalid.".to_string())?;
    if parsed.scheme() != "https" {
        return Err("Video helpers may only be downloaded over HTTPS.".to_string());
    }
    let host = parsed.host_str().unwrap_or_default();
    if !trusted_helper_host(host) {
        return Err("Video helper download host is not allowlisted.".to_string());
    }

    let mut response = client
        .get(url)
        .send()
        .map_err(|error| format!("Could not download a video helper: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Video helper download failed with HTTP {}.",
            response.status()
        ));
    }
    if response.content_length().is_some_and(|size| size > limit) {
        return Err("Video helper download exceeds RepoTunnel's safety limit.".to_string());
    }

    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err("Video analysis cancelled.".to_string());
        }
        let read = response
            .read(&mut buffer)
            .map_err(|error| format!("Could not read a video helper download: {error}"))?;
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > usize::try_from(limit).unwrap_or(usize::MAX) {
            return Err("Video helper download exceeds RepoTunnel's safety limit.".to_string());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(bytes)
}

fn yt_dlp_asset_name() -> Result<&'static str, String> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("yt-dlp_linux"),
        ("linux", "aarch64") => Ok("yt-dlp_linux_aarch64"),
        ("windows", "x86_64") => Ok("yt-dlp.exe"),
        ("macos", "x86_64" | "aarch64") => Ok("yt-dlp_macos"),
        _ => Err(format!(
            "Automatic yt-dlp provisioning is not available for {} {}.",
            env::consts::OS,
            env::consts::ARCH
        )),
    }
}

fn install_yt_dlp(
    app: &AppHandle,
    cancel: Option<&AtomicBool>,
    force_refresh: bool,
) -> Result<(), String> {
    if !force_refresh && program_path(app, "yt-dlp").is_some() {
        return Ok(());
    }
    let asset = yt_dlp_asset_name()?;
    let client = download_client()?;
    let sums = download_bytes(
        &client,
        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS",
        2 * 1024 * 1024,
        cancel,
    )?;
    let sums_text = String::from_utf8(sums)
        .map_err(|_| "yt-dlp checksum manifest was not valid UTF-8.".to_string())?;
    let expected = sums_text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let name = parts.next()?.trim_start_matches('*');
            (name == asset).then_some(hash.to_ascii_lowercase())
        })
        .next()
        .ok_or_else(|| {
            "yt-dlp checksum manifest did not contain the required platform binary.".to_string()
        })?;
    let url = format!("https://github.com/yt-dlp/yt-dlp/releases/latest/download/{asset}");
    let bytes = download_bytes(&client, &url, MAX_HELPER_DOWNLOAD_BYTES, cancel)?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(
            "yt-dlp checksum verification failed; RepoTunnel refused to install it.".to_string(),
        );
    }
    let destination = tools_root(app)?.join(executable_name("yt-dlp"));
    if force_refresh && destination.exists() {
        fs::remove_file(&destination)
            .map_err(|error| format!("Could not replace the managed yt-dlp helper: {error}"))?;
    }
    private_write(&destination, &bytes, true)?;
    if program_version(&destination, "yt-dlp").is_none() {
        let _ = fs::remove_file(&destination);
        return Err("Downloaded yt-dlp could not be verified by execution.".to_string());
    }
    Ok(())
}

fn wheel_matches_platform(filename: &str) -> bool {
    let lower = filename.to_ascii_lowercase();
    if !lower.ends_with(".whl") {
        return false;
    }
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => lower.contains("manylinux") && lower.contains("x86_64"),
        ("linux", "aarch64") => {
            lower.contains("manylinux") && (lower.contains("aarch64") || lower.contains("arm64"))
        }
        ("windows", "x86_64") => lower.contains("win_amd64"),
        ("windows", "aarch64") => lower.contains("win_arm64"),
        ("macos", "x86_64") => lower.contains("macosx") && lower.contains("x86_64"),
        ("macos", "aarch64") => lower.contains("macosx") && lower.contains("arm64"),
        _ => false,
    }
}

fn install_ffmpeg(app: &AppHandle, cancel: Option<&AtomicBool>) -> Result<(), String> {
    if program_path(app, "ffmpeg").is_some() {
        return Ok(());
    }
    let client = download_client()?;
    let metadata = download_bytes(
        &client,
        "https://pypi.org/pypi/imageio-ffmpeg/json",
        4 * 1024 * 1024,
        cancel,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&metadata)
        .map_err(|error| format!("Could not parse imageio-ffmpeg package metadata: {error}"))?;
    let files = value
        .get("urls")
        .and_then(|item| item.as_array())
        .ok_or_else(|| {
            "imageio-ffmpeg package metadata did not include release files.".to_string()
        })?;

    let selected = files
        .iter()
        .filter_map(|item| {
            let filename = item.get("filename")?.as_str()?;
            if !wheel_matches_platform(filename) {
                return None;
            }
            let url = item.get("url")?.as_str()?;
            let sha256 = item.get("digests")?.get("sha256")?.as_str()?;
            Some((
                filename.to_string(),
                url.to_string(),
                sha256.to_ascii_lowercase(),
            ))
        })
        .next()
        .ok_or_else(|| {
            format!(
                "No managed FFmpeg build is available for {} {}.",
                env::consts::OS,
                env::consts::ARCH
            )
        })?;

    let parsed =
        Url::parse(&selected.1).map_err(|_| "FFmpeg package URL is invalid.".to_string())?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("files.pythonhosted.org") {
        return Err("FFmpeg package URL did not use the expected PyPI file host.".to_string());
    }

    let wheel = download_bytes(&client, &selected.1, MAX_HELPER_DOWNLOAD_BYTES, cancel)?;
    if sha256_hex(&wheel) != selected.2 {
        return Err(
            "FFmpeg package checksum verification failed; RepoTunnel refused to install it."
                .to_string(),
        );
    }

    let mut archive = ZipArchive::new(Cursor::new(wheel))
        .map_err(|error| format!("Could not open the verified FFmpeg package: {error}"))?;
    let mut binary = None;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .map_err(|error| format!("Could not inspect the FFmpeg package: {error}"))?;
        let name = file.name().replace('\\', "/");
        let lower = name.to_ascii_lowercase();
        let looks_like_binary = lower.contains("/binaries/ffmpeg-")
            && !lower.ends_with(".txt")
            && !lower.ends_with(".md");
        if looks_like_binary {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(|error| {
                format!("Could not extract FFmpeg from its verified package: {error}")
            })?;
            if !bytes.is_empty() {
                binary = Some(bytes);
                break;
            }
        }
    }
    let bytes = binary.ok_or_else(|| {
        format!(
            "The verified FFmpeg wheel {} did not contain its expected executable.",
            selected.0
        )
    })?;
    let destination = tools_root(app)?.join(executable_name("ffmpeg"));
    private_write(&destination, &bytes, true)?;
    if program_version(&destination, "ffmpeg").is_none() {
        let _ = fs::remove_file(&destination);
        return Err("Downloaded FFmpeg could not be verified by execution.".to_string());
    }
    Ok(())
}

pub(crate) fn install_missing_tools(app: &AppHandle) -> Result<VideoToolsStatus, String> {
    install_yt_dlp(app, None, false)?;
    install_ffmpeg(app, None)?;
    tools_status(app)
}

fn ensure_tool(app: &AppHandle, name: &str, cancel: &AtomicBool) -> Result<PathBuf, String> {
    if let Some((path, _)) = program_path(app, name) {
        return Ok(path);
    }
    match name {
        "yt-dlp" => install_yt_dlp(app, Some(cancel), false)?,
        "ffmpeg" => install_ffmpeg(app, Some(cancel))?,
        _ => return Err("Unknown video helper.".to_string()),
    }
    program_path(app, name)
        .map(|(path, _)| path)
        .ok_or_else(|| format!("{name} is still unavailable after provisioning."))
}

fn refresh_yt_dlp_runtime(
    app: &AppHandle,
    runtime: &mut YtDlpRuntime,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if runtime.refreshed_after_failure {
        return Err(
            "The latest verified managed yt-dlp was already retried for this analysis.".to_string(),
        );
    }
    install_yt_dlp(app, Some(cancel), true)?;
    runtime.path = managed_program(app, "yt-dlp")
        .ok_or_else(|| "Managed yt-dlp was not available after refresh.".to_string())?;
    runtime.refreshed_after_failure = true;
    Ok(())
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || (octets[0] == 0)
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || octets[0] >= 240)
}

fn public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return public_ipv4(mapped);
    }
    let segments = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

fn public_url_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => public_ipv4(ip),
        Some(Host::Ipv6(ip)) => public_ipv6(ip),
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            !matches!(
                host.as_str(),
                "localhost" | "localhost.localdomain" | "home.arpa"
            ) && !host.ends_with(".localhost")
                && !host.ends_with(".local")
                && !host.ends_with(".home.arpa")
        }
        None => false,
    }
}

fn source_kind(source: &str) -> Result<&'static str, String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err("Video source cannot be empty.".to_string());
    }
    if trimmed.len() > MAX_URL_LENGTH {
        return Err("Video source is too long.".to_string());
    }
    if let Ok(url) = Url::parse(trimmed) {
        return match url.scheme() {
            "http" | "https" if public_url_host(&url) => Ok("url"),
            "http" | "https" => Err(
                "Localhost, private-network, link-local, and other non-public video URLs are blocked."
                    .to_string(),
            ),
            _ => Err("Only public http/https video URLs are accepted. Local media must use a workspace-relative path.".to_string()),
        };
    }
    Ok("workspace")
}

fn media_extension_allowed(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some(
            "mp4"
                | "mkv"
                | "webm"
                | "mov"
                | "m4v"
                | "avi"
                | "mpeg"
                | "mpg"
                | "mp3"
                | "m4a"
                | "aac"
                | "wav"
                | "flac"
                | "ogg"
                | "opus"
        )
    )
}

fn resolve_source(workspace: &Workspace, source: &str) -> Result<VideoSource, String> {
    match source_kind(source)? {
        "url" => Ok(VideoSource::Url(source.trim().to_string())),
        _ => {
            let path =
                resolve_workspace_path(workspace, source.trim(), AccessOperation::Read, true)?;
            let metadata = fs::metadata(&path)
                .map_err(|error| format!("Could not inspect local media: {error}"))?;
            if !metadata.is_file() {
                return Err("Local video source must be a regular file.".to_string());
            }
            if metadata.len() > MAX_LOCAL_MEDIA_BYTES {
                return Err("Local media exceeds RepoTunnel's 10 GiB analysis limit.".to_string());
            }
            if !media_extension_allowed(&path) {
                return Err(
                    "This local file type is not supported by Video Intelligence.".to_string(),
                );
            }
            Ok(VideoSource::Local(path))
        }
    }
}

fn cache_key(
    workspace_id: &str,
    source: &str,
    mode: VideoMode,
    start: f64,
    end: Option<f64>,
    max_frames: usize,
) -> String {
    let input = format!(
        "video-v1|{workspace_id}|{}|{}|{start:.3}|{}|{max_frames}",
        source.trim(),
        mode.as_str(),
        end.map(|value| format!("{value:.3}"))
            .unwrap_or_else(|| "end".to_string())
    );
    sha256_hex(input.as_bytes())
}

fn validate_range(start: Option<f64>, end: Option<f64>) -> Result<(f64, Option<f64>), String> {
    let start = start.unwrap_or(0.0);
    if !start.is_finite() || start < 0.0 {
        return Err("Video start time must be a non-negative number of seconds.".to_string());
    }
    if let Some(end) = end {
        if !end.is_finite() || end <= start {
            return Err("Video end time must be greater than start time.".to_string());
        }
        Ok((start, Some(end)))
    } else {
        Ok((start, None))
    }
}

fn update_job(id: &str, mutate: impl FnOnce(&mut VideoAnalysisJob)) {
    let Ok(mut guard) = jobs().lock() else {
        return;
    };
    if let Some(runtime) = guard.get_mut(id) {
        mutate(&mut runtime.job);
        runtime.job.updated_at = now_millis();
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("Video analysis cancelled.".to_string())
    } else {
        Ok(())
    }
}

fn trim_command_output(mut value: String) -> String {
    if value.len() <= COMMAND_OUTPUT_LIMIT {
        return value;
    }
    let start = value.len().saturating_sub(COMMAND_OUTPUT_LIMIT);
    while !value.is_char_boundary(start.min(value.len())) {
        let _ = value.remove(0);
    }
    value.split_off(start.min(value.len()))
}

pub(crate) fn ensure_ffmpeg_program(app: &AppHandle) -> Result<PathBuf, String> {
    let cancel = AtomicBool::new(false);
    ensure_tool(app, "ffmpeg", &cancel)
}

pub(crate) fn configure_background_command(command: &mut Command) {
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

pub(crate) fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        let pid = child.id() as i32;
        let _ = libc::kill(-pid, libc::SIGTERM);
        thread::sleep(Duration::from_millis(120));
        let _ = libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_capture(
    program: &Path,
    args: &[OsString],
    work_dir: &Path,
    cancel: &AtomicBool,
    label: &str,
) -> Result<CommandCapture, String> {
    check_cancel(cancel)?;
    let nonce = COMMAND_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stdout_path = work_dir.join(format!("._{label}-{nonce:x}.stdout"));
    let stderr_path = work_dir.join(format!("._{label}-{nonce:x}.stderr"));
    let stdout_file = File::create(&stdout_path)
        .map_err(|error| format!("Could not prepare video command output: {error}"))?;
    let stderr_file = File::create(&stderr_path)
        .map_err(|error| format!("Could not prepare video command diagnostics: {error}"))?;

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(work_dir)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    configure_background_command(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start {label}: {error}"))?;

    let status = loop {
        check_cancel(cancel).inspect_err(|_| {
            terminate_child(&mut child);
        })?;
        match child
            .try_wait()
            .map_err(|error| format!("Could not inspect {label}: {error}"))?
        {
            Some(status) => break status,
            None => thread::sleep(Duration::from_millis(120)),
        }
    };

    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);

    Ok(CommandCapture {
        success: status.success(),
        stdout: trim_command_output(stdout),
        stderr: trim_command_output(stderr),
    })
}

fn os(value: impl Into<OsString>) -> OsString {
    value.into()
}

fn url_metadata(
    yt_dlp: &Path,
    source: &str,
    work_dir: &Path,
    cancel: &AtomicBool,
) -> Result<SourceMetadata, String> {
    let args = vec![
        os("--dump-single-json"),
        os("--skip-download"),
        os("--no-playlist"),
        os("--socket-timeout"),
        os("15"),
        os("--retries"),
        os("2"),
        os("--no-warnings"),
        os(source),
    ];
    let capture = run_capture(yt_dlp, &args, work_dir, cancel, "video-metadata")?;
    if !capture.success {
        return Err(format!(
            "Could not inspect this video URL. {}",
            compact_error(&capture.stderr)
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&capture.stdout)
        .map_err(|error| format!("Video metadata response was invalid: {error}"))?;
    let title = value
        .get("title")
        .and_then(|item| item.as_str())
        .filter(|item| !item.trim().is_empty())
        .unwrap_or("Online video")
        .to_string();
    let duration_seconds = value.get("duration").and_then(|item| item.as_f64());
    Ok(SourceMetadata {
        title,
        duration_seconds,
    })
}

fn url_metadata_resilient(
    app: &AppHandle,
    runtime: &mut YtDlpRuntime,
    source: &str,
    work_dir: &Path,
    cancel: &AtomicBool,
    job_id: &str,
) -> Result<SourceMetadata, String> {
    match url_metadata(&runtime.path, source, work_dir, cancel) {
        Ok(metadata) => Ok(metadata),
        Err(first_error) => {
            update_job(job_id, |job| {
                job.phase = "helpers".to_string();
                job.progress = job.progress.max(12);
                job.message =
                    "Refreshing the video URL helper after an extractor failure…".to_string();
            });
            refresh_yt_dlp_runtime(app, runtime, cancel).map_err(|refresh_error| {
                format!(
                    "{first_error} RepoTunnel could not refresh yt-dlp securely: {refresh_error}"
                )
            })?;
            url_metadata(&runtime.path, source, work_dir, cancel).map_err(|retry_error| {
                format!(
                    "{retry_error} RepoTunnel retried with the latest verified yt-dlp after the initial extractor failure: {first_error}"
                )
            })
        }
    }
}

fn compact_error(value: &str) -> String {
    value
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("The media extractor reported an error.")
        .trim()
        .chars()
        .take(500)
        .collect()
}

fn parse_clock(value: &str) -> Option<f64> {
    let parts = value.trim().split(':').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let hours = parts[0].parse::<f64>().ok()?;
    let minutes = parts[1].parse::<f64>().ok()?;
    let seconds = parts[2].replace(',', ".").parse::<f64>().ok()?;
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn parse_ffmpeg_duration(stderr: &str) -> Option<f64> {
    let marker = "Duration: ";
    let index = stderr.find(marker)?;
    let rest = &stderr[index + marker.len()..];
    let time = rest.split(',').next()?.trim();
    parse_clock(time)
}

fn local_metadata(
    ffmpeg: &Path,
    source: &Path,
    work_dir: &Path,
    cancel: &AtomicBool,
) -> Result<SourceMetadata, String> {
    let args = vec![
        os("-hide_banner"),
        os("-i"),
        source.as_os_str().to_os_string(),
    ];
    let capture = run_capture(ffmpeg, &args, work_dir, cancel, "video-probe")?;
    let duration_seconds = parse_ffmpeg_duration(&capture.stderr);
    let title = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Local media")
        .to_string();
    Ok(SourceMetadata {
        title,
        duration_seconds,
    })
}

fn strip_tags(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut inside = false;
    for character in value.chars() {
        match character {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => output.push(character),
            _ => {}
        }
    }
    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_timestamp(seconds: f64) -> String {
    let seconds = seconds.max(0.0).floor() as u64;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    format!("{hours:02}:{minutes:02}:{secs:02}")
}

fn parse_vtt(contents: &str, start: f64, end: Option<f64>) -> Option<String> {
    let mut output = String::new();
    let mut cue_time = None;
    let mut cue_text = Vec::<String>::new();
    let mut previous = String::new();

    let flush = |time: Option<f64>,
                 text_parts: &mut Vec<String>,
                 output: &mut String,
                 previous: &mut String| {
        let Some(time) = time else {
            text_parts.clear();
            return;
        };
        if time < start || end.is_some_and(|limit| time > limit) {
            text_parts.clear();
            return;
        }
        let text = strip_tags(&text_parts.join(" "));
        text_parts.clear();
        if text.is_empty() || text == *previous {
            return;
        }
        *previous = text.clone();
        output.push_str(&format!("[{}] {text}\n", format_timestamp(time)));
    };

    for line in contents.lines().chain(std::iter::once("")) {
        let trimmed = line.trim();
        if trimmed.contains("-->") {
            flush(cue_time.take(), &mut cue_text, &mut output, &mut previous);
            cue_time = trimmed
                .split("-->")
                .next()
                .and_then(|value| parse_clock(value.trim()));
            continue;
        }
        if trimmed.is_empty() {
            flush(cue_time.take(), &mut cue_text, &mut output, &mut previous);
            continue;
        }
        if cue_time.is_some()
            && !trimmed.starts_with("WEBVTT")
            && !trimmed.starts_with("NOTE")
            && !trimmed.chars().all(|character| character.is_ascii_digit())
        {
            cue_text.push(trimmed.to_string());
        }
    }

    if output.is_empty() {
        None
    } else {
        if output.len() > MAX_TRANSCRIPT_CHARS {
            output.truncate(
                output
                    .char_indices()
                    .take_while(|(index, _)| *index <= MAX_TRANSCRIPT_CHARS)
                    .last()
                    .map(|(index, character)| index + character.len_utf8())
                    .unwrap_or(0),
            );
            output.push_str("\n[Transcript truncated by RepoTunnel safety limit.]");
        }
        Some(output)
    }
}

fn find_vtt(directory: &Path, prefix: &str) -> Option<PathBuf> {
    let mut candidates = fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("vtt"))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX));
    candidates.into_iter().next()
}

fn url_captions(
    yt_dlp: &Path,
    source: &str,
    work_dir: &Path,
    cancel: &AtomicBool,
    start: f64,
    end: Option<f64>,
) -> Option<String> {
    let template = work_dir.join("caption.%(ext)s");
    let args = vec![
        os("--skip-download"),
        os("--no-playlist"),
        os("--write-subs"),
        os("--write-auto-subs"),
        os("--sub-langs"),
        os("en.*,en"),
        os("--sub-format"),
        os("vtt"),
        os("--socket-timeout"),
        os("15"),
        os("--retries"),
        os("2"),
        os("--no-progress"),
        os("-o"),
        template.as_os_str().to_os_string(),
        os(source),
    ];
    let capture = run_capture(yt_dlp, &args, work_dir, cancel, "video-captions").ok()?;
    if !capture.success {
        return None;
    }
    let path = find_vtt(work_dir, "caption.")?;
    let contents = fs::read_to_string(path).ok()?;
    parse_vtt(&contents, start, end)
}

fn local_captions(
    ffmpeg: &Path,
    source: &Path,
    work_dir: &Path,
    cancel: &AtomicBool,
    start: f64,
    end: Option<f64>,
) -> Option<String> {
    let target = work_dir.join("embedded-subtitles.vtt");
    let args = vec![
        os("-hide_banner"),
        os("-loglevel"),
        os("error"),
        os("-y"),
        os("-i"),
        source.as_os_str().to_os_string(),
        os("-map"),
        os("0:s:0"),
        os("-c:s"),
        os("webvtt"),
        target.as_os_str().to_os_string(),
    ];
    let capture = run_capture(ffmpeg, &args, work_dir, cancel, "video-subtitles").ok()?;
    if !capture.success || !target.is_file() {
        return None;
    }
    parse_vtt(&fs::read_to_string(target).ok()?, start, end)
}

fn ffmpeg_parent(ffmpeg: &Path) -> OsString {
    ffmpeg
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .as_os_str()
        .to_os_string()
}

fn section_value(start: f64, end: Option<f64>) -> Option<String> {
    if start <= 0.0 && end.is_none() {
        return None;
    }
    Some(match end {
        Some(end) => format!("*{start:.3}-{end:.3}"),
        None => format!("*{start:.3}-inf"),
    })
}

fn find_generated_media(directory: &Path, prefix: &str) -> Option<PathBuf> {
    fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(prefix)
                            && !name.ends_with(".part")
                            && !name.ends_with(".ytdl")
                    })
        })
        .max_by_key(|path| {
            fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        })
}

fn download_url_stream(
    yt_dlp: &Path,
    ffmpeg: &Path,
    source: &str,
    work_dir: &Path,
    cancel: &AtomicBool,
    visual: bool,
    range: MediaRange,
) -> Result<PathBuf, String> {
    let start = range.start;
    let end = range.end;
    let prefix = if visual {
        "visual-source"
    } else {
        "audio-source"
    };
    let template = work_dir.join(format!("{prefix}.%(ext)s"));
    let format = if visual {
        "bv*[height<=480]/b[height<=480]/best[height<=480]/worst"
    } else {
        "ba/bestaudio/best"
    };
    let mut args = vec![
        os("--no-playlist"),
        os("--socket-timeout"),
        os("15"),
        os("--retries"),
        os("2"),
        os("--fragment-retries"),
        os("2"),
        os("--no-progress"),
        os("--max-filesize"),
        os("700M"),
        os("--ffmpeg-location"),
        ffmpeg_parent(ffmpeg),
        os("-f"),
        os(format),
        os("-o"),
        template.as_os_str().to_os_string(),
    ];
    if let Some(section) = section_value(start, end) {
        args.push(os("--download-sections"));
        args.push(os(section));
    }
    args.push(os(source));
    let capture = run_capture(yt_dlp, &args, work_dir, cancel, "video-download")?;
    if !capture.success {
        return Err(format!(
            "Could not retrieve the requested media stream. {}",
            compact_error(&capture.stderr)
        ));
    }
    find_generated_media(work_dir, prefix)
        .ok_or_else(|| "Media download finished without a usable file.".to_string())
}

fn download_url_stream_resilient(
    app: &AppHandle,
    runtime: &mut YtDlpRuntime,
    ffmpeg: &Path,
    source: &str,
    work_dir: &Path,
    cancel: &AtomicBool,
    plan: UrlDownloadPlan,
) -> Result<PathBuf, String> {
    match download_url_stream(
        &runtime.path,
        ffmpeg,
        source,
        work_dir,
        cancel,
        plan.visual,
        plan.range,
    ) {
        Ok(path) => Ok(path),
        Err(first_error) => {
            refresh_yt_dlp_runtime(app, runtime, cancel).map_err(|refresh_error| {
                format!(
                    "{first_error} RepoTunnel could not refresh yt-dlp securely: {refresh_error}"
                )
            })?;
            download_url_stream(
                &runtime.path,
                ffmpeg,
                source,
                work_dir,
                cancel,
                plan.visual,
                plan.range,
            )
            .map_err(|retry_error| {
                format!(
                    "{retry_error} RepoTunnel retried with the latest verified yt-dlp after the initial stream failure: {first_error}"
                )
            })
        }
    }
}

fn parse_showinfo_times(stderr: &str, offset: f64) -> Vec<f64> {
    let mut times = Vec::new();
    for line in stderr.lines() {
        let Some(index) = line.find("pts_time:") else {
            continue;
        };
        let rest = &line[index + "pts_time:".len()..];
        let token = rest.split_whitespace().next().unwrap_or_default();
        if let Ok(value) = token.parse::<f64>() {
            times.push((offset + value).max(0.0));
        }
    }
    times
}

fn frame_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("frame-") && name.ends_with(".jpg"))
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn remove_frames(directory: &Path) {
    for path in frame_files(directory) {
        let _ = fs::remove_file(path);
    }
}

fn extract_frames(
    ffmpeg: &Path,
    input: &Path,
    work_dir: &Path,
    cancel: &AtomicBool,
    range: MediaRange,
    plan: FrameExtractionPlan,
) -> Result<(Vec<PathBuf>, Vec<f64>), String> {
    let start = range.start;
    let end = range.end;
    let source_pretrimmed = plan.source_pretrimmed;
    let duration_seconds = plan.duration_seconds;
    let max_frames = plan.max_frames;
    let output = work_dir.join("frame-%03d.jpg");
    let mut args = vec![os("-hide_banner"), os("-y")];
    if !source_pretrimmed && start > 0.0 {
        args.push(os("-ss"));
        args.push(os(format!("{start:.3}")));
    }
    args.push(os("-i"));
    args.push(input.as_os_str().to_os_string());
    if !source_pretrimmed {
        if let Some(end) = end {
            args.push(os("-t"));
            args.push(os(format!("{:.3}", end - start)));
        }
    }
    args.extend([
        os("-vf"),
        os("select=gt(scene\\,0.28),showinfo,scale=960:-2:force_original_aspect_ratio=decrease"),
        os("-fps_mode"),
        os("vfr"),
        os("-q:v"),
        os("4"),
        os("-frames:v"),
        os(max_frames.to_string()),
        output.as_os_str().to_os_string(),
    ]);
    let capture = run_capture(ffmpeg, &args, work_dir, cancel, "video-frames")?;
    let mut files = frame_files(work_dir);
    let mut times = parse_showinfo_times(&capture.stderr, start);

    if !capture.success || files.len() < 2 {
        remove_frames(work_dir);
        let selected_duration = end
            .map(|value| value - start)
            .or_else(|| duration_seconds.map(|value| (value - start).max(0.0)))
            .unwrap_or(60.0)
            .max(1.0);
        let interval = (selected_duration / max_frames.max(1) as f64).max(1.0);
        let mut fallback = vec![os("-hide_banner"), os("-y")];
        if !source_pretrimmed && start > 0.0 {
            fallback.push(os("-ss"));
            fallback.push(os(format!("{start:.3}")));
        }
        fallback.push(os("-i"));
        fallback.push(input.as_os_str().to_os_string());
        if !source_pretrimmed {
            if let Some(end) = end {
                fallback.push(os("-t"));
                fallback.push(os(format!("{:.3}", end - start)));
            }
        }
        fallback.extend([
            os("-vf"),
            os(format!(
                "fps=1/{interval:.3},showinfo,scale=960:-2:force_original_aspect_ratio=decrease"
            )),
            os("-q:v"),
            os("4"),
            os("-frames:v"),
            os(max_frames.to_string()),
            output.as_os_str().to_os_string(),
        ]);
        let capture = run_capture(ffmpeg, &fallback, work_dir, cancel, "video-frame-fallback")?;
        if !capture.success {
            return Err(format!(
                "Could not extract visual frames. {}",
                compact_error(&capture.stderr)
            ));
        }
        files = frame_files(work_dir);
        times = parse_showinfo_times(&capture.stderr, start);
    }

    if files.is_empty() {
        return Err("No visual frames could be extracted from this media.".to_string());
    }
    if times.len() < files.len() {
        let selected_duration = end
            .map(|value| value - start)
            .or_else(|| duration_seconds.map(|value| (value - start).max(0.0)))
            .unwrap_or(files.len() as f64 * 5.0)
            .max(1.0);
        while times.len() < files.len() {
            let index = times.len();
            times.push(start + selected_duration * index as f64 / files.len().max(1) as f64);
        }
    }
    times.truncate(files.len());
    Ok((files, times))
}

fn audio_files(directory: &Path) -> Vec<PathBuf> {
    let mut files = fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("audio-chunk-")
                        && matches!(
                            path.extension()
                                .and_then(|extension| extension.to_str())
                                .map(|extension| extension.to_ascii_lowercase())
                                .as_deref(),
                            Some("ogg" | "m4a")
                        )
                })
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn remove_audio_chunks(directory: &Path) {
    for path in audio_files(directory) {
        let _ = fs::remove_file(path);
    }
}

fn extract_audio_chunks(
    ffmpeg: &Path,
    input: &Path,
    work_dir: &Path,
    cancel: &AtomicBool,
    start: f64,
    end: Option<f64>,
    source_pretrimmed: bool,
) -> Result<Vec<PathBuf>, String> {
    let output = work_dir.join("audio-chunk-%03d.ogg");
    let mut args = vec![os("-hide_banner"), os("-y")];
    if !source_pretrimmed && start > 0.0 {
        args.push(os("-ss"));
        args.push(os(format!("{start:.3}")));
    }
    args.push(os("-i"));
    args.push(input.as_os_str().to_os_string());
    if !source_pretrimmed {
        if let Some(end) = end {
            args.push(os("-t"));
            args.push(os(format!("{:.3}", end - start)));
        }
    }
    args.extend([
        os("-vn"),
        os("-ac"),
        os("1"),
        os("-ar"),
        os("16000"),
        os("-c:a"),
        os("libopus"),
        os("-b:a"),
        os("24k"),
        os("-f"),
        os("segment"),
        os("-segment_time"),
        os("900"),
        os("-reset_timestamps"),
        os("1"),
        output.as_os_str().to_os_string(),
    ]);
    let capture = run_capture(ffmpeg, &args, work_dir, cancel, "video-audio")?;
    if capture.success {
        let files = audio_files(work_dir);
        if !files.is_empty() {
            return Ok(files);
        }
    }

    remove_audio_chunks(work_dir);
    let output = work_dir.join("audio-chunk-%03d.m4a");
    let mut fallback = vec![os("-hide_banner"), os("-y")];
    if !source_pretrimmed && start > 0.0 {
        fallback.push(os("-ss"));
        fallback.push(os(format!("{start:.3}")));
    }
    fallback.push(os("-i"));
    fallback.push(input.as_os_str().to_os_string());
    if !source_pretrimmed {
        if let Some(end) = end {
            fallback.push(os("-t"));
            fallback.push(os(format!("{:.3}", end - start)));
        }
    }
    fallback.extend([
        os("-vn"),
        os("-ac"),
        os("1"),
        os("-ar"),
        os("16000"),
        os("-c:a"),
        os("aac"),
        os("-b:a"),
        os("32k"),
        os("-f"),
        os("segment"),
        os("-segment_time"),
        os("900"),
        os("-segment_format"),
        os("mp4"),
        os("-reset_timestamps"),
        os("1"),
        output.as_os_str().to_os_string(),
    ]);
    let capture = run_capture(ffmpeg, &fallback, work_dir, cancel, "video-audio-fallback")?;
    if !capture.success {
        return Err(format!(
            "Could not prepare compact audio for AI analysis. {}",
            compact_error(&capture.stderr)
        ));
    }
    let files = audio_files(work_dir);
    if files.is_empty() {
        return Err(
            "The media did not expose an audio stream for fallback understanding.".to_string(),
        );
    }
    Ok(files)
}

fn selected_duration(duration: Option<f64>, start: f64, end: Option<f64>) -> Option<f64> {
    if let Some(end) = end {
        return Some((end - start).max(0.0));
    }
    duration.map(|duration| (duration - start).max(0.0))
}

fn remove_temporary_media(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.starts_with("visual-source.") || name.starts_with("audio-source.") {
            let _ = fs::remove_file(path);
        }
    }
}

fn result_path(directory: &Path) -> PathBuf {
    directory.join("result.json")
}

fn load_cached(directory: &Path) -> Option<CachedVideoResult> {
    let contents = fs::read_to_string(result_path(directory)).ok()?;
    let cached: CachedVideoResult = serde_json::from_str(&contents).ok()?;
    let frames_ok = cached
        .frame_files
        .iter()
        .all(|name| directory.join(name).is_file());
    let audio_ok = cached
        .audio_files
        .iter()
        .all(|name| directory.join(name).is_file());
    (frames_ok && audio_ok).then_some(cached)
}

fn save_cached(directory: &Path, cached: &CachedVideoResult) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(cached)
        .map_err(|error| format!("Could not serialize video analysis cache: {error}"))?;
    private_write(&result_path(directory), &contents, false)
}

fn prune_cache(app: &AppHandle, keep: &Path) {
    let Ok(root) = cache_root(app) else {
        return;
    };
    let mut items = fs::read_dir(&root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path != keep)
        .map(|path| {
            let modified = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let size = directory_size(&path);
            (path, modified, size)
        })
        .collect::<Vec<_>>();
    let keep_size = directory_size(keep);
    let mut total = keep_size.saturating_add(items.iter().map(|item| item.2).sum::<u64>());
    items.sort_by_key(|item| item.1);
    for (path, _, size) in items {
        if total <= CACHE_LIMIT_BYTES {
            break;
        }
        if fs::remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

fn job_completed_from_result(job_id: &str, result: &VideoAnalysisResult) {
    update_job(job_id, |job| {
        job.status = "completed".to_string();
        job.phase = "ready".to_string();
        job.progress = 100;
        job.message = if result.cache_hit {
            "Loaded instantly from the RepoTunnel video cache.".to_string()
        } else {
            "Video is prepared for AI understanding.".to_string()
        };
        job.title = Some(result.title.clone());
        job.duration_seconds = result.duration_seconds;
        job.transcript_available = result.transcript.is_some();
        job.frame_count = result.frames.len();
        job.audio_chunk_count = result.audio_chunk_count;
        job.cache_key = Some(result.cache_key.clone());
        job.cache_hit = result.cache_hit;
    });
}

fn analyze_internal(
    app: &AppHandle,
    workspace: &Workspace,
    job_id: &str,
    source_text: &str,
    plan: AnalysisPlan,
    cancel: &AtomicBool,
) -> Result<VideoAnalysisResult, String> {
    check_cancel(cancel)?;
    let mode = plan.mode;
    let start = plan.range.start;
    let end = plan.range.end;
    let max_frames = plan.max_frames;
    let key = cache_key(&workspace.id, source_text, mode, start, end, max_frames);
    let root = cache_root(app)?;
    let directory = root.join(&key);
    if let Some(mut cached) = load_cached(&directory) {
        cached.result.job_id = job_id.to_string();
        cached.result.cache_hit = true;
        cached.result.completed_at = now_millis();
        return Ok(cached.result);
    }

    if directory.exists() {
        fs::remove_dir_all(&directory)
            .map_err(|error| format!("Could not reset an incomplete video cache entry: {error}"))?;
    }
    ensure_private_dir(&directory)?;

    update_job(job_id, |job| {
        job.phase = "inspect".to_string();
        job.progress = 5;
        job.cache_key = Some(key.clone());
        job.message = "Inspecting media source…".to_string();
    });

    let source = resolve_source(workspace, source_text)?;
    let source_kind_name = match &source {
        VideoSource::Url(_) => "url",
        VideoSource::Local(_) => "workspace",
    }
    .to_string();

    let mut yt_dlp = if matches!(&source, VideoSource::Url(_)) {
        update_job(job_id, |job| {
            job.phase = "helpers".to_string();
            job.progress = 8;
            job.message = "Checking video URL helper…".to_string();
        });
        Some(YtDlpRuntime {
            path: ensure_tool(app, "yt-dlp", cancel)?,
            refreshed_after_failure: false,
        })
    } else {
        None
    };

    let mut ffmpeg = None::<PathBuf>;
    let mut ensure_ffmpeg = || -> Result<PathBuf, String> {
        if let Some(path) = ffmpeg.clone() {
            return Ok(path);
        }
        update_job(job_id, |job| {
            job.phase = "helpers".to_string();
            job.progress = job.progress.max(10);
            job.message = "Checking media processing helper…".to_string();
        });
        let path = ensure_tool(app, "ffmpeg", cancel)?;
        ffmpeg = Some(path.clone());
        Ok(path)
    };

    let metadata = match &source {
        VideoSource::Url(url) => url_metadata_resilient(
            app,
            yt_dlp.as_mut().expect("yt-dlp exists for URL"),
            url,
            &directory,
            cancel,
            job_id,
        )?,
        VideoSource::Local(path) => {
            let ffmpeg = ensure_ffmpeg()?;
            local_metadata(&ffmpeg, path, &directory, cancel)?
        }
    };

    if start > metadata.duration_seconds.unwrap_or(f64::MAX) {
        return Err("Video start time is beyond the media duration.".to_string());
    }
    if end.is_some_and(|end| {
        metadata
            .duration_seconds
            .is_some_and(|duration| end > duration + 1.0)
    }) {
        return Err("Video end time is beyond the media duration.".to_string());
    }

    update_job(job_id, |job| {
        job.title = Some(metadata.title.clone());
        job.duration_seconds = metadata.duration_seconds;
        job.phase = "captions".to_string();
        job.progress = 18;
        job.message = "Checking for existing captions first…".to_string();
    });

    let transcript = match &source {
        VideoSource::Url(url) => url_captions(
            &yt_dlp.as_ref().expect("yt-dlp exists for URL").path,
            url,
            &directory,
            cancel,
            start,
            end,
        ),
        VideoSource::Local(path) => {
            let ffmpeg = ensure_ffmpeg()?;
            local_captions(&ffmpeg, path, &directory, cancel, start, end)
        }
    };
    let transcript_source = transcript.as_ref().map(|_| "captions".to_string());

    let duration_selected = selected_duration(metadata.duration_seconds, start, end);
    if mode.needs_frames()
        && end.is_none()
        && duration_selected.is_some_and(|duration| duration > MAX_FULL_VISUAL_SECONDS)
    {
        return Err(
            "This video is over two hours. For fast visual analysis, specify a start/end range."
                .to_string(),
        );
    }

    let mut frame_paths = Vec::<PathBuf>::new();
    let mut frame_times = Vec::<f64>::new();

    if mode.needs_frames() {
        check_cancel(cancel)?;
        update_job(job_id, |job| {
            job.phase = "visual".to_string();
            job.progress = 36;
            job.message = "Extracting smart scene frames…".to_string();
        });
        let ffmpeg = ensure_ffmpeg()?;
        let (input, pretrimmed) = match &source {
            VideoSource::Url(url) => (
                download_url_stream_resilient(
                    app,
                    yt_dlp.as_mut().expect("yt-dlp exists for URL"),
                    &ffmpeg,
                    url,
                    &directory,
                    cancel,
                    UrlDownloadPlan {
                        visual: true,
                        range: MediaRange { start, end },
                    },
                )?,
                start > 0.0 || end.is_some(),
            ),
            VideoSource::Local(path) => (path.clone(), false),
        };
        (frame_paths, frame_times) = extract_frames(
            &ffmpeg,
            &input,
            &directory,
            cancel,
            MediaRange { start, end },
            FrameExtractionPlan {
                source_pretrimmed: pretrimmed,
                duration_seconds: metadata.duration_seconds,
                max_frames,
            },
        )?;
    }

    let mut audio_paths = Vec::<PathBuf>::new();
    if mode.needs_audio_fallback() {
        let selected = duration_selected.unwrap_or(MAX_AUDIO_FALLBACK_SECONDS + 1.0);
        let audio_in_range = end.is_some() || selected <= MAX_AUDIO_FALLBACK_SECONDS;
        if transcript.is_none() && !audio_in_range {
            return Err("No captions were available and the requested audio is over one hour. Specify a start/end range up to one hour so RepoTunnel can send compact audio to the AI efficiently.".to_string());
        }

        if audio_in_range {
            check_cancel(cancel)?;
            let has_transcript = transcript.is_some();
            update_job(job_id, |job| {
                job.phase = "audio".to_string();
                job.progress = 70;
                job.message = if has_transcript {
                    "Preparing compact audio context alongside captions…".to_string()
                } else {
                    "No usable captions found; preparing compact audio for AI transcription…"
                        .to_string()
                };
            });
            let ffmpeg = ensure_ffmpeg()?;
            let (input, pretrimmed) = match &source {
                VideoSource::Url(url) => (
                    download_url_stream_resilient(
                        app,
                        yt_dlp.as_mut().expect("yt-dlp exists for URL"),
                        &ffmpeg,
                        url,
                        &directory,
                        cancel,
                        UrlDownloadPlan {
                            visual: false,
                            range: MediaRange { start, end },
                        },
                    )?,
                    start > 0.0 || end.is_some(),
                ),
                VideoSource::Local(path) => (path.clone(), false),
            };
            audio_paths =
                extract_audio_chunks(&ffmpeg, &input, &directory, cancel, start, end, pretrimmed)?;
        }
    }

    check_cancel(cancel)?;
    update_job(job_id, |job| {
        job.phase = "cache".to_string();
        job.progress = 92;
        job.message = "Finalizing reusable video cache…".to_string();
    });

    let frame_infos = frame_times
        .into_iter()
        .enumerate()
        .map(|(index, timestamp_seconds)| VideoFrameInfo {
            index,
            timestamp_seconds,
        })
        .collect::<Vec<_>>();
    let result = VideoAnalysisResult {
        job_id: job_id.to_string(),
        cache_key: key.clone(),
        source: source_text.trim().to_string(),
        source_kind: source_kind_name,
        mode: mode.as_str().to_string(),
        title: metadata.title,
        duration_seconds: metadata.duration_seconds,
        analysis_start_seconds: start,
        analysis_end_seconds: end,
        transcript,
        transcript_source,
        frames: frame_infos,
        audio_chunk_count: audio_paths.len(),
        cache_hit: false,
        completed_at: now_millis(),
    };
    let frame_files = frame_paths
        .iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    let audio_files = audio_paths
        .iter()
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    save_cached(
        &directory,
        &CachedVideoResult {
            result: result.clone(),
            frame_files,
            audio_files,
        },
    )?;
    remove_temporary_media(&directory);
    prune_cache(app, &directory);
    Ok(result)
}

pub(crate) fn start_analysis(
    app: AppHandle,
    workspace: Workspace,
    source: String,
    mode: String,
    start_seconds: Option<f64>,
    end_seconds: Option<f64>,
    max_frames: Option<usize>,
) -> Result<VideoAnalysisJob, String> {
    let mode = VideoMode::parse(&mode)?;
    source_kind(&source)?;
    let (start, end) = validate_range(start_seconds, end_seconds)?;
    let max_frames = max_frames.unwrap_or(DEFAULT_FRAMES).clamp(1, MAX_FRAMES);

    let id = new_job_id();
    let now = now_millis();
    let cancel = Arc::new(AtomicBool::new(false));
    let job = VideoAnalysisJob {
        id: id.clone(),
        workspace_id: workspace.id.clone(),
        source: source.trim().to_string(),
        mode: mode.as_str().to_string(),
        status: "queued".to_string(),
        phase: "queued".to_string(),
        progress: 0,
        message: "Queued for background video analysis.".to_string(),
        title: None,
        duration_seconds: None,
        transcript_available: false,
        frame_count: 0,
        audio_chunk_count: 0,
        cache_key: None,
        cache_hit: false,
        created_at: now,
        updated_at: now,
    };

    {
        let mut guard = jobs()
            .lock()
            .map_err(|_| "Video job state is unavailable.".to_string())?;
        if guard.len() >= MAX_JOBS {
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
            removable.sort_by_key(|item| item.1);
            for (old_id, _) in removable.into_iter().take(guard.len() - MAX_JOBS + 1) {
                guard.remove(&old_id);
            }
        }
        guard.insert(
            id.clone(),
            VideoJobRuntime {
                job: job.clone(),
                cancel: Arc::clone(&cancel),
            },
        );
    }

    let thread_id = id.clone();
    let thread_source = source.clone();
    thread::Builder::new()
        .name("repotunnel-video-analysis".to_string())
        .spawn(move || {
            update_job(&thread_id, |job| {
                job.status = "running".to_string();
                job.phase = "starting".to_string();
                job.progress = 2;
                job.message = "Video analysis started in the background.".to_string();
            });
            match analyze_internal(
                &app,
                &workspace,
                &thread_id,
                &thread_source,
                AnalysisPlan {
                    mode,
                    range: MediaRange { start, end },
                    max_frames,
                },
                cancel.as_ref(),
            ) {
                Ok(result) => job_completed_from_result(&thread_id, &result),
                Err(error)
                    if cancel.load(Ordering::Relaxed) || error == "Video analysis cancelled." =>
                {
                    update_job(&thread_id, |job| {
                        job.status = "cancelled".to_string();
                        job.phase = "cancelled".to_string();
                        job.message = "Video analysis cancelled.".to_string();
                    });
                }
                Err(error) => {
                    update_job(&thread_id, |job| {
                        job.status = "failed".to_string();
                        job.phase = "failed".to_string();
                        job.message = error;
                    });
                }
            }
        })
        .map_err(|error| {
            if let Ok(mut guard) = jobs().lock() {
                guard.remove(&id);
            }
            format!("Could not start the background video worker: {error}")
        })?;

    Ok(job)
}

pub(crate) fn get_job(job_id: &str) -> Result<VideoAnalysisJob, String> {
    let guard = jobs()
        .lock()
        .map_err(|_| "Video job state is unavailable.".to_string())?;
    guard
        .get(job_id)
        .map(|runtime| runtime.job.clone())
        .ok_or_else(|| "Video analysis job was not found in this RepoTunnel session.".to_string())
}

pub(crate) fn list_jobs(
    workspace_id: Option<&str>,
    limit: usize,
) -> Result<Vec<VideoAnalysisJob>, String> {
    let guard = jobs()
        .lock()
        .map_err(|_| "Video job state is unavailable.".to_string())?;
    let mut items = guard
        .values()
        .map(|runtime| runtime.job.clone())
        .filter(|job| workspace_id.is_none_or(|id| job.workspace_id == id))
        .collect::<Vec<_>>();
    items.sort_by_key(|item| std::cmp::Reverse(item.created_at));
    items.truncate(limit.clamp(1, 50));
    Ok(items)
}

pub(crate) fn cancel_analysis(job_id: &str) -> Result<VideoAnalysisJob, String> {
    let cancel = {
        let mut guard = jobs()
            .lock()
            .map_err(|_| "Video job state is unavailable.".to_string())?;
        let runtime = guard
            .get_mut(job_id)
            .ok_or_else(|| "Video analysis job was not found.".to_string())?;
        if matches!(
            runtime.job.status.as_str(),
            "completed" | "failed" | "cancelled"
        ) {
            return Ok(runtime.job.clone());
        }
        runtime.job.message = "Cancelling video analysis…".to_string();
        runtime.job.updated_at = now_millis();
        Arc::clone(&runtime.cancel)
    };
    cancel.store(true, Ordering::Relaxed);
    get_job(job_id)
}

fn cached_for_job(app: &AppHandle, job_id: &str) -> Result<(PathBuf, CachedVideoResult), String> {
    let job = get_job(job_id)?;
    if job.status != "completed" {
        return Err("Video analysis is not complete yet.".to_string());
    }
    let key = job
        .cache_key
        .ok_or_else(|| "Completed video analysis is missing its cache key.".to_string())?;
    let directory = cache_root(app)?.join(key);
    let cached = load_cached(&directory)
        .ok_or_else(|| "Video analysis cache is no longer available.".to_string())?;
    Ok((directory, cached))
}

pub(crate) fn get_result(app: &AppHandle, job_id: &str) -> Result<VideoAnalysisResult, String> {
    let (_, mut cached) = cached_for_job(app, job_id)?;
    cached.result.job_id = job_id.to_string();
    cached.result.cache_hit = get_job(job_id)?.cache_hit;
    Ok(cached.result)
}

pub(crate) fn mcp_payload(app: &AppHandle, job_id: &str) -> Result<VideoMcpPayload, String> {
    let (directory, mut cached) = cached_for_job(app, job_id)?;
    cached.result.job_id = job_id.to_string();
    cached.result.cache_hit = get_job(job_id)?.cache_hit;

    let mut frames = Vec::new();
    for (index, name) in cached.frame_files.iter().enumerate() {
        let path = directory.join(name);
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("Could not inspect cached video frame: {error}"))?;
        if metadata.len() > MAX_FRAME_BYTES {
            return Err("A cached video frame exceeded the MCP image safety limit.".to_string());
        }
        let bytes = fs::read(&path)
            .map_err(|error| format!("Could not read cached video frame: {error}"))?;
        let info = cached
            .result
            .frames
            .get(index)
            .cloned()
            .unwrap_or(VideoFrameInfo {
                index,
                timestamp_seconds: cached.result.analysis_start_seconds,
            });
        frames.push((
            info,
            BASE64_STANDARD.encode(bytes),
            "image/jpeg".to_string(),
        ));
    }

    let mut audio = Vec::new();
    let mut total = 0_u64;
    for (index, name) in cached.audio_files.iter().enumerate() {
        let path = directory.join(name);
        let bytes = fs::read(&path)
            .map_err(|error| format!("Could not read cached video audio: {error}"))?;
        total = total.saturating_add(bytes.len() as u64);
        if total > MAX_AUDIO_TOTAL_BYTES {
            return Err("Prepared video audio exceeds the MCP audio safety limit. Analyze a shorter time range.".to_string());
        }
        let mime = match path.extension().and_then(|extension| extension.to_str()) {
            Some(extension) if extension.eq_ignore_ascii_case("m4a") => "audio/mp4",
            _ => "audio/ogg",
        };
        audio.push((index, BASE64_STANDARD.encode(bytes), mime.to_string()));
    }

    Ok(VideoMcpPayload {
        result: cached.result,
        frames,
        audio,
    })
}

pub(crate) fn clear_cache(app: &AppHandle) -> Result<VideoToolsStatus, String> {
    if jobs()
        .lock()
        .map_err(|_| "Video job state is unavailable.".to_string())?
        .values()
        .any(|runtime| matches!(runtime.job.status.as_str(), "queued" | "running"))
    {
        return Err(
            "Wait for active video analysis to finish or cancel it before clearing the cache."
                .to_string(),
        );
    }
    let root = cache_root(app)?;
    for entry in fs::read_dir(&root)
        .map_err(|error| format!("Could not inspect the video cache: {error}"))?
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(path)
                .map_err(|error| format!("Could not clear the video cache: {error}"))?;
        } else {
            let _ = fs::remove_file(path);
        }
    }
    tools_status(app)
}

pub(crate) fn stop_all_activity() {
    let Ok(guard) = jobs().lock() else {
        return;
    };
    for runtime in guard.values() {
        if matches!(runtime.job.status.as_str(), "queued" | "running") {
            runtime.cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command, sync::atomic::AtomicBool};

    use super::{
        extract_audio_chunks, extract_frames, find_on_path, local_metadata, now_millis,
        parse_ffmpeg_duration, parse_vtt, source_kind, FrameExtractionPlan, MediaRange, VideoMode,
    };

    #[test]
    fn accepts_only_public_http_video_urls_or_workspace_paths() {
        assert_eq!(source_kind("https://example.com/video").unwrap(), "url");
        assert_eq!(source_kind("https://8.8.8.8/video.mp4").unwrap(), "url");
        assert_eq!(source_kind("media/demo.mp4").unwrap(), "workspace");

        for blocked in [
            "http://localhost/video",
            "http://localhost.localdomain/video",
            "http://service.local/video",
            "http://127.0.0.1/video",
            "http://10.0.0.8/video",
            "http://172.16.1.2/video",
            "http://192.168.1.2/video",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/video",
            "http://[::1]/video",
            "http://[fc00::1]/video",
            "http://[fe80::1]/video",
            "http://[::ffff:127.0.0.1]/video",
        ] {
            assert!(source_kind(blocked).is_err(), "{blocked} must be rejected");
        }

        assert!(source_kind("file:///etc/passwd").is_err());
        assert!(source_kind("javascript:alert(1)").is_err());
    }

    #[test]
    fn parses_timestamped_vtt_and_range() {
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nHello world\n\n00:00:05.000 --> 00:00:07.000\nSecond line\n";
        assert_eq!(
            parse_vtt(vtt, 0.0, None).unwrap(),
            "[00:00:01] Hello world\n[00:00:05] Second line\n"
        );
        assert_eq!(
            parse_vtt(vtt, 4.0, Some(8.0)).unwrap(),
            "[00:00:05] Second line\n"
        );
    }

    #[test]
    fn parses_ffmpeg_duration_without_decoding_media() {
        let stderr = "Input #0\n  Duration: 01:02:03.50, start: 0.000000, bitrate: 1 kb/s\n";
        assert_eq!(parse_ffmpeg_duration(stderr), Some(3723.5));
    }

    #[test]
    fn video_modes_keep_full_context_available() {
        assert!(VideoMode::parse("transcript").unwrap().needs_frames());
        assert!(VideoMode::parse("visual").unwrap().needs_frames());
        assert!(VideoMode::parse("visual").unwrap().needs_audio_fallback());
        assert!(VideoMode::parse("instruction")
            .unwrap()
            .needs_audio_fallback());
        assert!(VideoMode::parse("unknown").is_err());
    }

    #[test]
    fn local_media_pipeline_smoke_test_when_ffmpeg_is_available() {
        let Some(ffmpeg) = find_on_path("ffmpeg") else {
            return;
        };

        let root = std::env::temp_dir().join(format!(
            "repotunnel-video-smoke-{}-{}",
            std::process::id(),
            now_millis()
        ));
        fs::create_dir_all(&root).unwrap();
        let input = root.join("sample.mp4");

        let status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:sample_rate=16000",
                "-t",
                "3",
                "-c:v",
                "mpeg4",
                "-q:v",
                "8",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&input)
            .status()
            .unwrap();
        assert!(status.success());

        let cancel = AtomicBool::new(false);
        let metadata = local_metadata(&ffmpeg, &input, &root, &cancel).unwrap();
        let duration = metadata.duration_seconds.unwrap();
        assert!((2.5..=3.5).contains(&duration));

        let frames_dir = root.join("frames");
        fs::create_dir_all(&frames_dir).unwrap();
        let (frames, timestamps) = extract_frames(
            &ffmpeg,
            &input,
            &frames_dir,
            &cancel,
            MediaRange {
                start: 0.0,
                end: None,
            },
            FrameExtractionPlan {
                source_pretrimmed: false,
                duration_seconds: metadata.duration_seconds,
                max_frames: 4,
            },
        )
        .unwrap();
        assert!(!frames.is_empty());
        assert!(frames.len() <= 4);
        assert_eq!(frames.len(), timestamps.len());
        assert!(frames
            .iter()
            .all(|path| fs::metadata(path).unwrap().len() > 0));

        let audio_dir = root.join("audio");
        fs::create_dir_all(&audio_dir).unwrap();
        let audio =
            extract_audio_chunks(&ffmpeg, &input, &audio_dir, &cancel, 0.0, None, false).unwrap();
        assert!(!audio.is_empty());
        assert!(audio
            .iter()
            .all(|path| fs::metadata(path).unwrap().len() > 0));

        fs::remove_dir_all(root).unwrap();
    }
}
