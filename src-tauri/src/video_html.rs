use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::{
    access::AccessOperation,
    models::Workspace,
    video,
    video_production::{self, VideoProductionProject},
};

const TEMPLATE_HTML: &str = include_str!("../resources/video_html_scene_template.html");
const CAPTURE_SCRIPT: &str = include_str!("../resources/video_html_capture.mjs");
const MAX_SCENE_SECONDS: f64 = 30.0;
const MAX_ITEMS: usize = 12;
const MAX_TEXT_CHARS: usize = 12_000;

const TEMPLATES: &[&str] = &[
    "intro",
    "title_bullets",
    "card_grid",
    "flow_steps",
    "git_graph",
    "code_typing",
    "terminal",
    "comparison",
    "stats_chart",
    "quote",
    "lower_third",
    "outro",
];

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoHtmlItem {
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoHtmlSceneSpec {
    pub(crate) id: String,
    pub(crate) template: String,
    pub(crate) duration_seconds: f64,
    #[serde(default = "default_theme")]
    pub(crate) theme: String,
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) subtitle: String,
    #[serde(default)]
    pub(crate) kicker: String,
    #[serde(default)]
    pub(crate) items: Vec<VideoHtmlItem>,
    #[serde(default)]
    pub(crate) code: String,
    #[serde(default)]
    pub(crate) quote: String,
    #[serde(default)]
    pub(crate) speaker: String,
    #[serde(default)]
    pub(crate) accent: Option<String>,
    #[serde(default)]
    pub(crate) transition: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HtmlDesignQaIssue {
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HtmlDesignQaReport {
    pub(crate) passed: bool,
    #[serde(default)]
    pub(crate) auto_fixed: bool,
    #[serde(default)]
    pub(crate) coverage_ratio: f64,
    #[serde(default)]
    pub(crate) issues: Vec<HtmlDesignQaIssue>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoHtmlSceneRender {
    pub(crate) project_id: String,
    pub(crate) scene_id: String,
    pub(crate) template: String,
    pub(crate) theme: String,
    pub(crate) source_path: String,
    pub(crate) output_path: String,
    pub(crate) design_qa_path: String,
    pub(crate) sample_frame_paths: Vec<String>,
    pub(crate) duration_seconds: f64,
    pub(crate) frame_count: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) design_qa: HtmlDesignQaReport,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureConfig {
    chrome: String,
    html_path: String,
    frames_dir: String,
    sample_dir: String,
    qa_report_path: String,
    width: u32,
    height: u32,
    fps: u32,
    frame_count: u32,
}

fn default_theme() -> String {
    "modern-dark".to_string()
}

pub(crate) fn template_catalog() -> Vec<String> {
    TEMPLATES.iter().map(|value| (*value).to_string()).collect()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else {
            dash = true;
        }
        if out.len() >= 56 {
            break;
        }
    }
    if out.is_empty() {
        "scene".to_string()
    } else {
        out.trim_matches('-').to_string()
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn normalize_theme(value: &str) -> Result<&'static str, String> {
    match value.trim().to_ascii_lowercase().replace(' ', "-").as_str() {
        "modern-dark" | "dark" => Ok("modern-dark"),
        "playful-bright" | "bright" | "playful" => Ok("playful-bright"),
        _ => Err("Video HTML theme must be Modern dark or Playful bright.".to_string()),
    }
}

fn validate_hex(value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#') else {
        return false;
    };
    matches!(hex.len(), 6 | 8) && hex.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn validate_spec(spec: &VideoHtmlSceneSpec) -> Result<(), String> {
    if !TEMPLATES.contains(&spec.template.as_str()) {
        return Err(format!(
            "Unknown Video HTML template '{}'. Use one of: {}.",
            spec.template,
            TEMPLATES.join(", ")
        ));
    }
    let _ = normalize_theme(&spec.theme)?;
    if !(1.5..=MAX_SCENE_SECONDS).contains(&spec.duration_seconds)
        || !spec.duration_seconds.is_finite()
    {
        return Err(format!(
            "HTML scene duration must be between 1.5 and {MAX_SCENE_SECONDS:.0} seconds so entrance, continuous motion, and exit transitions have deterministic room."
        ));
    }
    if spec.items.len() > MAX_ITEMS {
        return Err(format!(
            "HTML scenes support at most {MAX_ITEMS} template items."
        ));
    }
    let total_text = spec.title.len()
        + spec.subtitle.len()
        + spec.kicker.len()
        + spec.code.len()
        + spec.quote.len()
        + spec.speaker.len()
        + spec
            .items
            .iter()
            .map(|item| item.title.len() + item.body.len() + item.value.len())
            .sum::<usize>();
    if total_text > MAX_TEXT_CHARS {
        return Err("HTML scene text exceeds the bounded template limit.".to_string());
    }
    if let Some(accent) = spec.accent.as_deref() {
        if !validate_hex(accent) {
            return Err("HTML scene accent must be #RRGGBB or #RRGGBBAA.".to_string());
        }
    }
    if let Some(transition) = spec.transition.as_deref() {
        if !matches!(transition, "slide" | "wipe" | "zoom") {
            return Err("HTML scene transition must be slide, wipe, or zoom.".to_string());
        }
    }
    Ok(())
}

fn heading(spec: &VideoHtmlSceneSpec) -> String {
    let mut out = String::new();
    if !spec.kicker.trim().is_empty() {
        out.push_str(&format!(
            r#"<div class="kicker reveal" data-qa data-qa-text data-kind="label">{}</div>"#,
            html_escape(spec.kicker.trim())
        ));
    }
    if !spec.title.trim().is_empty() {
        out.push_str(&format!(
            r#"<h1 class="title reveal" data-qa data-qa-text data-kind="title">{}</h1>"#,
            html_escape(spec.title.trim())
        ));
    }
    if !spec.subtitle.trim().is_empty() {
        out.push_str(&format!(
            r#"<p class="subtitle reveal" data-qa data-qa-text data-kind="body">{}</p>"#,
            html_escape(spec.subtitle.trim())
        ));
    }
    out
}

fn card(item: &VideoHtmlItem, extra: &str) -> String {
    let mut body = String::new();
    if !item.value.trim().is_empty() {
        body.push_str(&format!(
            r#"<div class="value badge" data-qa-text data-kind="label">{}</div>"#,
            html_escape(item.value.trim())
        ));
    }
    if !item.title.trim().is_empty() {
        body.push_str(&format!(
            r#"<h3 data-qa-text data-kind="label">{}</h3>"#,
            html_escape(item.title.trim())
        ));
    }
    if !item.body.trim().is_empty() {
        body.push_str(&format!(
            r#"<p data-qa-text data-kind="body">{}</p>"#,
            html_escape(item.body.trim())
        ));
    }
    format!(
        r#"<section class="card {extra}" data-qa><div class="card-content">{body}</div></section>"#
    )
}

fn cards(spec: &VideoHtmlSceneSpec, class_name: &str) -> String {
    let content = spec
        .items
        .iter()
        .map(|item| card(item, class_name))
        .collect::<Vec<_>>()
        .join("");
    format!(r#"<div class="grid reveal">{content}</div>"#)
}

fn bullets(spec: &VideoHtmlSceneSpec) -> String {
    let items = spec
        .items
        .iter()
        .map(|item| {
            let text = if item.body.trim().is_empty() {
                item.title.trim()
            } else {
                item.body.trim()
            };
            format!(
                r#"<div class="bullet" data-qa><i class="bullet-dot" aria-hidden="true"></i><div class="bullet-text" data-qa-text data-kind="body">{}</div></div>"#,
                html_escape(text)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(r#"<div class="bullet-list reveal">{items}</div>"#)
}

fn flow(spec: &VideoHtmlSceneSpec) -> String {
    let mut parts = Vec::new();
    for (index, item) in spec.items.iter().enumerate() {
        if index > 0 {
            parts.push(r#"<div class="arrow" aria-hidden="true"></div>"#.to_string());
        }
        let title = html_escape(item.title.trim());
        let body = html_escape(item.body.trim());
        parts.push(format!(
            r#"<section class="flow-node" data-qa><h3 data-qa-text data-kind="label">{title}</h3><p data-qa-text data-kind="body">{body}</p></section>"#
        ));
    }
    format!(r#"<div class="flow reveal">{}</div>"#, parts.join(""))
}

fn code_block(spec: &VideoHtmlSceneSpec, terminal: bool) -> String {
    let lines = spec
        .code
        .lines()
        .map(|line| {
            format!(
                r#"<span class="type-line" data-qa-text data-kind="code">{}</span>"#,
                html_escape(line)
            )
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    let shell_class = if terminal {
        "terminal-shell"
    } else {
        "code-shell"
    };
    format!(
        r#"<div class="{shell_class} reveal" data-qa><div class="shell-bar"><i class="dot"></i><i class="dot"></i><i class="dot"></i></div><pre>{lines}</pre></div>"#
    )
}

fn git_graph(spec: &VideoHtmlSceneSpec) -> String {
    let count = spec.items.len().clamp(3, 6);
    let nodes = (0..count)
        .map(|_| r#"<i class="git-node"></i>"#)
        .collect::<Vec<_>>()
        .join("");
    let labels = spec
        .items
        .iter()
        .take(count)
        .map(|item| {
            format!(
                r#"<span class="git-label" data-qa-text data-kind="label">{}</span>"#,
                html_escape(item.title.trim())
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        r#"<div class="git-graph reveal" data-qa><div class="git-track"></div><div class="git-branch"></div><div class="git-nodes">{nodes}</div><div class="git-labels">{labels}</div></div>"#
    )
}

fn stats_chart(spec: &VideoHtmlSceneSpec) -> String {
    let cards = spec
        .items
        .iter()
        .take(3)
        .map(|item| card(item, "stat"))
        .collect::<Vec<_>>()
        .join("");
    let bars = spec
        .items
        .iter()
        .take(5)
        .enumerate()
        .map(|(index, item)| {
            let seed = item
                .value
                .bytes()
                .chain(item.title.bytes())
                .fold(index * 17 + 11, |value, byte| value.wrapping_add(byte as usize));
            let height = 38 + (seed % 53);
            format!(
                r#"<div class="chart-bar-wrap"><div class="chart-bar" style="--bar-height:{height}%"></div><div class="chart-label" data-qa-text data-kind="label">{}</div></div>"#,
                html_escape(item.title.trim())
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        r#"<div class="stat-grid reveal">{cards}</div><div class="chart-bars reveal" data-qa>{bars}</div>"#
    )
}

fn body_for(spec: &VideoHtmlSceneSpec) -> String {
    let head = heading(spec);
    match spec.template.as_str() {
        "intro" | "outro" => format!(
            r#"{head}<div class="hero-ring reveal" data-qa>{}</div>"#,
            html_escape(
                spec.items
                    .first()
                    .map(|item| item.value.as_str())
                    .unwrap_or("•")
            )
        ),
        "title_bullets" => format!("{head}{}", bullets(spec)),
        "card_grid" => format!("{head}{}", cards(spec, "")),
        "flow_steps" => format!("{head}{}", flow(spec)),
        "git_graph" => format!("{head}{}", git_graph(spec)),
        "code_typing" => format!("{head}{}", code_block(spec, false)),
        "terminal" => format!("{head}{}", code_block(spec, true)),
        "comparison" => {
            let pair = spec
                .items
                .iter()
                .take(2)
                .map(|item| card(item, ""))
                .collect::<Vec<_>>()
                .join("");
            format!(r#"{head}<div class="compare reveal">{pair}</div>"#)
        }
        "stats_chart" => format!("{head}{}", stats_chart(spec)),
        "quote" => format!(
            r#"{head}<div class="quote-mark reveal">“</div><blockquote class="quote reveal" data-qa data-qa-text data-kind="body">{}</blockquote><div class="speaker reveal" data-qa data-qa-text data-kind="label">{}</div>"#,
            html_escape(spec.quote.trim()),
            html_escape(spec.speaker.trim())
        ),
        "lower_third" => format!(
            r#"<div class="hero-ring reveal" data-qa>{}</div><div class="lower-third reveal" data-qa><strong data-qa-text data-kind="label">{}</strong><span data-qa-text data-kind="label">{}</span></div>"#,
            html_escape(
                spec.items
                    .first()
                    .map(|item| item.value.as_str())
                    .unwrap_or("•")
            ),
            html_escape(spec.title.trim()),
            html_escape(spec.subtitle.trim())
        ),
        _ => head,
    }
}

fn accent_pair(spec: &VideoHtmlSceneSpec) -> (String, String) {
    if let Some(accent) = spec.accent.as_deref() {
        return (accent.to_string(), "#7c5cff".to_string());
    }
    let palettes = [
        ("#58d6ff", "#7c5cff"),
        ("#63f5b5", "#34a8ff"),
        ("#ff7ac8", "#8e6cff"),
        ("#ffb35c", "#ff6a7a"),
        ("#71e2ff", "#5ee6a8"),
    ];
    let hash = spec.id.bytes().fold(0usize, |value, byte| {
        value.wrapping_mul(31).wrapping_add(byte as usize)
    });
    let (a, b) = palettes[hash % palettes.len()];
    (a.to_string(), b.to_string())
}

fn scene_html(spec: &VideoHtmlSceneSpec, width: u32, height: u32) -> Result<String, String> {
    validate_spec(spec)?;
    let theme = normalize_theme(&spec.theme)?;
    let (accent, accent2) = accent_pair(spec);
    let hash = spec
        .id
        .bytes()
        .fold(0usize, |value, byte| value + byte as usize);
    let layout = format!("layout-{}", (hash % 3) + 1);
    let transition = spec.transition.as_deref().unwrap_or(match hash % 3 {
        0 => "slide",
        1 => "wipe",
        _ => "zoom",
    });
    let safe = 120u32
        .min(width.saturating_div(5))
        .min(height.saturating_div(5));
    let html = TEMPLATE_HTML
        .replace("__WIDTH__", &width.to_string())
        .replace("__HEIGHT__", &height.to_string())
        .replace("__SAFE__", &safe.to_string())
        .replace("__ACCENT__", &accent)
        .replace("__ACCENT2__", &accent2)
        .replace("__THEME_CLASS__", theme)
        .replace("__LAYOUT_CLASS__", &layout)
        .replace("__BODY__", &body_for(spec))
        .replace("__DURATION__", &format!("{:.3}", spec.duration_seconds))
        .replace("__TRANSITION__", transition);
    Ok(html)
}

fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
            #[cfg(windows)]
            {
                let candidate = directory.join(format!("{name}.exe"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

#[allow(clippy::unnecessary_lazy_evaluations)]
fn chrome_program() -> Option<PathBuf> {
    find_on_path(&[
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "msedge",
        "chrome",
    ])
    .or_else(|| {
        #[cfg(target_os = "macos")]
        {
            let path =
                PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
            if path.is_file() {
                return Some(path);
            }
        }
        None
    })
}

fn node_program() -> Option<PathBuf> {
    find_on_path(&["node", "nodejs"])
}

#[allow(clippy::type_complexity)]
fn scene_paths(
    workspace: &Workspace,
    project: &VideoProductionProject,
    scene_id: &str,
) -> Result<
    (
        PathBuf,
        String,
        PathBuf,
        String,
        PathBuf,
        PathBuf,
        String,
        PathBuf,
        String,
    ),
    String,
> {
    let base = format!("{}-{}", slug(scene_id), now_millis());
    let source_relative = format!("{}/animations/source/{base}.html", project.relative_path);
    let output_relative = format!("{}/animations/generated/{base}.mp4", project.relative_path);
    let frames_relative = format!(
        "{}/animations/generated/.frames-{base}",
        project.relative_path
    );
    let qa_relative = format!("{}/qa/design-scenes/{base}.json", project.relative_path);
    let samples_relative = format!("{}/qa/frame-review/{base}", project.relative_path);
    let source = video_production::resolve_project_path(
        workspace,
        project,
        &source_relative,
        AccessOperation::Write,
        false,
    )?;
    let output = video_production::resolve_project_path(
        workspace,
        project,
        &output_relative,
        AccessOperation::Write,
        false,
    )?;
    let frames = video_production::resolve_project_path(
        workspace,
        project,
        &frames_relative,
        AccessOperation::Write,
        false,
    )?;
    let qa = video_production::resolve_project_path(
        workspace,
        project,
        &qa_relative,
        AccessOperation::Write,
        false,
    )?;
    let samples = video_production::resolve_project_path(
        workspace,
        project,
        &samples_relative,
        AccessOperation::Write,
        false,
    )?;
    Ok((
        source,
        source_relative,
        output,
        output_relative,
        frames,
        qa,
        qa_relative,
        samples,
        samples_relative,
    ))
}

fn encode_frames(
    app: &AppHandle,
    project: &VideoProductionProject,
    frames: &Path,
    fps: u32,
    output: &Path,
) -> Result<(), String> {
    let ffmpeg =
        video::ffmpeg_program(app, project.resource_policy.allow_automatic_package_install)?;
    let input = frames.join("frame-%06d.png");
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-nostats",
            "-loglevel",
            "error",
            "-y",
            "-framerate",
        ])
        .arg(fps.to_string())
        .arg("-i")
        .arg(input)
        .args([
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
        ])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Could not start HTML-scene FFmpeg encoder: {error}"))?;
    if !status.status.success() {
        return Err(format!(
            "HTML-scene FFmpeg encoder failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    if !output.is_file() {
        return Err("HTML-scene encoder produced no output.".to_string());
    }
    Ok(())
}

pub(crate) fn render_scene(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    spec: VideoHtmlSceneSpec,
) -> Result<VideoHtmlSceneRender, String> {
    validate_spec(&spec)?;
    let project = video_production::get_project(workspace, project_id)?;
    if project.production_mode != "story" {
        if project.script_path.is_none() || project.storyboard_path.is_none() {
            return Err(
                "HTML scene rendering is blocked until the tutorial script and storyboard are saved."
                    .to_string(),
            );
        }
        let missing = ["spec_check", "template_theme", "assets_voice"]
            .into_iter()
            .filter(|stage| !video_production::latest_checkpoint_passed(&project, stage))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!(
                "HTML scene rendering is blocked until the required planning/asset gates pass: {}.",
                missing.join(", ")
            ));
        }
    }
    let node = node_program().ok_or_else(|| {
        "HTML/GSAP renderer requires an already-installed Node.js runtime. RepoTunnel will not install it automatically; use the native 2D fallback if unavailable.".to_string()
    })?;
    let chrome = chrome_program().ok_or_else(|| {
        "HTML/GSAP renderer requires an already-installed Chrome/Chromium browser. RepoTunnel will not install it automatically; use the native 2D fallback if unavailable.".to_string()
    })?;
    let fps = project.fps.clamp(15, 60);
    let frame_count = ((spec.duration_seconds * f64::from(fps)).ceil() as u32).max(1);
    let (
        source,
        source_relative,
        output,
        output_relative,
        frames,
        qa,
        qa_relative,
        samples,
        samples_relative,
    ) = scene_paths(workspace, &project, &spec.id)?;

    if source.exists() || output.exists() || frames.exists() || qa.exists() || samples.exists() {
        return Err("HTML scene output path unexpectedly already exists.".to_string());
    }
    if let Some(parent) = source.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create HTML scene source directory: {error}"))?;
    }
    if let Some(parent) = qa.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create design-QA directory: {error}"))?;
    }
    fs::create_dir_all(&frames)
        .map_err(|error| format!("Could not create HTML frame directory: {error}"))?;
    fs::create_dir_all(&samples)
        .map_err(|error| format!("Could not create frame-review directory: {error}"))?;

    let result = (|| {
        let html = scene_html(&spec, project.width, project.height)?;
        fs::write(&source, html)
            .map_err(|error| format!("Could not save HTML scene source: {error}"))?;
        let script = frames.join(".capture.mjs");
        let config_path = frames.join(".capture.json");
        fs::write(&script, CAPTURE_SCRIPT)
            .map_err(|error| format!("Could not prepare HTML capture helper: {error}"))?;
        let config = CaptureConfig {
            chrome: chrome.to_string_lossy().into_owned(),
            html_path: source.to_string_lossy().into_owned(),
            frames_dir: frames.to_string_lossy().into_owned(),
            sample_dir: samples.to_string_lossy().into_owned(),
            qa_report_path: qa.to_string_lossy().into_owned(),
            width: project.width,
            height: project.height,
            fps,
            frame_count,
        };
        fs::write(
            &config_path,
            serde_json::to_vec_pretty(&config)
                .map_err(|error| format!("Could not serialize HTML capture config: {error}"))?,
        )
        .map_err(|error| format!("Could not write HTML capture config: {error}"))?;

        video_production::update_project_status(
            workspace,
            project_id,
            "generating",
            Some("Rendering deterministic HTML/CSS + GSAP scene."),
        )?;

        let capture = Command::new(node)
            .arg(&script)
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| {
                format!("Could not start deterministic Chrome frame capture: {error}")
            })?;
        if !capture.status.success() {
            return Err(format!(
                "Deterministic HTML/GSAP capture failed: {}",
                String::from_utf8_lossy(&capture.stderr).trim()
            ));
        }

        let design_qa: HtmlDesignQaReport = serde_json::from_slice(
            &fs::read(&qa)
                .map_err(|error| format!("Could not read HTML design-QA report: {error}"))?,
        )
        .map_err(|error| format!("Could not parse HTML design-QA report: {error}"))?;
        if !design_qa.passed {
            return Err(
                "HTML design QA did not pass after automatic layout correction.".to_string(),
            );
        }

        encode_frames(app, &project, &frames, fps, &output)?;

        let project_prefix = format!("{}/", project.relative_path);
        let source_asset = source_relative
            .strip_prefix(&project_prefix)
            .ok_or_else(|| "HTML source escaped its Video Project.".to_string())?;
        let output_asset = output_relative
            .strip_prefix(&project_prefix)
            .ok_or_else(|| "HTML render escaped its Video Project.".to_string())?;
        let qa_asset = qa_relative
            .strip_prefix(&project_prefix)
            .ok_or_else(|| "HTML QA report escaped its Video Project.".to_string())?;

        video_production::register_asset(
            workspace,
            project_id,
            "animation-source",
            source_asset,
            Some(&format!("HTML/GSAP scene source: {}", spec.id)),
        )?;
        video_production::register_asset(
            workspace,
            project_id,
            "animation",
            output_asset,
            Some(&format!("HTML/GSAP scene: {}", spec.id)),
        )?;
        video_production::register_asset(
            workspace,
            project_id,
            "design-qa",
            qa_asset,
            Some(&format!("HTML scene design QA: {}", spec.id)),
        )?;
        let _ = video_production::record_scene_render(
            workspace,
            project_id,
            &spec.id,
            spec.duration_seconds,
            &source_relative,
            &output_relative,
        );

        let mut sample_frame_paths = fs::read_dir(&samples)
            .map_err(|error| format!("Could not list sampled review frames: {error}"))?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_file())
            .map(|entry| {
                format!(
                    "{}/{}",
                    samples_relative,
                    entry.file_name().to_string_lossy()
                )
            })
            .collect::<Vec<_>>();
        sample_frame_paths.sort();

        video_production::update_project_status(
            workspace,
            project_id,
            "editing",
            Some("HTML/GSAP scene rendered; DOM design QA passed and review frames were sampled."),
        )?;

        Ok(VideoHtmlSceneRender {
            project_id: project.id.clone(),
            scene_id: spec.id.clone(),
            template: spec.template.clone(),
            theme: normalize_theme(&spec.theme)?.to_string(),
            source_path: source_relative.clone(),
            output_path: output_relative.clone(),
            design_qa_path: qa_relative.clone(),
            sample_frame_paths,
            duration_seconds: spec.duration_seconds,
            frame_count,
            width: project.width,
            height: project.height,
            fps,
            design_qa,
        })
    })();

    let _ = fs::remove_dir_all(&frames);
    if let Err(error) = &result {
        let _ = fs::remove_file(&output);
        let _ = video_production::update_project_status(
            workspace,
            project_id,
            "failed",
            Some(&format!("HTML/GSAP scene failed: {error}")),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{scene_html, template_catalog, validate_spec, VideoHtmlItem, VideoHtmlSceneSpec};

    fn sample() -> VideoHtmlSceneSpec {
        VideoHtmlSceneSpec {
            id: "git-branches".to_string(),
            template: "card_grid".to_string(),
            duration_seconds: 4.0,
            theme: "modern-dark".to_string(),
            title: "Git branches".to_string(),
            subtitle: "Parallel work without disturbing stable code".to_string(),
            kicker: "Git".to_string(),
            items: vec![
                VideoHtmlItem {
                    title: "Main".to_string(),
                    body: "Stable code".to_string(),
                    value: "A".to_string(),
                },
                VideoHtmlItem {
                    title: "Feature".to_string(),
                    body: "Independent work".to_string(),
                    value: "B".to_string(),
                },
            ],
            code: String::new(),
            quote: String::new(),
            speaker: String::new(),
            accent: None,
            transition: Some("wipe".to_string()),
        }
    }

    #[test]
    fn catalog_contains_required_reusable_templates() {
        let catalog = template_catalog();
        for expected in [
            "intro",
            "title_bullets",
            "card_grid",
            "flow_steps",
            "git_graph",
            "code_typing",
            "terminal",
            "comparison",
            "stats_chart",
            "quote",
            "lower_third",
            "outro",
        ] {
            assert!(catalog.iter().any(|item| item == expected));
        }
    }

    #[test]
    fn html_scene_contains_design_and_motion_contract() {
        let spec = sample();
        validate_spec(&spec).unwrap();
        let html = scene_html(&spec, 1920, 1080).unwrap();
        assert!(html.contains("--safe:120px"));
        assert!(html.contains("font-size:82px"));
        assert!(html.contains("power3.out"));
        assert!(html.contains("back.out"));
        assert!(html.contains("window.__repotunnelSeek"));
        assert!(html.contains("window.__repotunnelQa"));
        assert!(html.contains("getBoundingClientRect"));
    }

    #[test]
    fn unknown_template_and_paid_style_shortcuts_are_not_accepted() {
        let mut spec = sample();
        spec.template = "custom_paid_template".to_string();
        assert!(validate_spec(&spec).is_err());
    }
}
