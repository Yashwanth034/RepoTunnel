use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use resvg::{tiny_skia, usvg};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::{
    access::AccessOperation,
    models::Workspace,
    video,
    video_production::{self, VideoProductionProject},
};

const MAX_SCENE_SECONDS: f64 = 30.0;
const MAX_ELEMENTS: usize = 96;
const MAX_TEXT_CHARS: usize = 4096;
static SCENE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoSceneSpec {
    #[serde(default = "scene_version")]
    pub(crate) version: u32,
    pub(crate) id: String,
    pub(crate) duration_seconds: f64,
    #[serde(default)]
    pub(crate) background: Option<String>,
    #[serde(default)]
    pub(crate) elements: Vec<VideoSceneElement>,
}

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoSceneElement {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) x: f64,
    #[serde(default)]
    pub(crate) y: f64,
    #[serde(default)]
    pub(crate) width: f64,
    #[serde(default)]
    pub(crate) height: f64,
    #[serde(default)]
    pub(crate) x2: f64,
    #[serde(default)]
    pub(crate) y2: f64,
    #[serde(default)]
    pub(crate) radius: f64,
    #[serde(default)]
    pub(crate) corner_radius: f64,
    #[serde(default)]
    pub(crate) font_size: f64,
    #[serde(default)]
    pub(crate) stroke_width: f64,
    #[serde(default)]
    pub(crate) fill: Option<String>,
    #[serde(default)]
    pub(crate) stroke: Option<String>,
    #[serde(default)]
    pub(crate) font_family: Option<String>,
    #[serde(default)]
    pub(crate) start_seconds: f64,
    #[serde(default)]
    pub(crate) end_seconds: Option<f64>,
    #[serde(default)]
    pub(crate) animation: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoSceneRender {
    pub(crate) project_id: String,
    pub(crate) scene_id: String,
    pub(crate) source_path: String,
    pub(crate) output_path: String,
    pub(crate) duration_seconds: f64,
    pub(crate) frame_count: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
}

fn scene_version() -> u32 {
    1
}

fn now_millis() -> Result<u64, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System time is unavailable.".to_string())?
        .as_millis();
    Ok(u64::try_from(millis).unwrap_or(u64::MAX))
}

fn scene_slug(value: &str) -> String {
    let mut result = String::new();
    let mut dash = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if dash && !result.is_empty() {
                result.push('-');
            }
            result.push(ch.to_ascii_lowercase());
            dash = false;
        } else {
            dash = true;
        }
        if result.len() >= 48 {
            break;
        }
    }
    if result.is_empty() {
        "scene".to_string()
    } else {
        result.trim_matches('-').to_string()
    }
}

fn finite(value: f64) -> bool {
    value.is_finite() && value.abs() <= 100_000.0
}

fn validate_color(value: Option<&str>, fallback: &str) -> Result<String, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(fallback.to_string());
    };
    if value == "transparent" {
        return Ok(value.to_string());
    }
    let Some(hex) = value.strip_prefix('#') else {
        return Err("Scene colors must use #RGB, #RRGGBB, #RRGGBBAA, or transparent.".to_string());
    };
    if matches!(hex.len(), 3 | 6 | 8) && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Ok(format!("#{hex}"))
    } else {
        Err("Scene colors must use #RGB, #RRGGBB, #RRGGBBAA, or transparent.".to_string())
    }
}

fn validate_font_family(value: Option<&str>) -> Result<String, String> {
    let family = value.unwrap_or("DejaVu Sans").trim();
    if family.is_empty()
        || family.len() > 80
        || !family
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, ' ' | '-' | '_' | '.'))
    {
        return Err("Scene font family contains unsupported characters.".to_string());
    }
    Ok(family.to_string())
}

fn validate_scene(scene: &VideoSceneSpec) -> Result<(), String> {
    if scene.version != 1 {
        return Err("Unsupported generated-scene version.".to_string());
    }
    if !(0.25..=MAX_SCENE_SECONDS).contains(&scene.duration_seconds)
        || !scene.duration_seconds.is_finite()
    {
        return Err(format!(
            "Generated scene duration must be between 0.25 and {MAX_SCENE_SECONDS:.0} seconds."
        ));
    }
    if scene.elements.len() > MAX_ELEMENTS {
        return Err(format!(
            "Generated scenes are limited to {MAX_ELEMENTS} elements."
        ));
    }
    let _ = validate_color(scene.background.as_deref(), "#0b0f14")?;
    for element in &scene.elements {
        if !matches!(
            element.kind.as_str(),
            "text" | "rect" | "circle" | "line" | "arrow"
        ) {
            return Err(format!(
                "Unsupported generated-scene element '{}'.",
                element.kind
            ));
        }
        for value in [
            element.x,
            element.y,
            element.width,
            element.height,
            element.x2,
            element.y2,
            element.radius,
            element.corner_radius,
            element.font_size,
            element.stroke_width,
            element.start_seconds,
            element.end_seconds.unwrap_or(0.0),
        ] {
            if !finite(value) {
                return Err("Generated-scene element contains an invalid number.".to_string());
            }
        }
        if element.start_seconds < 0.0 || element.start_seconds > scene.duration_seconds {
            return Err("Generated-scene element start time is outside the scene.".to_string());
        }
        if let Some(end) = element.end_seconds {
            if end <= element.start_seconds || end > scene.duration_seconds + 0.001 {
                return Err(
                    "Generated-scene element end time must follow its start and stay inside the scene."
                        .to_string(),
                );
            }
        }
        if element
            .text
            .as_ref()
            .is_some_and(|text| text.chars().count() > MAX_TEXT_CHARS)
        {
            return Err("Generated-scene text is too long.".to_string());
        }
        let _ = validate_color(element.fill.as_deref(), "#e8eef4")?;
        let _ = validate_color(element.stroke.as_deref(), "#83b6df")?;
        let _ = validate_font_family(element.font_family.as_deref())?;
        if let Some(animation) = element.animation.as_deref() {
            if !matches!(
                animation,
                "none" | "fade" | "slideUp" | "slideLeft" | "scale" | "draw"
            ) {
                return Err(format!(
                    "Unsupported generated-scene animation '{animation}'."
                ));
            }
        }
    }
    Ok(())
}

fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn ease_out_cubic(value: f64) -> f64 {
    let value = value.clamp(0.0, 1.0);
    1.0 - (1.0 - value).powi(3)
}

fn element_progress(element: &VideoSceneElement, time: f64) -> Option<f64> {
    if time + 0.000_1 < element.start_seconds {
        return None;
    }
    let animation = element.animation.as_deref().unwrap_or("fade");
    if animation == "none" {
        return Some(1.0);
    }
    let end = element
        .end_seconds
        .unwrap_or((element.start_seconds + 0.6).min(element.start_seconds + 1.0));
    let span = (end - element.start_seconds).max(0.001);
    Some(ease_out_cubic(
        ((time - element.start_seconds) / span).clamp(0.0, 1.0),
    ))
}

fn transform_for(element: &VideoSceneElement, progress: f64) -> (f64, String) {
    match element.animation.as_deref().unwrap_or("fade") {
        "fade" => (progress, String::new()),
        "slideUp" => (
            progress,
            format!("translate(0 {:.3})", (1.0 - progress) * 42.0),
        ),
        "slideLeft" => (
            progress,
            format!("translate({:.3} 0)", (1.0 - progress) * 58.0),
        ),
        "scale" => {
            let scale = 0.82 + progress * 0.18;
            (
                progress,
                format!(
                    "translate({x:.3} {y:.3}) scale({scale:.5}) translate({nx:.3} {ny:.3})",
                    x = element.x,
                    y = element.y,
                    nx = -element.x,
                    ny = -element.y,
                ),
            )
        }
        _ => (1.0, String::new()),
    }
}

fn arrow_head(x1: f64, y1: f64, x2: f64, y2: f64, size: f64) -> String {
    let angle = (y2 - y1).atan2(x2 - x1);
    let back = angle + std::f64::consts::PI;
    let left = back + 0.48;
    let right = back - 0.48;
    let lx = x2 + left.cos() * size;
    let ly = y2 + left.sin() * size;
    let rx = x2 + right.cos() * size;
    let ry = y2 + right.sin() * size;
    format!("{x2:.3},{y2:.3} {lx:.3},{ly:.3} {rx:.3},{ry:.3}")
}

fn element_svg(element: &VideoSceneElement, time: f64) -> Result<Option<String>, String> {
    let Some(progress) = element_progress(element, time) else {
        return Ok(None);
    };
    let fill = validate_color(element.fill.as_deref(), "#e8eef4")?;
    let stroke = validate_color(element.stroke.as_deref(), "#83b6df")?;
    let stroke_width = if element.stroke_width > 0.0 {
        element.stroke_width
    } else {
        3.0
    };
    let (opacity, transform) = transform_for(element, progress);
    let transform_attr = if transform.is_empty() {
        String::new()
    } else {
        format!(r#" transform="{transform}""#)
    };
    let wrapper_start = format!(r#"<g opacity="{opacity:.5}"{transform_attr}>"#);
    let wrapper_end = "</g>";

    let body = match element.kind.as_str() {
        "text" => {
            let text = escape_xml(element.text.as_deref().unwrap_or_default());
            let font_size = if element.font_size > 0.0 {
                element.font_size
            } else {
                48.0
            };
            let family = escape_xml(&validate_font_family(element.font_family.as_deref())?);
            format!(
                r#"<text x="{:.3}" y="{:.3}" fill="{}" font-family="{}" font-size="{:.3}" font-weight="600">{}</text>"#,
                element.x, element.y, fill, family, font_size, text
            )
        }
        "rect" => format!(
            r#"<rect x="{:.3}" y="{:.3}" width="{:.3}" height="{:.3}" rx="{:.3}" fill="{}" stroke="{}" stroke-width="{:.3}"/>"#,
            element.x,
            element.y,
            element.width.max(0.0),
            element.height.max(0.0),
            element.corner_radius.max(0.0),
            fill,
            stroke,
            stroke_width
        ),
        "circle" => format!(
            r#"<circle cx="{:.3}" cy="{:.3}" r="{:.3}" fill="{}" stroke="{}" stroke-width="{:.3}"/>"#,
            element.x,
            element.y,
            element.radius.max(0.0),
            fill,
            stroke,
            stroke_width
        ),
        "line" | "arrow" => {
            let draw_progress = if element.animation.as_deref() == Some("draw") {
                progress
            } else {
                1.0
            };
            let x2 = element.x + (element.x2 - element.x) * draw_progress;
            let y2 = element.y + (element.y2 - element.y) * draw_progress;
            let mut value = format!(
                r#"<line x1="{:.3}" y1="{:.3}" x2="{:.3}" y2="{:.3}" stroke="{}" stroke-width="{:.3}" stroke-linecap="round"/>"#,
                element.x, element.y, x2, y2, stroke, stroke_width
            );
            if element.kind == "arrow" && draw_progress > 0.08 {
                let head = arrow_head(element.x, element.y, x2, y2, (stroke_width * 3.6).max(10.0));
                value.push_str(&format!(
                    r#"<polygon points="{}" fill="{}"/>"#,
                    head, stroke
                ));
            }
            value
        }
        _ => unreachable!(),
    };

    Ok(Some(format!("{wrapper_start}{body}{wrapper_end}")))
}

fn scene_svg(scene: &VideoSceneSpec, width: u32, height: u32, time: f64) -> Result<String, String> {
    let background = validate_color(scene.background.as_deref(), "#0b0f14")?;
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><rect width="100%" height="100%" fill="{background}"/>"#
    );
    for element in &scene.elements {
        if let Some(value) = element_svg(element, time)? {
            svg.push_str(&value);
        }
    }
    svg.push_str("</svg>");
    Ok(svg)
}

fn render_svg_png(
    svg: &str,
    width: u32,
    height: u32,
    options: &usvg::Options<'_>,
    output: &Path,
) -> Result<(), String> {
    let tree = usvg::Tree::from_str(svg, options)
        .map_err(|error| format!("Generated scene SVG is invalid: {error}"))?;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| "Could not allocate generated-scene frame.".to_string())?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    pixmap
        .save_png(output)
        .map_err(|error| format!("Could not save generated-scene frame: {error}"))
}

fn ffmpeg_encode_frames(
    app: &AppHandle,
    frames_dir: &Path,
    fps: u32,
    output: &Path,
) -> Result<(), String> {
    let ffmpeg = video::ensure_ffmpeg_program(app)?;
    let input = frames_dir.join("frame-%06d.png");
    let mut command = Command::new(ffmpeg);
    command
        .args([
            "-hide_banner",
            "-nostats",
            "-loglevel",
            "error",
            "-y",
            "-framerate",
            &fps.to_string(),
            "-i",
        ])
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
        .arg(output);
    video::configure_background_command(&mut command);
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    let output_result = command
        .output()
        .map_err(|error| format!("Could not start generated-scene encoder: {error}"))?;
    if !output_result.status.success() {
        let mut detail = String::from_utf8_lossy(&output_result.stderr)
            .trim()
            .to_string();
        if detail.len() > 4000 {
            detail.truncate(4000);
        }
        return Err(if detail.is_empty() {
            format!(
                "Generated-scene encoder exited with status {}.",
                output_result.status
            )
        } else {
            format!("Generated-scene encoder failed: {detail}")
        });
    }
    if !output.is_file()
        || fs::metadata(output)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true)
    {
        return Err("Generated-scene encoder produced no usable output.".to_string());
    }
    Ok(())
}

fn scene_paths(
    workspace: &Workspace,
    project: &VideoProductionProject,
    scene_id: &str,
) -> Result<(PathBuf, String, PathBuf, String, PathBuf), String> {
    let timestamp = now_millis()?;
    let sequence = SCENE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let base = format!("{}-{timestamp}-{sequence}", scene_slug(scene_id));
    let source_relative = format!("{}/animations/source/{base}.json", project.relative_path);
    let output_relative = format!("{}/animations/generated/{base}.mp4", project.relative_path);
    let frames_relative = format!(
        "{}/animations/generated/.frames-{base}",
        project.relative_path
    );
    let source = crate::video_production::resolve_project_path(
        workspace,
        project,
        &source_relative,
        AccessOperation::Write,
        false,
    )?;
    let output = crate::video_production::resolve_project_path(
        workspace,
        project,
        &output_relative,
        AccessOperation::Write,
        false,
    )?;
    let frames = crate::video_production::resolve_project_path(
        workspace,
        project,
        &frames_relative,
        AccessOperation::Write,
        false,
    )?;
    Ok((source, source_relative, output, output_relative, frames))
}

pub(crate) fn render_scene(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    scene: VideoSceneSpec,
) -> Result<VideoSceneRender, String> {
    validate_scene(&scene)?;
    let project = video_production::get_project(workspace, project_id)?;
    let fps = project.fps.clamp(12, 60);
    let frame_count = ((scene.duration_seconds * f64::from(fps)).ceil() as u32).max(1);
    let (source, source_relative, output, output_relative, frames_dir) =
        scene_paths(workspace, &project, &scene.id)?;

    if source.exists() || output.exists() || frames_dir.exists() {
        return Err("Generated-scene output path unexpectedly already exists.".to_string());
    }

    let source_json = serde_json::to_vec_pretty(&scene)
        .map_err(|error| format!("Could not serialize generated scene: {error}"))?;
    fs::write(&source, source_json)
        .map_err(|error| format!("Could not save generated-scene source: {error}"))?;
    fs::create_dir(&frames_dir)
        .map_err(|error| format!("Could not create generated-scene frame directory: {error}"))?;

    let result = (|| {
        video_production::update_project_status(
            workspace,
            project_id,
            "generating",
            Some("Rendering generated 2D scene."),
        )?;

        let mut options = usvg::Options::default();
        options.fontdb_mut().load_system_fonts();

        for frame_index in 0..frame_count {
            let time = f64::from(frame_index) / f64::from(fps);
            let svg = scene_svg(&scene, project.width, project.height, time)?;
            let frame = frames_dir.join(format!("frame-{frame_index:06}.png"));
            render_svg_png(&svg, project.width, project.height, &options, &frame)?;
        }

        ffmpeg_encode_frames(app, &frames_dir, fps, &output)?;

        let source_asset = source_relative
            .strip_prefix(&format!("{}/", project.relative_path))
            .ok_or_else(|| "Generated-scene source escaped its Video Project.".to_string())?;
        let output_asset = output_relative
            .strip_prefix(&format!("{}/", project.relative_path))
            .ok_or_else(|| "Generated-scene render escaped its Video Project.".to_string())?;
        video_production::register_asset(
            workspace,
            project_id,
            "animation-source",
            source_asset,
            Some(&format!("Generated scene source: {}", scene.id)),
        )?;
        video_production::register_asset(
            workspace,
            project_id,
            "animation",
            output_asset,
            Some(&format!("Generated 2D scene: {}", scene.id)),
        )?;
        video_production::update_project_status(
            workspace,
            project_id,
            "editing",
            Some("Generated 2D scene rendered and registered."),
        )?;

        Ok(VideoSceneRender {
            project_id: project.id,
            scene_id: scene.id.clone(),
            source_path: source_relative,
            output_path: output_relative,
            duration_seconds: scene.duration_seconds,
            frame_count,
            width: project.width,
            height: project.height,
            fps,
        })
    })();

    let _ = fs::remove_dir_all(&frames_dir);
    if let Err(error) = &result {
        let _ = fs::remove_file(&output);
        let _ = video_production::update_project_status(
            workspace,
            project_id,
            "failed",
            Some(&format!("Generated 2D scene failed: {error}")),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        element_progress, render_svg_png, scene_slug, scene_svg, validate_scene, VideoSceneElement,
        VideoSceneSpec,
    };

    fn sample_scene() -> VideoSceneSpec {
        VideoSceneSpec {
            version: 1,
            id: "Router explanation".to_string(),
            duration_seconds: 3.0,
            background: Some("#0b0f14".to_string()),
            elements: vec![
                VideoSceneElement {
                    kind: "rect".to_string(),
                    text: None,
                    x: 100.0,
                    y: 100.0,
                    width: 260.0,
                    height: 120.0,
                    x2: 0.0,
                    y2: 0.0,
                    radius: 0.0,
                    corner_radius: 16.0,
                    font_size: 0.0,
                    stroke_width: 3.0,
                    fill: Some("#17212b".to_string()),
                    stroke: Some("#5fa4d7".to_string()),
                    font_family: None,
                    start_seconds: 0.0,
                    end_seconds: Some(0.6),
                    animation: Some("scale".to_string()),
                },
                VideoSceneElement {
                    kind: "text".to_string(),
                    text: Some("Home router".to_string()),
                    x: 145.0,
                    y: 170.0,
                    width: 0.0,
                    height: 0.0,
                    x2: 0.0,
                    y2: 0.0,
                    radius: 0.0,
                    corner_radius: 0.0,
                    font_size: 38.0,
                    stroke_width: 0.0,
                    fill: Some("#eef4f8".to_string()),
                    stroke: None,
                    font_family: Some("DejaVu Sans".to_string()),
                    start_seconds: 0.35,
                    end_seconds: Some(0.9),
                    animation: Some("fade".to_string()),
                },
                VideoSceneElement {
                    kind: "arrow".to_string(),
                    text: None,
                    x: 360.0,
                    y: 160.0,
                    width: 0.0,
                    height: 0.0,
                    x2: 700.0,
                    y2: 160.0,
                    radius: 0.0,
                    corner_radius: 0.0,
                    font_size: 0.0,
                    stroke_width: 5.0,
                    fill: None,
                    stroke: Some("#45d0a5".to_string()),
                    font_family: None,
                    start_seconds: 0.8,
                    end_seconds: Some(1.5),
                    animation: Some("draw".to_string()),
                },
            ],
        }
    }

    #[test]
    fn generated_scene_validation_accepts_core_tutorial_shapes() {
        validate_scene(&sample_scene()).unwrap();
    }

    #[test]
    fn generated_scene_rejects_external_or_unknown_element_types() {
        let mut scene = sample_scene();
        scene.elements[0].kind = "image".to_string();
        assert!(validate_scene(&scene).is_err());
    }

    #[test]
    fn generated_scene_svg_is_self_contained_and_escapes_text() {
        let mut scene = sample_scene();
        scene.elements[1].text = Some("A < B & C".to_string());
        let svg = scene_svg(&scene, 1920, 1080, 1.6).unwrap();
        assert!(svg.contains("A &lt; B &amp; C"));
        assert!(svg.contains("<polygon"));
        assert!(!svg.contains("http://") || svg.contains("http://www.w3.org/2000/svg"));
    }

    #[test]
    fn animation_progress_is_bounded() {
        let element = &sample_scene().elements[0];
        assert!(element_progress(element, -0.1).is_none());
        assert_eq!(element_progress(element, 1.0), Some(1.0));
    }

    #[test]
    fn scene_ids_are_safe_for_project_paths() {
        assert_eq!(scene_slug("../../Router Demo !!!"), "router-demo");
    }

    #[test]
    fn generated_scene_renders_real_png_frame() {
        let scene = sample_scene();
        let svg = scene_svg(&scene, 640, 360, 1.2).unwrap();
        let mut options = resvg::usvg::Options::default();
        options.fontdb_mut().load_system_fonts();

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output = std::env::temp_dir().join(format!(
            "repotunnel-video-scene-{}-{nonce}.png",
            std::process::id()
        ));
        render_svg_png(&svg, 640, 360, &options, &output).unwrap();

        let bytes = fs::read(&output).unwrap();
        assert!(bytes.len() > 1000);
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        fs::remove_file(output).unwrap();
    }
}
