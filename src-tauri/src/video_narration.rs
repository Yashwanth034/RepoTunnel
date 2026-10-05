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
    access::AccessOperation, models::Workspace, video_director, video_narration_managed,
    video_production,
};

const MAX_NARRATION_CHARS: usize = 120_000;
const MAX_SUBTITLE_CUES: usize = 4_000;
const TARGET_CAPTION_WORDS: usize = 7;
const MAX_CAPTION_WORDS: usize = 10;
const MIN_CAPTION_WORDS: usize = 3;
const MAX_CAPTION_CHARS: usize = 48;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NarrationProviderStatus {
    pub(crate) id: String,
    pub(crate) available: bool,
    pub(crate) ready: bool,
    pub(crate) requires_download: bool,
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
    /// Alternate WebVTT track with per-word cue timestamps for players that support karaoke-style highlighting.
    pub(crate) word_highlight_vtt_path: String,
    pub(crate) cue_count: usize,
    pub(crate) duration_seconds: f64,
}

#[derive(Clone, Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NarrationRequest {
    pub(crate) text: String,
    pub(crate) language: String,
    #[serde(default)]
    pub(crate) scene_id: Option<String>,
    /// Story-mode character whose persisted voice cast must be used for this dialogue.
    #[serde(default)]
    pub(crate) character_id: Option<String>,
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) voice: Option<String>,
    #[serde(default)]
    pub(crate) voice_model_path: Option<String>,
    #[serde(default)]
    pub(crate) rate: Option<f64>,
    #[serde(default)]
    pub(crate) allow_managed_download: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NarrationAsset {
    pub(crate) project_id: String,
    pub(crate) provider: String,
    pub(crate) language: String,
    #[serde(default)]
    pub(crate) character_id: Option<String>,
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
            ready: managed_ready,
            requires_download: managed_supported && !managed_ready,
            quality: "neural".to_string(),
            languages: "31 local languages: en, ko, ja, ar, bg, cs, da, de, el, es, et, fi, fr, hi, hr, hu, id, it, lt, lv, nl, pl, pt, ro, ru, sk, sl, sv, tr, uk, vi".to_string(),
            message: if managed_ready {
                "RepoTunnel-managed Supertonic 3 is ready for private offline narration."
                    .to_string()
            } else if managed_supported {
                "Managed neural narration is supported but not installed. A first-time runtime/model download now requires explicit allowManagedDownload=true."
                    .to_string()
            } else {
                "Managed Supertonic 3 is not packaged for this OS/architecture.".to_string()
            },
        },
        NarrationProviderStatus {
            id: "piper".to_string(),
            available: find_on_path("piper").is_some() || python_piper_available(),
            ready: find_on_path("piper").is_some() || python_piper_available(),
            requires_download: false,
            quality: "neural".to_string(),
            languages: "project voice-model dependent; multilingual catalog".to_string(),
            message: "Optional project-supplied Piper voice/model fallback.".to_string(),
        },
        NarrationProviderStatus {
            id: "espeak-ng".to_string(),
            available: find_on_path("espeak-ng").is_some(),
            ready: find_on_path("espeak-ng").is_some(),
            requires_download: false,
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

fn clause_lead(word: &str) -> bool {
    matches!(
        word.trim_matches(|ch: char| !ch.is_alphanumeric())
            .to_ascii_lowercase()
            .as_str(),
        "and"
            | "but"
            | "or"
            | "because"
            | "while"
            | "when"
            | "where"
            | "which"
            | "who"
            | "although"
            | "though"
            | "unless"
            | "until"
            | "before"
            | "after"
            | "without"
            | "instead"
            | "then"
    )
}

fn dangling_caption_word(word: &str) -> bool {
    matches!(
        word.trim_matches(|ch: char| !ch.is_alphanumeric())
            .to_ascii_lowercase()
            .as_str(),
        "a" | "an"
            | "the"
            | "and"
            | "or"
            | "but"
            | "to"
            | "of"
            | "for"
            | "with"
            | "without"
            | "from"
            | "into"
            | "on"
            | "in"
            | "at"
            | "by"
            | "because"
            | "while"
            | "when"
            | "which"
            | "that"
    )
}

#[allow(clippy::unnecessary_map_or)]
fn split_long_caption_phrase(phrase: &str) -> Vec<String> {
    let words = phrase.split_whitespace().collect::<Vec<_>>();
    if words.len() <= MAX_CAPTION_WORDS && phrase.chars().count() <= MAX_CAPTION_CHARS {
        return vec![phrase.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < words.len() {
        let remaining = words.len() - start;
        if remaining <= MAX_CAPTION_WORDS {
            let tail = words[start..].join(" ");
            if tail.chars().count() <= MAX_CAPTION_CHARS {
                chunks.push(tail);
                break;
            }
        }

        let max_end = (start + MAX_CAPTION_WORDS).min(words.len());
        let target_end = (start + TARGET_CAPTION_WORDS).min(max_end);
        let min_end = (start + MIN_CAPTION_WORDS).min(max_end);
        let mut best = None::<(usize, usize)>;
        for end in min_end..=max_end {
            let previous = words[end - 1];
            let boundary = previous
                .chars()
                .last()
                .is_some_and(|ch| matches!(ch, ',' | ';' | ':' | '—'))
                || words.get(end).is_some_and(|word| clause_lead(word));
            if boundary && !dangling_caption_word(previous) {
                let distance = end.abs_diff(target_end);
                if best.map_or(true, |(_, score)| distance < score) {
                    best = Some((end, distance));
                }
            }
        }
        let mut end = best.map(|(end, _)| end).unwrap_or(target_end.max(min_end));
        while end > min_end && dangling_caption_word(words[end - 1]) {
            end -= 1;
        }
        while end > min_end && words[start..end].join(" ").chars().count() > MAX_CAPTION_CHARS {
            end -= 1;
        }
        if end <= start {
            end = max_end.max(start + 1);
        }
        chunks.push(words[start..end].join(" "));
        start = end;
    }
    chunks
}

fn caption_chunks(text: &str) -> Vec<String> {
    let mut output = Vec::new();
    for sentence in split_sentences(text) {
        let words = sentence.split_whitespace().collect::<Vec<_>>();
        if words.is_empty() {
            continue;
        }

        let mut phrases = Vec::<String>::new();
        let mut current = Vec::<&str>::new();
        for word in words {
            if current.len() >= MIN_CAPTION_WORDS && clause_lead(word) {
                phrases.push(current.join(" "));
                current.clear();
            }
            current.push(word);

            let punctuation_pause = word
                .chars()
                .last()
                .is_some_and(|ch| matches!(ch, ',' | ';' | ':' | '—'));
            if current.len() >= MIN_CAPTION_WORDS && punctuation_pause {
                phrases.push(current.join(" "));
                current.clear();
            }
        }
        if !current.is_empty() {
            phrases.push(current.join(" "));
        }
        let phrases = phrases
            .into_iter()
            .flat_map(|phrase| split_long_caption_phrase(&phrase))
            .collect::<Vec<_>>();

        let mut sentence_chunks = Vec::<String>::new();
        let mut carry = String::new();
        for phrase in phrases {
            let phrase_words = phrase.split_whitespace().count();
            let phrase_chars = phrase.chars().count();
            if carry.is_empty() {
                carry = phrase;
                continue;
            }

            let combined_words = carry.split_whitespace().count() + phrase_words;
            let combined_chars = carry.chars().count() + 1 + phrase_chars;
            if combined_words <= TARGET_CAPTION_WORDS && combined_chars <= MAX_CAPTION_CHARS {
                carry.push(' ');
                carry.push_str(&phrase);
            } else {
                sentence_chunks.push(carry);
                carry = phrase;
            }
        }
        if !carry.is_empty() {
            sentence_chunks.push(carry);
        }

        if sentence_chunks.len() >= 2 {
            let last_is_short = sentence_chunks
                .last()
                .is_some_and(|value| value.split_whitespace().count() < MIN_CAPTION_WORDS);
            if last_is_short {
                let last = sentence_chunks.pop().unwrap_or_default();
                if let Some(previous) = sentence_chunks.last_mut() {
                    let combined_words =
                        previous.split_whitespace().count() + last.split_whitespace().count();
                    let combined_chars = previous.chars().count() + 1 + last.chars().count();
                    if combined_words <= MAX_CAPTION_WORDS && combined_chars <= MAX_CAPTION_CHARS {
                        previous.push(' ');
                        previous.push_str(&last);
                    } else {
                        sentence_chunks.push(last);
                    }
                }
            }
        }

        for index in 0..sentence_chunks.len().saturating_sub(1) {
            let dangling = sentence_chunks[index]
                .split_whitespace()
                .last()
                .is_some_and(dangling_caption_word);
            if dangling {
                if let Some(word) = sentence_chunks[index]
                    .split_whitespace()
                    .last()
                    .map(str::to_string)
                {
                    let keep = sentence_chunks[index]
                        .split_whitespace()
                        .take(
                            sentence_chunks[index]
                                .split_whitespace()
                                .count()
                                .saturating_sub(1),
                        )
                        .collect::<Vec<_>>()
                        .join(" ");
                    sentence_chunks[index] = keep;
                    sentence_chunks[index + 1] = format!("{word} {}", sentence_chunks[index + 1]);
                }
            }
        }

        output.extend(
            sentence_chunks
                .into_iter()
                .filter(|chunk| !chunk.trim().is_empty()),
        );
    }
    output
}

pub(crate) fn caption_chunks_are_phrase_safe(chunks: &[String]) -> bool {
    chunks.iter().enumerate().all(|(index, chunk)| {
        let word_count = chunk.split_whitespace().count();
        let length_ok = word_count <= MAX_CAPTION_WORDS || chunks.len() == 1;
        let clause_ok = index + 1 == chunks.len()
            || !chunk
                .split_whitespace()
                .last()
                .is_some_and(dangling_caption_word);
        length_ok && clause_ok && chunk.chars().count() <= MAX_CAPTION_CHARS
    })
}

fn cues_from_text(text: &str, duration_seconds: Option<f64>) -> Result<Vec<SubtitleCue>, String> {
    let chunks = caption_chunks(text);
    if chunks.is_empty() {
        return Err("Narration text does not contain any spoken content.".to_string());
    }
    if chunks.len() > MAX_SUBTITLE_CUES {
        return Err("Narration creates too many subtitle cues.".to_string());
    }

    let weights = chunks
        .iter()
        .map(|chunk| chunk.split_whitespace().count().max(1) as f64)
        .collect::<Vec<_>>();
    let total_weight = weights.iter().sum::<f64>().max(1.0);
    let duration = duration_seconds
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or_else(|| estimated_duration(text));

    let mut cursor = 0.0;
    let mut cues = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.into_iter().enumerate() {
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
            text: chunk,
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
    let mut output = String::from(
        "WEBVTT\n\nSTYLE\n::cue { color: white; background-color: rgba(0,0,0,0.78); font-size: 44px; }\n\n",
    );
    for cue in cues {
        output.push_str(&format!(
            "{} --> {} line:88% position:50% align:center size:88%\n{}\n\n",
            vtt_time(cue.start_seconds),
            vtt_time(cue.end_seconds),
            cue.text.trim()
        ));
    }
    output
}

fn subtitle_text_vtt_word_highlight(cues: &[SubtitleCue]) -> String {
    let mut output = String::from(
        "WEBVTT\n\nSTYLE\n::cue { color: white; background-color: rgba(0,0,0,0.78); font-size: 44px; }\n::cue(:past) { color: #66e3ff; }\n::cue(:future) { color: white; }\n\n",
    );
    for cue in cues {
        let words = cue.text.split_whitespace().collect::<Vec<_>>();
        let span = (cue.end_seconds - cue.start_seconds).max(0.05);
        let mut payload = String::new();
        for (index, word) in words.iter().enumerate() {
            if index > 0 {
                let at = cue.start_seconds + span * (index as f64 / words.len().max(1) as f64);
                payload.push(' ');
                payload.push_str(&format!("<{}>", vtt_time(at)));
            }
            payload.push_str(word);
        }
        output.push_str(&format!(
            "{} --> {} line:88% position:50% align:center size:88%\n{}\n\n",
            vtt_time(cue.start_seconds),
            vtt_time(cue.end_seconds),
            payload
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
    let word_highlight_vtt_relative = format!(
        "{}/subtitles/{slug}-{stamp}-word-highlight.vtt",
        project.relative_path
    );
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
    let word_highlight_vtt = video_production::resolve_project_path(
        workspace,
        &project,
        &word_highlight_vtt_relative,
        AccessOperation::Write,
        false,
    )?;

    fs::write(&srt, subtitle_text_srt(cues))
        .map_err(|error| format!("Could not save SRT subtitles: {error}"))?;
    fs::write(&vtt, subtitle_text_vtt(cues))
        .map_err(|error| format!("Could not save VTT subtitles: {error}"))?;
    fs::write(&word_highlight_vtt, subtitle_text_vtt_word_highlight(cues))
        .map_err(|error| format!("Could not save word-highlight VTT subtitles: {error}"))?;

    let prefix = format!("{}/", project.relative_path);
    let srt_asset = srt_relative
        .strip_prefix(&prefix)
        .ok_or_else(|| "SRT subtitle path escaped its Video Project.".to_string())?;
    let vtt_asset = vtt_relative
        .strip_prefix(&prefix)
        .ok_or_else(|| "VTT subtitle path escaped its Video Project.".to_string())?;
    let word_highlight_vtt_asset = word_highlight_vtt_relative
        .strip_prefix(&prefix)
        .ok_or_else(|| "Word-highlight VTT path escaped its Video Project.".to_string())?;
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
    video_production::register_asset(
        workspace,
        project_id,
        "subtitle-word-highlight",
        word_highlight_vtt_asset,
        Some(&format!("{language} word-highlight VTT subtitles")),
    )?;

    Ok(SubtitleAsset {
        project_id: project.id,
        language,
        srt_path: srt_relative,
        vtt_path: vtt_relative,
        word_highlight_vtt_path: word_highlight_vtt_relative,
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

#[allow(clippy::too_many_arguments)]
fn select_provider(
    requested: &str,
    language: &str,
    managed_language_supported: bool,
    managed_platform_supported: bool,
    managed_ready: bool,
    allow_managed_download: bool,
    piper_ready: bool,
    has_piper_model: bool,
) -> Result<String, String> {
    match requested {
        "auto" if managed_language_supported && managed_platform_supported && managed_ready => {
            Ok("supertonic-3".to_string())
        }
        "auto" if has_piper_model && piper_ready => Ok("piper".to_string()),
        "auto"
            if managed_language_supported
                && managed_platform_supported
                && allow_managed_download =>
        {
            Ok("supertonic-3".to_string())
        }
        "auto" if managed_language_supported && managed_platform_supported => Err(
            "Managed neural narration is available but not installed. RepoTunnel will not download a narration runtime/model without explicit opt-in. Set allowManagedDownload=true for this request, choose an already configured provider, or import narration audio."
                .to_string(),
        ),
        "auto" => Err(format!(
            "No high-quality local neural narrator is configured for {language}. Supply a compatible project Piper model, opt in to a supported managed narrator when available, or import narration audio."
        )),
        "supertonic-3" => {
            if !managed_language_supported {
                return Err(format!(
                    "Supertonic 3 does not support {language}. Use another configured provider for this language."
                ));
            }
            if !managed_platform_supported {
                return Err(
                    "RepoTunnel-managed Supertonic 3 is not packaged for this OS/architecture."
                        .to_string(),
                );
            }
            if !managed_ready && !allow_managed_download {
                return Err(
                    "Supertonic 3 is not installed. RepoTunnel will not download its runtime/model without explicit opt-in. Retry with allowManagedDownload=true if the user wants the managed download."
                        .to_string(),
                );
            }
            Ok("supertonic-3".to_string())
        }
        "piper" | "espeak-ng" => Ok(requested.to_string()),
        _ => Err(
            "Narration provider must be auto, supertonic-3, piper, or espeak-ng.".to_string(),
        ),
    }
}

fn apply_story_voice_cast(
    workspace: &Workspace,
    project_id: &str,
    request: &mut NarrationRequest,
) -> Result<Option<String>, String> {
    let Some(character_id) = request
        .character_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    let cast = video_director::resolve_voice_cast(workspace, project_id, character_id)?
        .ok_or_else(|| {
            format!(
                "Story character '{character_id}' has no persisted voice-cast assignment. Compile/update the story plan before synthesizing dialogue."
            )
        })?;

    if !request
        .language
        .trim()
        .eq_ignore_ascii_case(cast.language.trim())
    {
        return Err(format!(
            "Story character '{}' is cast with language '{}' but this narration requested '{}'. Keep one persistent character voice/language assignment.",
            cast.character_id, cast.language, request.language
        ));
    }

    if let Some(provider) = request
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("auto"))
    {
        if !provider.eq_ignore_ascii_case(&cast.provider) {
            return Err(format!(
                "Story character '{}' is cast with provider '{}' but narration requested '{}'. Character voice casting cannot change per line.",
                cast.character_id, cast.provider, provider
            ));
        }
    }

    if let Some(voice) = request
        .voice
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if voice != cast.voice {
            return Err(format!(
                "Story character '{}' is cast with voice '{}' but narration requested '{}'. Character voice casting cannot change per line.",
                cast.character_id, cast.voice, voice
            ));
        }
    }

    match (
        request
            .voice_model_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        cast.voice_model_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    ) {
        (Some(requested), Some(cast_path)) if requested != cast_path => {
            return Err(format!(
                "Story character '{}' uses a different persisted voice model. Character voice models cannot change per line.",
                cast.character_id
            ));
        }
        (Some(_), None) => {
            return Err(format!(
                "Story character '{}' has no voice model in its persisted cast, so a one-off voice model cannot be supplied.",
                cast.character_id
            ));
        }
        _ => {}
    }

    let cast_rate = cast.rate.unwrap_or(1.0);
    if let Some(requested_rate) = request.rate {
        if (requested_rate - cast_rate).abs() > 0.000_001 {
            return Err(format!(
                "Story character '{}' is cast at rate {:.3} but narration requested {:.3}. Character delivery rate cannot change per line.",
                cast.character_id, cast_rate, requested_rate
            ));
        }
    }

    request.provider = Some(cast.provider.clone());
    request.voice = Some(cast.voice.clone());
    request.voice_model_path = cast.voice_model_path.clone();
    request.rate = Some(cast_rate);
    request.character_id = Some(cast.character_id.clone());
    Ok(Some(cast.character_id))
}

pub(crate) fn synthesize(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    mut request: NarrationRequest,
) -> Result<NarrationAsset, String> {
    if request.text.trim().is_empty() {
        return Err("Narration text cannot be empty.".to_string());
    }
    if request.text.chars().count() > MAX_NARRATION_CHARS {
        return Err("Narration text exceeds the 120,000 character safety limit.".to_string());
    }
    let language = validate_language(&request.language)?;
    let project = video_production::get_project(workspace, project_id)?;
    let character_id = apply_story_voice_cast(workspace, project_id, &mut request)?;
    let provider = request
        .provider
        .as_deref()
        .unwrap_or("auto")
        .trim()
        .to_ascii_lowercase();

    let managed_language_supported = video_narration_managed::language_code(&language).is_some();
    let managed_platform_supported = video_narration_managed::platform_supported();
    let managed_ready = managed_platform_supported && video_narration_managed::is_ready(app);
    let piper_ready = find_on_path("piper").is_some() || python_piper_available();
    if request.allow_managed_download && !project.resource_policy.allow_local_model_downloads {
        return Err(
            "This Video Project blocks local model/runtime downloads. Enable allowLocalModelDownloads in the project resource policy before explicitly opting in to a managed narrator download."
                .to_string(),
        );
    }
    let selected = select_provider(
        &provider,
        &language,
        managed_language_supported,
        managed_platform_supported,
        managed_ready,
        request.allow_managed_download && project.resource_policy.allow_local_model_downloads,
        piper_ready,
        request.voice_model_path.is_some(),
    )?;

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

    let mut command = match selected.as_str() {
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
    if let Some(scene_id) = request.scene_id.as_deref() {
        let _ = video_production::record_scene_narration(
            workspace,
            project_id,
            scene_id,
            &request.text,
            duration,
            &audio_relative,
            &subtitles.srt_path,
        );
    }
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
        character_id,
        voice: request.voice,
        audio_path: audio_relative,
        duration_seconds: duration,
        subtitles,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        caption_chunks, caption_chunks_are_phrase_safe, cues_from_text, select_provider, srt_time,
        subtitle_text_srt, subtitle_text_vtt, subtitle_text_vtt_word_highlight, validate_language,
        MAX_CAPTION_CHARS, MAX_CAPTION_WORDS,
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
    fn subtitle_chunks_are_short_and_readable() {
        let chunks = caption_chunks(
            "Masked diffusion predicts several candidate tokens at once, then commits only the most confident subset before repeating the refinement cycle.",
        );
        assert!(chunks.len() >= 3);
        for chunk in chunks {
            assert!(chunk.split_whitespace().count() <= MAX_CAPTION_WORDS);
            assert!(chunk.chars().count() <= MAX_CAPTION_CHARS);
        }
    }

    #[test]
    fn captions_do_not_leave_article_or_preposition_dangling_at_a_break() {
        let chunks = caption_chunks(
            "Branches let you work independently without disturbing the stable codebase.",
        );
        assert!(caption_chunks_are_phrase_safe(&chunks));
        assert!(!chunks
            .iter()
            .any(|chunk| chunk.ends_with("without disturbing the")));
        assert!(!chunks.iter().any(|chunk| chunk == "the stable codebase."));
    }

    #[test]
    fn managed_narration_download_requires_explicit_opt_in() {
        let blocked =
            select_provider("auto", "en-US", true, true, false, false, false, false).unwrap_err();
        assert!(blocked.contains("explicit opt-in"));

        assert_eq!(
            select_provider("auto", "en-US", true, true, false, true, false, false,).unwrap(),
            "supertonic-3"
        );
        assert_eq!(
            select_provider("auto", "en-US", true, true, false, false, true, true,).unwrap(),
            "piper"
        );
    }

    #[test]
    fn srt_and_vtt_are_generated_from_same_cues() {
        let cues = cues_from_text("Hello world. Next step!", Some(4.0)).unwrap();
        let srt = subtitle_text_srt(&cues);
        let vtt = subtitle_text_vtt(&cues);
        assert!(srt.contains("00:00:00,000"));
        assert!(vtt.starts_with("WEBVTT"));
        assert!(vtt.contains("00:00:00.000"));
        assert!(vtt.contains("line:88% position:50% align:center size:88%"));
        let highlighted = subtitle_text_vtt_word_highlight(&cues);
        assert!(highlighted.contains("::cue(:past)"));
        assert!(highlighted.contains("<00:"));
    }

    #[test]
    fn subtitle_time_format_handles_hours() {
        assert_eq!(srt_time(3723.5), "01:02:03,500");
    }
}
