use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bzip2::read::BzDecoder;
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};
use tar::Archive;
use tauri::{path::BaseDirectory, AppHandle, Manager};
use url::Url;

const NARRATION_DIR: &str = "video-narration";
const SHERPA_VERSION: &str = "1.13.8";
const SUPERTONIC_VERSION: &str = "2026-05-11";
const SUPERTONIC_ARCHIVE: &str = "sherpa-onnx-supertonic-3-tts-int8-2026-05-11.tar.bz2";
const SUPERTONIC_SHA256: &str = "82fa96f91c4ef8abaae3a14a3f4153facf88bed821d1f7331cec2700f432c427";
const MAX_DOWNLOAD_BYTES: u64 = 220 * 1024 * 1024;
const SUPPORTED_LANGUAGES: &[&str] = &[
    "en", "ko", "ja", "ar", "bg", "cs", "da", "de", "el", "es", "et", "fi", "fr", "hi", "hr", "hu",
    "id", "it", "lt", "lv", "nl", "pl", "pt", "ro", "ru", "sk", "sl", "sv", "tr", "uk", "vi",
];

#[derive(Clone, Copy)]
struct RuntimeAsset {
    archive: &'static str,
    sha256: &'static str,
}

#[derive(Clone, Debug)]
pub(crate) struct ManagedNarrator {
    pub(crate) binary: PathBuf,
    pub(crate) runtime_root: PathBuf,
    pub(crate) model_root: PathBuf,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn runtime_asset() -> Result<RuntimeAsset, String> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok(RuntimeAsset {
            archive: "sherpa-onnx-v1.13.8-linux-x64-shared.tar.bz2",
            sha256: "c0bdb7907d3a74bba1d55d22bf4d9fa75586cf1530614ebe88a27b9118e015c4",
        }),
        ("linux", "aarch64") => Ok(RuntimeAsset {
            archive: "sherpa-onnx-v1.13.8-linux-aarch64-shared-cpu.tar.bz2",
            sha256: "4e3734f82bc1379fd91f219f5869c7e9d03b7a4f7561907d8abca4849c51a789",
        }),
        ("macos", "x86_64") => Ok(RuntimeAsset {
            archive: "sherpa-onnx-v1.13.8-osx-x64-shared.tar.bz2",
            sha256: "54aad64acee9d2d596535a6080d6f22602a720af5460e1d50461b1e1b06bee40",
        }),
        ("macos", "aarch64") => Ok(RuntimeAsset {
            archive: "sherpa-onnx-v1.13.8-osx-arm64-shared.tar.bz2",
            sha256: "b10e5c7e2c30ea03de9c442655d14860d9edc475c6251d58a8f5f06e913a1d56",
        }),
        ("windows", "x86_64") => Ok(RuntimeAsset {
            archive: "sherpa-onnx-v1.13.8-win-x64-shared-MT-Release.tar.bz2",
            sha256: "6dffdc715a4465b989446a6105265d2cb345e7101591a17d35534b6758f6e8df",
        }),
        ("windows", "aarch64") => Ok(RuntimeAsset {
            archive: "sherpa-onnx-v1.13.8-win-arm64-shared-MT-Release.tar.bz2",
            sha256: "455ea18dc44aea188b4f7976037dea3d77e3f8d7afe0760f34244a7a0db53c4c",
        }),
        _ => Err(format!(
            "Managed neural narration is not yet packaged for {} {}.",
            env::consts::OS,
            env::consts::ARCH
        )),
    }
}

pub(crate) fn platform_supported() -> bool {
    runtime_asset().is_ok()
}

fn data_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(NARRATION_DIR, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve private narration data: {error}"))
}

fn protect_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create private narration directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect narration directory: {error}"))?;
    }
    Ok(())
}

fn protect_file(path: &Path, executable: bool) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
        )
        .map_err(|error| format!("Could not protect narration file: {error}"))?;
    }
    Ok(())
}

fn trusted_host(host: &str) -> bool {
    matches!(
        host,
        "github.com" | "release-assets.githubusercontent.com" | "objects.githubusercontent.com"
    )
}

fn download_client() -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().host_str().is_some_and(trusted_host) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .user_agent("RepoTunnel/0.3.1 Video Production Narration")
        .build()
        .map_err(|error| format!("Could not initialize narration downloader: {error}"))
}

fn download_verified(url: &str, expected_sha256: &str, destination: &Path) -> Result<(), String> {
    let parsed =
        Url::parse(url).map_err(|_| "Managed narration download URL is invalid.".to_string())?;
    if parsed.scheme() != "https" || !parsed.host_str().is_some_and(trusted_host) {
        return Err("Managed narration downloads require an allowlisted HTTPS host.".to_string());
    }

    let client = download_client()?;
    let mut response = client
        .get(url)
        .send()
        .map_err(|error| format!("Could not download managed narration component: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Managed narration download failed with HTTP {}.",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_DOWNLOAD_BYTES)
    {
        return Err("Managed narration download exceeds RepoTunnel's safety limit.".to_string());
    }

    let parent = destination
        .parent()
        .ok_or_else(|| "Managed narration download has no parent directory.".to_string())?;
    protect_dir(parent)?;
    let temporary = parent.join(format!(".download-{:x}.tmp", now_millis()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Could not create narration download file: {error}"))?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|error| format!("Could not read managed narration download: {error}"))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_DOWNLOAD_BYTES {
            let _ = fs::remove_file(&temporary);
            return Err(
                "Managed narration download exceeds RepoTunnel's safety limit.".to_string(),
            );
        }
        digest.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| format!("Could not save managed narration download: {error}"))?;
    }
    file.sync_all()
        .map_err(|error| format!("Could not flush managed narration download: {error}"))?;

    let actual = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual != expected_sha256 {
        let _ = fs::remove_file(&temporary);
        return Err("Managed narration SHA-256 verification failed.".to_string());
    }
    fs::rename(&temporary, destination)
        .map_err(|error| format!("Could not finalize managed narration download: {error}"))?;
    protect_file(destination, false)
}

fn safe_archive_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn extract_verified_archive(archive_path: &Path, destination: &Path) -> Result<(), String> {
    protect_dir(destination)?;
    let file = File::open(archive_path)
        .map_err(|error| format!("Could not open narration archive: {error}"))?;
    let decoder = BzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|error| format!("Could not inspect narration archive: {error}"))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|error| format!("Could not read narration archive entry: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("Narration archive path is invalid: {error}"))?
            .into_owned();
        if !safe_archive_path(&path) {
            return Err("Narration archive contains an unsafe path.".to_string());
        }
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(
                "Narration archive contains a link or unsupported special entry.".to_string(),
            );
        }
        let output = destination.join(&path);
        if kind.is_dir() {
            protect_dir(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            protect_dir(parent)?;
        }
        entry
            .unpack(&output)
            .map_err(|error| format!("Could not extract narration archive entry: {error}"))?;
        protect_file(&output, false)?;
    }
    Ok(())
}

fn find_named(root: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).ok()?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_file() && path.file_name().and_then(|value| value.to_str()) == Some(name) {
            return Some(path);
        }
        if metadata.is_dir() {
            if let Some(found) = find_named(&path, name, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "sherpa-onnx-offline-tts.exe"
    } else {
        "sherpa-onnx-offline-tts"
    }
}

fn required_model_files(root: &Path) -> Option<PathBuf> {
    let tts_json = find_named(root, "tts.json", 4)?;
    let model_root = tts_json.parent()?.to_path_buf();
    for name in [
        "duration_predictor.int8.onnx",
        "text_encoder.int8.onnx",
        "vector_estimator.int8.onnx",
        "vocoder.int8.onnx",
        "tts.json",
        "unicode_indexer.bin",
        "voice.bin",
    ] {
        if !model_root.join(name).is_file() {
            return None;
        }
    }
    Some(model_root)
}

fn runtime_dir(root: &Path) -> PathBuf {
    root.join(format!(
        "runtime-{}-{}-{}",
        SHERPA_VERSION,
        env::consts::OS,
        env::consts::ARCH
    ))
}

fn model_dir(root: &Path) -> PathBuf {
    root.join(format!("supertonic-3-int8-{SUPERTONIC_VERSION}"))
}

fn ensure_runtime(root: &Path) -> Result<PathBuf, String> {
    let asset = runtime_asset()?;
    let destination = runtime_dir(root);
    if let Some(binary) = find_named(&destination, binary_name(), 5) {
        return Ok(binary);
    }
    if destination.exists() {
        fs::remove_dir_all(&destination)
            .map_err(|error| format!("Could not refresh narration runtime: {error}"))?;
    }

    protect_dir(root)?;
    let archive_path = root.join(format!(".{}", asset.archive));
    let url = format!(
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/v{SHERPA_VERSION}/{}",
        asset.archive
    );
    download_verified(&url, asset.sha256, &archive_path)?;

    let staging = root.join(format!(".runtime-staging-{:x}", now_millis()));
    let result = extract_verified_archive(&archive_path, &staging);
    let _ = fs::remove_file(&archive_path);
    result?;

    let binary = find_named(&staging, binary_name(), 5).ok_or_else(|| {
        "Verified sherpa-onnx archive did not contain the TTS executable.".to_string()
    })?;
    protect_file(&binary, true)?;
    fs::rename(&staging, &destination)
        .map_err(|error| format!("Could not install managed narration runtime: {error}"))?;
    find_named(&destination, binary_name(), 5)
        .ok_or_else(|| "Managed narration runtime disappeared after installation.".to_string())
}

fn ensure_model(root: &Path) -> Result<PathBuf, String> {
    let destination = model_dir(root);
    if let Some(model_root) = required_model_files(&destination) {
        return Ok(model_root);
    }
    if destination.exists() {
        fs::remove_dir_all(&destination)
            .map_err(|error| format!("Could not refresh narration model: {error}"))?;
    }

    protect_dir(root)?;
    let archive_path = root.join(format!(".{SUPERTONIC_ARCHIVE}"));
    let url = format!(
        "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/{SUPERTONIC_ARCHIVE}"
    );
    download_verified(&url, SUPERTONIC_SHA256, &archive_path)?;

    let staging = root.join(format!(".model-staging-{:x}", now_millis()));
    let result = extract_verified_archive(&archive_path, &staging);
    let _ = fs::remove_file(&archive_path);
    result?;

    required_model_files(&staging).ok_or_else(|| {
        "Verified Supertonic archive is missing required model files.".to_string()
    })?;
    fs::rename(&staging, &destination)
        .map_err(|error| format!("Could not install managed narration model: {error}"))?;

    let notice = destination.join("REPOTUNNEL_MODEL_NOTICE.txt");
    fs::write(
        &notice,
        "Supertonic 3 model\nLicense: OpenRAIL-M\nUpstream: https://huggingface.co/Supertone/supertonic-3\nRepoTunnel downloads this model on demand and does not modify its weights.\n",
    )
    .map_err(|error| format!("Could not write narration model notice: {error}"))?;
    protect_file(&notice, false)?;

    required_model_files(&destination)
        .ok_or_else(|| "Managed Supertonic model disappeared after installation.".to_string())
}

pub(crate) fn is_ready(app: &AppHandle) -> bool {
    let Ok(root) = data_root(app) else {
        return false;
    };
    find_named(&runtime_dir(&root), binary_name(), 5).is_some()
        && required_model_files(&model_dir(&root)).is_some()
}

pub(crate) fn ensure(app: &AppHandle) -> Result<ManagedNarrator, String> {
    let root = data_root(app)?;
    protect_dir(&root)?;
    let binary = ensure_runtime(&root)?;
    let model_root = ensure_model(&root)?;
    let runtime_root = runtime_dir(&root);
    Ok(ManagedNarrator {
        binary,
        runtime_root,
        model_root,
    })
}

pub(crate) fn language_code(language: &str) -> Option<&'static str> {
    let base = language
        .split(['-', '_'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    SUPPORTED_LANGUAGES
        .iter()
        .copied()
        .find(|candidate| *candidate == base)
}

pub(crate) fn voice_id(value: Option<&str>) -> Result<u8, String> {
    let value = value.unwrap_or("M1").trim().to_ascii_uppercase();
    let sid = match value.as_str() {
        "M1" | "VOICE-1" | "0" => 0,
        "M2" | "VOICE-2" | "1" => 1,
        "M3" | "VOICE-3" | "2" => 2,
        "M4" | "VOICE-4" | "3" => 3,
        "M5" | "VOICE-5" | "4" => 4,
        "F1" | "VOICE-6" | "5" => 5,
        "F2" | "VOICE-7" | "6" => 6,
        "F3" | "VOICE-8" | "7" => 7,
        "F4" | "VOICE-9" | "8" => 8,
        "F5" | "VOICE-10" | "9" => 9,
        _ => {
            return Err(
                "Supertonic voice must be M1-M5, F1-F5, voice-1..voice-10, or speaker ID 0..9."
                    .to_string(),
            )
        }
    };
    Ok(sid)
}

pub(crate) fn command(
    managed: &ManagedNarrator,
    language: &str,
    voice: Option<&str>,
    speed: f64,
    text: &str,
    output: &Path,
) -> Result<Command, String> {
    let language = language_code(language).ok_or_else(|| {
        format!(
            "Managed neural narration does not currently support language {language}. Supported language families: {}.",
            SUPPORTED_LANGUAGES.join(", ")
        )
    })?;
    let sid = voice_id(voice)?;
    let speed = speed.clamp(0.65, 1.6);
    let model = &managed.model_root;

    let option = |name: &str, value: &Path| format!("--{name}={}", value.to_string_lossy());
    let mut command = Command::new(&managed.binary);
    command
        .current_dir(&managed.runtime_root)
        .arg(option(
            "supertonic-duration-predictor",
            &model.join("duration_predictor.int8.onnx"),
        ))
        .arg(option(
            "supertonic-text-encoder",
            &model.join("text_encoder.int8.onnx"),
        ))
        .arg(option(
            "supertonic-vector-estimator",
            &model.join("vector_estimator.int8.onnx"),
        ))
        .arg(option(
            "supertonic-vocoder",
            &model.join("vocoder.int8.onnx"),
        ))
        .arg(option("supertonic-tts-json", &model.join("tts.json")))
        .arg(option(
            "supertonic-unicode-indexer",
            &model.join("unicode_indexer.bin"),
        ))
        .arg(option("supertonic-voice-style", &model.join("voice.bin")))
        .arg(format!("--sid={sid}"))
        .arg(format!("--lang={language}"))
        .arg("--num-steps=8")
        .arg(format!("--speed={speed:.3}"))
        .arg(format!("--output-filename={}", output.to_string_lossy()))
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    if let Some(lib_dir) = find_named(&managed.runtime_root, "libsherpa-onnx-c-api.so", 5)
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        command.env("LD_LIBRARY_PATH", lib_dir);
    }

    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::{
        command, ensure_model, ensure_runtime, language_code, runtime_asset, runtime_dir,
        safe_archive_path, voice_id, ManagedNarrator,
    };
    use std::{fs, path::Path};

    #[test]
    fn managed_narration_runtime_is_defined_for_supported_build_platform() {
        if matches!(std::env::consts::OS, "linux" | "macos" | "windows")
            && matches!(std::env::consts::ARCH, "x86_64" | "aarch64")
        {
            let asset = runtime_asset().unwrap();
            assert!(asset.archive.ends_with(".tar.bz2"));
            assert_eq!(asset.sha256.len(), 64);
        }
    }

    #[test]
    fn supertonic_language_mapping_preserves_bcp47_families() {
        assert_eq!(language_code("en-US"), Some("en"));
        assert_eq!(language_code("hi-IN"), Some("hi"));
        assert_eq!(language_code("pt_BR"), Some("pt"));
        assert_eq!(language_code("te-IN"), None);
    }

    #[test]
    fn supertonic_voice_aliases_are_bounded() {
        assert_eq!(voice_id(None).unwrap(), 0);
        assert_eq!(voice_id(Some("M1")).unwrap(), 0);
        assert_eq!(voice_id(Some("F5")).unwrap(), 9);
        assert_eq!(voice_id(Some("9")).unwrap(), 9);
        assert!(voice_id(Some("voice-11")).is_err());
    }

    #[test]
    fn managed_archive_paths_cannot_escape_private_storage() {
        assert!(safe_archive_path(Path::new("runtime/bin/tool")));
        assert!(!safe_archive_path(Path::new("../escape")));
        assert!(!safe_archive_path(Path::new("/tmp/escape")));
    }

    #[test]
    #[ignore = "downloads the pinned sherpa-onnx runtime and Supertonic model"]
    fn managed_supertonic_provisions_and_synthesizes_english_and_hindi() {
        if runtime_asset().is_err() {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let binary = ensure_runtime(temp.path()).unwrap();
        let model_root = ensure_model(temp.path()).unwrap();
        let managed = ManagedNarrator {
            binary,
            runtime_root: runtime_dir(temp.path()),
            model_root,
        };

        for (language, voice, text, file_name) in [
            (
                "en-US",
                "M1",
                "RepoTunnel can create clear tutorial narration locally.",
                "english.wav",
            ),
            (
                "hi-IN",
                "F1",
                "रिपो टनल स्थानीय रूप से स्पष्ट वीडियो वर्णन बना सकता है।",
                "hindi.wav",
            ),
        ] {
            let output = temp.path().join(file_name);
            let result = command(&managed, language, Some(voice), 1.0, text, &output)
                .unwrap()
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{} narration failed: {}",
                language,
                String::from_utf8_lossy(&result.stderr)
            );
            let bytes = fs::read(&output).unwrap();
            assert!(bytes.len() > 44, "{language} WAV was empty");
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(&bytes[8..12], b"WAVE");
        }
    }
}
