use std::{
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::{
    access::AccessOperation, models::Workspace, video_narration_managed, video_production,
};

const MAX_NARRATION_CHARS: usize = 120_000;
const MAX_SUBTITLE_CUES: usize = 4_000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NarrationProviderStatus {
    pub(crate) id: String,
    pub(crate) available: bool,
    pub(crate) quality: String,
    pub(crate) languages: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubtitleCue {
    pub(crate) start_seconds: f64,
    pub(crate) end_seconds: f64,
    pub(crate) text: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubtitleAsset {
    pub(crate) project_id: String,
    pub(crate) language: String,
    pub(crate) srt_path: String,
    pub(crate) vtt_path: String,
    pub(crate) cue_count: usize,
    pub(crate) duration_seconds: f64,
}

#[derive(Clone, Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NarrationRequest {
    pub(crate) text: String,
    pub(crate) language: String,
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) voice: Option<String>,
    #[serde(default)]
    pub(crate) voice_model_path: Option<String>,
    #[serde(default)]
    pub(crate) rate: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NarrationAsset {
    pub(crate) project_id: String,
    pub(crate) provider: String,
    pub(crate) language: String,
    pub(crate) voice: Option<String>,
    pub(crate) audio_path: String,
    pub(crate) duration_seconds: f64,
    pub(crate) subtitles: SubtitleAsset,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let candidate = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    env::var_os("PATH")
        .into_iter()
        .flat_map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join(&candidate))
        .find(|path| path.is_file())
}

fn python_piper_available() -> bool {
    let Some(python) = find_on_path("python3").or_else(|| find_on_path("python")) else {
        return false;
    };
    Command::new(python)
        .args(["-c", "import piper"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub(crate) fn provider_status(app: &AppHandle) -> Vec<NarrationProviderStatus> {
    let managed_supported = video_narration_managed::platform_supported();
    let managed_ready = managed_supported && video_narration_managed::is_ready(app);
    vec![
        NarrationProviderStatus {
            id: "supertonic-3".to_string(),
            available: managed_supported,
            quality: "neural".to_string(),
            languages: "31 local languages: en, ko, ja, ar, bg, cs, da, de, el, es, et, fi, fr, hi, hr, hu, id, it, lt, lv, nl, pl, pt, ro, ru, sk, sl, sv, tr, uk, vi".to_string(),
            message: if managed_ready {
                "RepoTunnel-managed Supertonic 3 is ready for private offline narration."
                    .to_string()
            } else if managed_supported {
                "High-quality RepoTunnel-managed neural narration. The verified runtime/model are downloaded privately on first use; no admin install or PATH change is required."
                    .to_string()
            } else {
                "Managed Supertonic 3 is not packaged for this OS/architecture.".to_string()
            },
        },
        NarrationProviderStatus {
            id: "piper".to_string(),
            available: find_on_path("piper").is_some() || python_piper_available(),
            quality: "neural".to_string(),
            languages: "project voice-model dependent; multilingual catalog".to_string(),
            message: "Optional project-supplied Piper voice/model fallback.".to_string(),
        },
        NarrationProviderStatus {
            id: "espeak-ng".to_string(),
            available: find_on_path("espeak-ng").is_some(),
            quality: "fallback".to_string(),
            languages: "broad multilingual fallback".to_string(),
            message: "Broad-language offline fallback when installed. RepoTunnel does not silently choose it over a supported neural voice."
                .to_string(),
        },
    ]
}

fn validate_language(language: &str) -> Result<String, String> {
    let language = language.trim();
    if language.is_empty() || language.len() > 48 {
        return Err("Narration language must be a BCP-47 style language tag.".to_string());
    }
    if !language
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err("Narration language contains unsupported characters.".to_string());
    }
    Ok(language.to_string())
}

fn subtitle_slug(language: &str) -> String {
    language.to_ascii_lowercase().replace('-', "_")
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if ch == '\n' || matches!(ch, '.' | '!' | '?' | '।' | '。' | '！' | '？') {
            let trimmed = current.trim();
            if !trimmed.is_empty() {
                output.push(trimmed.to_string());
            }
            current.clear();
        }
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        output.push(trimmed.to_string());
    }
    output
}

fn estimated_duration(text: &str) -> f64 {
    let words = text.split_whitespace().count().max(1) as f64;
    (words / 2.45).clamp(0.7, 60.0 * 60.0)
}

fn cues_from_text(text: &str, duration_seconds: Option<f64>) -> Result<Vec<SubtitleCue>, String> {
    let sentences = split_sentences(text);
    if sentences.is_empty() {
        return Err("Narration text does not contain any spoken content.".to_string());
    }
    if sentences.len() > MAX_SUBTITLE_CUES {
        return Err("Narration creates too many subtitle cues.".to_string());
    }

    let weights = sentences
        .iter()
        .map(|sentence| sentence.split_whitespace().count().max(1) as f64)
        .collect::<Vec<_>>();
    let total_weight = weights.iter().sum::<f64>().max(1.0);
    let duration = duration_seconds
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or_else(|| estimated_duration(text));

    let mut cursor = 0.0;
    let mut cues = Vec::with_capacity(sentences.len());
    for (index, sentence) in sentences.into_iter().enumerate() {
        let mut span = duration * (weights[index] / total_weight);
        span = span.max(0.55);
        let end = if index + 1 == weights.len() {
            duration.max(cursor + 0.55)
        } else {
            (cursor + span).min(duration)
        };
        cues.push(SubtitleCue {
            start_seconds: cursor,
            end_seconds: end.max(cursor + 0.25),
            text: sentence,
        });
        cursor = end;
    }
    Ok(cues)
}

fn srt_time(seconds: f64) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let millis = total_ms % 1000;
    let total_seconds = total_ms / 1000;
    let secs = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    let mins = total_minutes % 60;
    let hours = total_minutes / 60;
    format!("{hours:02}:{mins:02}:{secs:02},{millis:03}")
}

fn vtt_time(seconds: f64) -> String {
    srt_time(seconds).replace(',', ".")
}

fn subtitle_text_srt(cues: &[SubtitleCue]) -> String {
    let mut output = String::new();
    for (index, cue) in cues.iter().enumerate() {
        output.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            index + 1,
            srt_time(cue.start_seconds),
            srt_time(cue.end_seconds),
            cue.text.trim()
        ));
    }
    output
}

fn subtitle_text_vtt(cues: &[SubtitleCue]) -> String {
    let mut output = String::from("WEBVTT\n\n");
    for cue in cues {
        output.push_str(&format!(
            "{} --> {}\n{}\n\n",
            vtt_time(cue.start_seconds),
            vtt_time(cue.end_seconds),
            cue.text.trim()
        ));
    }
    output
}

fn write_subtitles(
    workspace: &Workspace,
    project_id: &str,
    language: &str,
    cues: &[SubtitleCue],
) -> Result<SubtitleAsset, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let language = validate_language(language)?;
    let slug = subtitle_slug(&language);
    let stamp = now_millis();
    let srt_relative = format!("{}/subtitles/{slug}-{stamp}.srt", project.relative_path);
    let vtt_relative = format!("{}/subtitles/{slug}-{stamp}.vtt", project.relative_path);
    let srt = video_production::resolve_project_path(
        workspace,
        &project,
        &srt_relative,
        AccessOperation::Write,
        false,
    )?;
    let vtt = video_production::resolve_project_path(
        workspace,
        &project,
        &vtt_relative,
        AccessOperation::Write,
        false,
    )?;

    fs::write(&srt, subtitle_text_srt(cues))
        .map_err(|error| format!("Could not save SRT subtitles: {error}"))?;
    fs::write(&vtt, subtitle_text_vtt(cues))
        .map_err(|error| format!("Could not save VTT subtitles: {error}"))?;

    let prefix = format!("{}/", project.relative_path);
    let srt_asset = srt_relative
        .strip_prefix(&prefix)
        .ok_or_else(|| "SRT subtitle path escaped its Video Project.".to_string())?;
    let vtt_asset = vtt_relative
        .strip_prefix(&prefix)
        .ok_or_else(|| "VTT subtitle path escaped its Video Project.".to_string())?;
    video_production::register_asset(
        workspace,
        project_id,
        "subtitle",
        srt_asset,
        Some(&format!("{language} SRT subtitles")),
    )?;
    video_production::register_asset(
        workspace,
        project_id,
        "subtitle",
        vtt_asset,
        Some(&format!("{language} VTT subtitles")),
    )?;

    Ok(SubtitleAsset {
        project_id: project.id,
        language,
        srt_path: srt_relative,
        vtt_path: vtt_relative,
        cue_count: cues.len(),
        duration_seconds: cues.last().map(|cue| cue.end_seconds).unwrap_or(0.0),
    })
}

pub(crate) fn create_subtitles(
    workspace: &Workspace,
    project_id: &str,
    language: &str,
    text: &str,
    duration_seconds: Option<f64>,
) -> Result<SubtitleAsset, String> {
    if text.chars().count() > MAX_NARRATION_CHARS {
        return Err("Narration text exceeds the 120,000 character safety limit.".to_string());
    }
    let cues = cues_from_text(text, duration_seconds)?;
    write_subtitles(workspace, project_id, language, &cues)
}

fn wav_duration(audio: &Path) -> Result<f64, String> {
    let mut file = fs::File::open(audio)
        .map_err(|error| format!("Could not open generated narration WAV: {error}"))?;
    let mut header = [0_u8; 12];
    file.read_exact(&mut header)
        .map_err(|error| format!("Could not read generated narration WAV header: {error}"))?;
    if &header[..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Err("Narration provider produced an invalid WAV file.".to_string());
    }

    let mut byte_rate = None;
    let mut data_bytes = None;
    loop {
        let mut chunk = [0_u8; 8];
        match file.read_exact(&mut chunk) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => {
                return Err(format!(
                    "Could not inspect generated narration WAV: {error}"
                ));
            }
        }
        let size = u32::from_le_bytes(chunk[4..8].try_into().unwrap()) as u64;
        match &chunk[..4] {
            b"fmt " => {
                if size < 16 {
                    return Err("Narration WAV has an invalid format chunk.".to_string());
                }
                let mut format = [0_u8; 16];
                file.read_exact(&mut format)
                    .map_err(|error| format!("Could not read narration WAV format: {error}"))?;
                let rate = u32::from_le_bytes(format[8..12].try_into().unwrap());
                if rate == 0 {
                    return Err("Narration WAV has an invalid byte rate.".to_string());
                }
                byte_rate = Some(rate as u64);
                if size > 16 {
                    file.seek(SeekFrom::Current((size - 16) as i64))
                        .map_err(|error| {
                            format!("Could not skip narration WAV metadata: {error}")
                        })?;
                }
            }
            b"data" => {
                data_bytes = Some(size);
                file.seek(SeekFrom::Current(size as i64))
                    .map_err(|error| format!("Could not skip narration WAV samples: {error}"))?;
            }
            _ => {
                file.seek(SeekFrom::Current(size as i64))
                    .map_err(|error| format!("Could not skip narration WAV chunk: {error}"))?;
            }
        }
        if size % 2 == 1 {
            file.seek(SeekFrom::Current(1))
                .map_err(|error| format!("Could not align narration WAV chunk: {error}"))?;
        }
        if byte_rate.is_some() && data_bytes.is_some() {
            break;
        }
    }

    let duration = data_bytes
        .zip(byte_rate)
        .map(|(bytes, rate)| bytes as f64 / rate as f64)
        .ok_or_else(|| "Narration WAV is missing format or sample data.".to_string())?;
    if !duration.is_finite() || duration <= 0.0 {
        return Err("Narration audio has no usable duration.".to_string());
    }
    Ok(duration)
}

fn piper_command(
    request: &NarrationRequest,
    model: &Path,
    output: &Path,
) -> Result<Command, String> {
    if let Some(binary) = find_on_path("piper") {
        let mut command = Command::new(binary);
        command
            .args(["--model"])
            .arg(model)
            .args(["--output_file"])
            .arg(output)
            .arg("--")
            .arg(&request.text);
        return Ok(command);
    }

    let python = find_on_path("python3")
        .or_else(|| find_on_path("python"))
        .filter(|_| python_piper_available())
        .ok_or_else(|| "Piper is not installed on this machine.".to_string())?;
    let mut command = Command::new(python);
    command
        .args(["-m", "piper", "-m"])
        .arg(model)
        .args(["-f"])
        .arg(output)
        .arg("--")
        .arg(&request.text);
    Ok(command)
}

fn espeak_command(request: &NarrationRequest, output: &Path) -> Result<Command, String> {
    let binary = find_on_path("espeak-ng")
        .ok_or_else(|| "eSpeak NG is not installed on this machine.".to_string())?;
    let rate = request.rate.unwrap_or(1.0).clamp(0.65, 1.6);
    let words_per_minute = (175.0 * rate).round() as u32;
    let mut command = Command::new(binary);
    command
        .arg("-v")
        .arg(&request.language)
        .arg("-s")
        .arg(words_per_minute.to_string())
        .arg("-w")
        .arg(output)
        .arg(&request.text);
    Ok(command)
}

pub(crate) fn synthesize(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    request: NarrationRequest,
) -> Result<NarrationAsset, String> {
    if request.text.trim().is_empty() {
        return Err("Narration text cannot be empty.".to_string());
    }
    if request.text.chars().count() > MAX_NARRATION_CHARS {
        return Err("Narration text exceeds the 120,000 character safety limit.".to_string());
    }
    let language = validate_language(&request.language)?;
    let project = video_production::get_project(workspace, project_id)?;
    let provider = request
        .provider
        .as_deref()
        .unwrap_or("auto")
        .trim()
        .to_ascii_lowercase();

    let managed_language_supported = video_narration_managed::language_code(&language).is_some();
    let selected = match provider.as_str() {
        "auto" if managed_language_supported && video_narration_managed::platform_supported() => {
            "supertonic-3"
        }
        "auto"
            if request.voice_model_path.is_some()
                && (find_on_path("piper").is_some() || python_piper_available()) =>
        {
            "piper"
        }
        "auto" => {
            return Err(format!(
                "No high-quality local neural narrator is configured for {language}. RepoTunnel-managed Supertonic 3 currently covers 31 language families but not this one. Supply a compatible project Piper model, explicitly request espeak-ng when installed, or import narration audio."
            ));
        }
        "supertonic-3" => {
            if !managed_language_supported {
                return Err(format!(
                    "Supertonic 3 does not support {language}. Use another configured provider for this language."
                ));
            }
            if !video_narration_managed::platform_supported() {
                return Err(
                    "RepoTunnel-managed Supertonic 3 is not packaged for this OS/architecture."
                        .to_string(),
                );
            }
            "supertonic-3"
        }
        "piper" | "espeak-ng" => provider.as_str(),
        _ => {
            return Err(
                "Narration provider must be auto, supertonic-3, piper, or espeak-ng.".to_string(),
            )
        }
    };

    if selected == "piper" && request.voice_model_path.is_none() {
        return Err(
            "Piper narration requires a voice model stored inside the Video Project.".to_string(),
        );
    }

    let stamp = now_millis();
    let audio_relative = format!(
        "{}/narration/{}-{stamp}.wav",
        project.relative_path,
        subtitle_slug(&language)
    );
    let audio = video_production::resolve_project_path(
        workspace,
        &project,
        &audio_relative,
        AccessOperation::Write,
        false,
    )?;

    let mut command = match selected {
        "supertonic-3" => {
            let managed = video_narration_managed::ensure(app)?;
            video_narration_managed::command(
                &managed,
                &language,
                request.voice.as_deref(),
                request.rate.unwrap_or(1.0),
                &request.text,
                &audio,
            )?
        }
        "piper" => {
            let model_relative = request.voice_model_path.as_deref().unwrap_or_default();
            let model = video_production::resolve_project_path(
                workspace,
                &project,
                model_relative,
                AccessOperation::Read,
                true,
            )?;
            if !model.is_file() {
                return Err("Piper voice model is not a regular project file.".to_string());
            }
            piper_command(&request, &model, &audio)?
        }
        "espeak-ng" => espeak_command(&request, &audio)?,
        _ => return Err("Internal narration provider selection failed.".to_string()),
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let result = command
        .output()
        .map_err(|error| format!("Could not start narration provider: {error}"))?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr).trim().to_string();
        let _ = fs::remove_file(&audio);
        return Err(if detail.is_empty() {
            format!("Narration provider exited with status {}.", result.status)
        } else {
            format!("Narration provider failed: {detail}")
        });
    }
    if !audio.is_file()
        || fs::metadata(&audio)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true)
    {
        return Err("Narration provider produced no usable audio.".to_string());
    }

    let duration = wav_duration(&audio)?;
    let subtitles = create_subtitles(
        workspace,
        project_id,
        &language,
        &request.text,
        Some(duration),
    )?;

    let prefix = format!("{}/", project.relative_path);
    let narration_asset = audio_relative
        .strip_prefix(&prefix)
        .ok_or_else(|| "Narration path escaped its Video Project.".to_string())?;
    video_production::register_asset(
        workspace,
        project_id,
        "narration",
        narration_asset,
        Some(&format!("{language} narration")),
    )?;
    video_production::update_project_status(
        workspace,
        project_id,
        "editing",
        Some("Narration and subtitles generated."),
    )?;

    Ok(NarrationAsset {
        project_id: project.id,
        provider: selected.to_string(),
        language,
        voice: request.voice,
        audio_path: audio_relative,
        duration_seconds: duration,
        subtitles,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        cues_from_text, srt_time, subtitle_text_srt, subtitle_text_vtt, validate_language,
    };

    #[test]
    fn multilingual_language_tags_are_not_hardcoded_to_english() {
        for language in [
            "en-US", "en-IN", "hi-IN", "te-IN", "es-ES", "zh-CN", "ar-SA",
        ] {
            assert_eq!(validate_language(language).unwrap(), language);
        }
        assert!(validate_language("../bad").is_err());
    }

    #[test]
    fn subtitle_timing_uses_requested_audio_duration() {
        let cues = cues_from_text("First sentence. Second sentence.", Some(8.0)).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues.first().unwrap().start_seconds, 0.0);
        assert!((cues.last().unwrap().end_seconds - 8.0).abs() < 0.001);
    }

    #[test]
    fn srt_and_vtt_are_generated_from_same_cues() {
        let cues = cues_from_text("Hello world. Next step!", Some(4.0)).unwrap();
        let srt = subtitle_text_srt(&cues);
        let vtt = subtitle_text_vtt(&cues);
        assert!(srt.contains("00:00:00,000"));
        assert!(vtt.starts_with("WEBVTT"));
        assert!(vtt.contains("00:00:00.000"));
    }

    #[test]
    fn subtitle_time_format_handles_hours() {
        assert_eq!(srt_time(3723.5), "01:02:03,500");
    }
}
