use std::{
    collections::{HashMap, HashSet},
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
    #[schemars(skip)]
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

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoDiagramNode {
    pub(crate) id: String,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) detail: Option<String>,
    #[serde(default)]
    pub(crate) group: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoDiagramEdge {
    pub(crate) from: String,
    pub(crate) to: String,
    #[serde(default)]
    pub(crate) label: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoDiagramSpec {
    pub(crate) id: String,
    pub(crate) template: String,
    pub(crate) duration_seconds: f64,
    #[serde(default)]
    pub(crate) title: Option<String>,
    pub(crate) nodes: Vec<VideoDiagramNode>,
    #[serde(default)]
    pub(crate) edges: Vec<VideoDiagramEdge>,
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
    pub(crate) layout: VideoSceneLayoutReport,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoSceneLayoutIssue {
    pub(crate) severity: String,
    pub(crate) code: String,
    pub(crate) element_index: Option<usize>,
    pub(crate) message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoSceneLayoutReport {
    pub(crate) scene_id: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) passed: bool,
    pub(crate) issues: Vec<VideoSceneLayoutIssue>,
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

fn validate_diagram(spec: &VideoDiagramSpec) -> Result<(), String> {
    if !matches!(
        spec.template.as_str(),
        "flow_diagram"
            | "architecture_diagram"
            | "comparison"
            | "timeline"
            | "token_flow"
            | "before_after"
            | "metric_cards"
    ) {
        return Err(
            "Diagram template must be flow_diagram, architecture_diagram, comparison, timeline, token_flow, before_after, or metric_cards."
                .to_string(),
        );
    }
    if !(0.25..=MAX_SCENE_SECONDS).contains(&spec.duration_seconds)
        || !spec.duration_seconds.is_finite()
    {
        return Err(format!(
            "Diagram duration must be between 0.25 and {MAX_SCENE_SECONDS:.0} seconds."
        ));
    }
    let max_nodes = match spec.template.as_str() {
        "flow_diagram" | "timeline" | "token_flow" => 6,
        "comparison" | "before_after" | "metric_cards" => 4,
        _ => 8,
    };
    if spec.nodes.is_empty() || spec.nodes.len() > max_nodes {
        return Err(format!(
            "Diagram template '{}' requires 1 to {max_nodes} nodes.",
            spec.template
        ));
    }
    if matches!(spec.template.as_str(), "comparison" | "before_after") && spec.nodes.len() < 2 {
        return Err(format!(
            "Diagram template '{}' requires at least two nodes.",
            spec.template
        ));
    }
    if spec.edges.len() > 16 {
        return Err("Diagrams are limited to 16 relationships.".to_string());
    }

    let mut ids = HashSet::new();
    for node in &spec.nodes {
        let id = node.id.trim();
        if id.is_empty()
            || id.len() > 64
            || !id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        {
            return Err("Diagram node IDs must be 1 to 64 safe ASCII characters.".to_string());
        }
        if !ids.insert(id.to_string()) {
            return Err(format!("Diagram node ID '{id}' is duplicated."));
        }
        let label = node.label.trim();
        if label.is_empty()
            || label.chars().count() > 80
            || label.contains('\n')
            || label.contains('\r')
        {
            return Err(
                "Diagram node labels must be one line and at most 80 characters.".to_string(),
            );
        }
        if node.detail.as_deref().is_some_and(|detail| {
            detail.chars().count() > 160 || detail.contains('\n') || detail.contains('\r')
        }) {
            return Err(
                "Diagram node detail must be one line and at most 160 characters.".to_string(),
            );
        }
        if node.group.as_deref().is_some_and(|group| {
            group.chars().count() > 60 || group.contains('\n') || group.contains('\r')
        }) {
            return Err(
                "Diagram node group must be one line and at most 60 characters.".to_string(),
            );
        }
    }
    if spec.title.as_deref().is_some_and(|title| {
        title.trim().is_empty()
            || title.chars().count() > 120
            || title.contains('\n')
            || title.contains('\r')
    }) {
        return Err("Diagram title must be one line and at most 120 characters.".to_string());
    }
    for edge in &spec.edges {
        if edge.from == edge.to {
            return Err("Diagram relationships cannot point a node to itself.".to_string());
        }
        if !ids.contains(edge.from.trim()) || !ids.contains(edge.to.trim()) {
            return Err("Diagram relationship references an unknown node ID.".to_string());
        }
        if edge.label.as_deref().is_some_and(|label| {
            label.chars().count() > 48 || label.contains('\n') || label.contains('\r')
        }) {
            return Err(
                "Diagram relationship labels must be one line and at most 48 characters."
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn fitted_font(text: &str, max_width: f64, preferred: f64, minimum: f64) -> f64 {
    let chars = text.chars().count().max(1) as f64;
    (max_width / (chars * 0.58)).clamp(minimum, preferred)
}

#[allow(clippy::too_many_arguments)]
fn diagram_element(
    kind: &str,
    text: Option<String>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    x2: f64,
    y2: f64,
    font_size: f64,
    fill: Option<&str>,
    stroke: Option<&str>,
    start_seconds: f64,
    animation: Option<&str>,
) -> VideoSceneElement {
    VideoSceneElement {
        kind: kind.to_string(),
        text,
        x,
        y,
        width,
        height,
        x2,
        y2,
        radius: 0.0,
        corner_radius: if kind == "rect" { 18.0 } else { 0.0 },
        font_size,
        stroke_width: if matches!(kind, "line" | "arrow") {
            4.0
        } else {
            2.0
        },
        fill: fill.map(str::to_string),
        stroke: stroke.map(str::to_string),
        font_family: Some("Sans".to_string()),
        start_seconds,
        end_seconds: None,
        animation: animation.map(str::to_string),
    }
}

fn diagram_scene(
    width: u32,
    height: u32,
    spec: &VideoDiagramSpec,
) -> Result<VideoSceneSpec, String> {
    validate_diagram(spec)?;
    let width = f64::from(width);
    let height = f64::from(height);
    let margin_x = (width * 0.05).max(24.0);
    let margin_y = (height * 0.07).max(24.0);
    let title_height = if spec.title.is_some() {
        (height * 0.11).clamp(44.0, 90.0)
    } else {
        0.0
    };
    let content_top = margin_y + title_height + if spec.title.is_some() { 16.0 } else { 0.0 };
    let content_height = (height - content_top - margin_y).max(80.0);
    let content_width = (width - margin_x * 2.0).max(120.0);
    let count = spec.nodes.len();

    let (columns, rows) = match spec.template.as_str() {
        "architecture_diagram" if count > 4 => (4usize, 2usize),
        _ => (count, 1usize),
    };
    let gap_x = if columns > 1 {
        (content_width * 0.035).clamp(18.0, 52.0)
    } else {
        0.0
    };
    let gap_y = if rows > 1 {
        (content_height * 0.09).clamp(18.0, 48.0)
    } else {
        0.0
    };
    let card_width =
        ((content_width - gap_x * (columns.saturating_sub(1) as f64)) / columns as f64).max(90.0);
    let card_height =
        ((content_height - gap_y * (rows.saturating_sub(1) as f64)) / rows as f64).max(76.0);

    let mut elements = Vec::new();
    if let Some(title) = spec.title.as_deref() {
        let title = title.trim();
        let font = fitted_font(
            title,
            content_width,
            (height * 0.055).clamp(26.0, 52.0),
            18.0,
        );
        elements.push(diagram_element(
            "text",
            Some(title.to_string()),
            margin_x,
            margin_y + font,
            0.0,
            0.0,
            0.0,
            0.0,
            font,
            Some("#e8eef4"),
            None,
            0.0,
            Some("fade"),
        ));
    }

    let mut positions: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
    for (index, node) in spec.nodes.iter().enumerate() {
        let row = if rows == 1 { 0 } else { index / columns };
        let column = if rows == 1 { index } else { index % columns };
        let row_count = if rows == 1 || row == 0 {
            columns.min(count)
        } else {
            count - columns
        };
        let row_width =
            card_width * row_count as f64 + gap_x * (row_count.saturating_sub(1) as f64);
        let row_left = margin_x + (content_width - row_width) / 2.0;
        let x = row_left + column as f64 * (card_width + gap_x);
        let y = content_top + row as f64 * (card_height + gap_y);
        positions.insert(
            node.id.trim().to_string(),
            (
                x + card_width / 2.0,
                y + card_height / 2.0,
                card_width,
                card_height,
            ),
        );
    }

    for (index, edge) in spec.edges.iter().enumerate() {
        let Some(&(from_x, from_y, from_w, from_h)) = positions.get(edge.from.trim()) else {
            continue;
        };
        let Some(&(to_x, to_y, to_w, to_h)) = positions.get(edge.to.trim()) else {
            continue;
        };
        let horizontal = (to_x - from_x).abs() >= (to_y - from_y).abs();
        let (x1, y1, x2, y2) = if horizontal {
            let direction = if to_x >= from_x { 1.0 } else { -1.0 };
            (
                from_x + direction * from_w * 0.52,
                from_y,
                to_x - direction * to_w * 0.52,
                to_y,
            )
        } else {
            let direction = if to_y >= from_y { 1.0 } else { -1.0 };
            (
                from_x,
                from_y + direction * from_h * 0.52,
                to_x,
                to_y - direction * to_h * 0.52,
            )
        };
        elements.push(diagram_element(
            "arrow",
            None,
            x1,
            y1,
            0.0,
            0.0,
            x2,
            y2,
            0.0,
            Some("#83b6df"),
            Some("#83b6df"),
            0.15 + index as f64 * 0.05,
            Some("draw"),
        ));
        if let Some(label) = edge
            .label
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let max_width = ((x2 - x1).abs().max((y2 - y1).abs()) * 0.7).max(90.0);
            let font = fitted_font(label, max_width, 18.0, 12.0);
            elements.push(diagram_element(
                "text",
                Some(label.to_string()),
                (x1 + x2) / 2.0 + 8.0,
                (y1 + y2) / 2.0 - 8.0,
                0.0,
                0.0,
                0.0,
                0.0,
                font,
                Some("#9fb8ca"),
                None,
                0.2 + index as f64 * 0.05,
                Some("fade"),
            ));
        }
    }

    for (index, node) in spec.nodes.iter().enumerate() {
        let &(center_x, center_y, card_width, card_height) = positions
            .get(node.id.trim())
            .ok_or_else(|| "Diagram node layout could not be resolved.".to_string())?;
        let x = center_x - card_width / 2.0;
        let y = center_y - card_height / 2.0;
        let start = 0.25 + index as f64 * 0.08;
        elements.push(diagram_element(
            "rect",
            None,
            x,
            y,
            card_width,
            card_height,
            0.0,
            0.0,
            0.0,
            Some("#111c24"),
            Some("#4f7fa0"),
            start,
            Some("scale"),
        ));

        let inset = (card_width * 0.08).clamp(12.0, 28.0);
        let text_width = (card_width - inset * 2.0).max(60.0);
        let label = node.label.trim();
        let label_font = fitted_font(
            label,
            text_width,
            (card_height * 0.22).clamp(18.0, 32.0),
            13.0,
        );
        let has_detail = node
            .detail
            .as_deref()
            .map(str::trim)
            .is_some_and(|detail| !detail.is_empty());
        let label_y = if has_detail {
            y + card_height * 0.42
        } else {
            center_y + label_font * 0.35
        };
        elements.push(diagram_element(
            "text",
            Some(label.to_string()),
            x + inset,
            label_y,
            0.0,
            0.0,
            0.0,
            0.0,
            label_font,
            Some("#e8eef4"),
            None,
            start + 0.08,
            Some("fade"),
        ));

        if let Some(detail) = node
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
        {
            let detail_font = fitted_font(
                detail,
                text_width,
                (card_height * 0.14).clamp(13.0, 20.0),
                10.0,
            );
            elements.push(diagram_element(
                "text",
                Some(detail.to_string()),
                x + inset,
                y + card_height * 0.72,
                0.0,
                0.0,
                0.0,
                0.0,
                detail_font,
                Some("#a8bac7"),
                None,
                start + 0.14,
                Some("fade"),
            ));
        }
    }

    Ok(VideoSceneSpec {
        version: 1,
        id: spec.id.clone(),
        duration_seconds: spec.duration_seconds,
        background: Some("#0b0f14".to_string()),
        elements,
    })
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

#[derive(Clone, Copy)]
struct LayoutBox {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl LayoutBox {
    fn width(self) -> f64 {
        (self.right - self.left).max(0.0)
    }

    fn height(self) -> f64 {
        (self.bottom - self.top).max(0.0)
    }

    fn overlaps(self, other: Self) -> bool {
        self.left < other.right
            && self.right > other.left
            && self.top < other.bottom
            && self.bottom > other.top
    }
}

fn text_layout_box(element: &VideoSceneElement) -> Option<LayoutBox> {
    let text = element.text.as_deref()?.trim();
    if text.is_empty() {
        return None;
    }
    let font_size = if element.font_size > 0.0 {
        element.font_size
    } else {
        48.0
    };
    let mut line_count = 0usize;
    let mut max_chars = 0usize;
    for line in text.lines() {
        line_count += 1;
        max_chars = max_chars.max(line.chars().count());
    }
    let line_count = line_count.max(1);
    let estimated_width = max_chars as f64 * font_size * 0.58;
    let estimated_height = font_size * (1.0 + 1.2 * (line_count.saturating_sub(1) as f64));
    Some(LayoutBox {
        left: element.x,
        top: element.y - font_size,
        right: element.x + estimated_width,
        bottom: element.y - font_size + estimated_height,
    })
}

fn shape_layout_box(element: &VideoSceneElement) -> Option<LayoutBox> {
    match element.kind.as_str() {
        "rect" => Some(LayoutBox {
            left: element.x,
            top: element.y,
            right: element.x + element.width.max(0.0),
            bottom: element.y + element.height.max(0.0),
        }),
        "circle" => Some(LayoutBox {
            left: element.x - element.radius.max(0.0),
            top: element.y - element.radius.max(0.0),
            right: element.x + element.radius.max(0.0),
            bottom: element.y + element.radius.max(0.0),
        }),
        "line" | "arrow" => {
            let padding = element.stroke_width.max(1.0) * 2.0;
            Some(LayoutBox {
                left: element.x.min(element.x2) - padding,
                top: element.y.min(element.y2) - padding,
                right: element.x.max(element.x2) + padding,
                bottom: element.y.max(element.y2) + padding,
            })
        }
        _ => None,
    }
}

fn layout_issue(
    severity: &str,
    code: &str,
    element_index: Option<usize>,
    message: impl Into<String>,
) -> VideoSceneLayoutIssue {
    VideoSceneLayoutIssue {
        severity: severity.to_string(),
        code: code.to_string(),
        element_index,
        message: message.into(),
    }
}

fn layout_report(scene: &VideoSceneSpec, width: u32, height: u32) -> VideoSceneLayoutReport {
    let canvas_width = f64::from(width);
    let canvas_height = f64::from(height);
    let safe_margin = (canvas_width.min(canvas_height) * 0.03).clamp(18.0, 48.0);
    let mut issues = Vec::new();
    let mut text_boxes = Vec::<(usize, LayoutBox)>::new();
    let mut explanatory_text = 0usize;

    for (index, element) in scene.elements.iter().enumerate() {
        match element.kind.as_str() {
            "text" => {
                let text = element.text.as_deref().unwrap_or_default().trim();
                if text.is_empty() {
                    issues.push(layout_issue(
                        "warning",
                        "emptyText",
                        Some(index),
                        "Text element is empty.",
                    ));
                    continue;
                }
                explanatory_text += 1;
                let font_size = if element.font_size > 0.0 {
                    element.font_size
                } else {
                    48.0
                };
                if font_size < 18.0 {
                    issues.push(layout_issue(
                        "warning",
                        "smallText",
                        Some(index),
                        format!("Text font size {font_size:.1}px may be difficult to read."),
                    ));
                }
                if text.contains('\n') {
                    issues.push(layout_issue(
                        "error",
                        "multilineTextUnsupported",
                        Some(index),
                        "Generated-scene text contains line breaks, but the current renderer does not lay out multiline text safely. Split it into separate text elements.",
                    ));
                }
                if let Some(bounds) = text_layout_box(element) {
                    if element.width > 0.0 && bounds.width() > element.width + 1.0 {
                        issues.push(layout_issue(
                            "error",
                            "textOverflow",
                            Some(index),
                            format!(
                                "Text is estimated at {:.0}px wide but its declared width is {:.0}px.",
                                bounds.width(),
                                element.width
                            ),
                        ));
                    }
                    if element.height > 0.0 && bounds.height() > element.height + 1.0 {
                        issues.push(layout_issue(
                            "error",
                            "textOverflow",
                            Some(index),
                            format!(
                                "Text is estimated at {:.0}px high but its declared height is {:.0}px.",
                                bounds.height(),
                                element.height
                            ),
                        ));
                    }
                    if bounds.left < 0.0
                        || bounds.top < 0.0
                        || bounds.right > canvas_width
                        || bounds.bottom > canvas_height
                    {
                        issues.push(layout_issue(
                            "error",
                            "outOfBounds",
                            Some(index),
                            format!("Text extends outside the {width}x{height} scene canvas."),
                        ));
                    } else if bounds.left < safe_margin
                        || bounds.top < safe_margin
                        || canvas_width - bounds.right < safe_margin
                        || canvas_height - bounds.bottom < safe_margin
                    {
                        issues.push(layout_issue(
                            "warning",
                            "safeArea",
                            Some(index),
                            "Text is very close to the scene edge and may be unsafe for player UI/cropping.",
                        ));
                    }
                    text_boxes.push((index, bounds));
                }
            }
            "rect" => {
                if element.width <= 0.0 || element.height <= 0.0 {
                    issues.push(layout_issue(
                        "error",
                        "invalidGeometry",
                        Some(index),
                        "Rectangle width and height must both be greater than zero.",
                    ));
                }
            }
            "circle" => {
                if element.radius <= 0.0 {
                    issues.push(layout_issue(
                        "error",
                        "invalidGeometry",
                        Some(index),
                        "Circle radius must be greater than zero.",
                    ));
                }
            }
            "line" | "arrow"
                if (element.x - element.x2).abs() < 0.001
                    && (element.y - element.y2).abs() < 0.001 =>
            {
                issues.push(layout_issue(
                    "error",
                    "invalidGeometry",
                    Some(index),
                    "Line/arrow start and end points must be different.",
                ));
            }
            _ => {}
        }

        if element.kind != "text" {
            if let Some(bounds) = shape_layout_box(element) {
                if bounds.left < 0.0
                    || bounds.top < 0.0
                    || bounds.right > canvas_width
                    || bounds.bottom > canvas_height
                {
                    issues.push(layout_issue(
                        "error",
                        "outOfBounds",
                        Some(index),
                        format!(
                            "{} extends outside the {width}x{height} scene canvas.",
                            element.kind
                        ),
                    ));
                }
            }
        }
    }

    for left_index in 0..text_boxes.len() {
        for right_index in (left_index + 1)..text_boxes.len() {
            let (first_index, first) = text_boxes[left_index];
            let (second_index, second) = text_boxes[right_index];
            if first.overlaps(second) {
                issues.push(layout_issue(
                    "error",
                    "textCollision",
                    Some(second_index),
                    format!(
                        "Text element {} overlaps text element {}.",
                        second_index + 1,
                        first_index + 1
                    ),
                ));
            }
        }
    }

    if explanatory_text == 0 && scene.elements.len() >= 2 {
        issues.push(layout_issue(
            "warning",
            "semanticDensity",
            None,
            "Scene contains multiple visual elements but no explanatory text; verify that the diagram conveys a real teaching point.",
        ));
    }

    let passed = !issues.iter().any(|issue| issue.severity == "error");
    VideoSceneLayoutReport {
        scene_id: scene.id.clone(),
        width,
        height,
        passed,
        issues,
    }
}

pub(crate) fn validate_scene_layout(
    workspace: &Workspace,
    project_id: &str,
    scene: &VideoSceneSpec,
) -> Result<VideoSceneLayoutReport, String> {
    validate_scene(scene)?;
    let project = video_production::get_project(workspace, project_id)?;
    Ok(layout_report(scene, project.width, project.height))
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
    allow_automatic_package_install: bool,
) -> Result<(), String> {
    let ffmpeg = video::ffmpeg_program(app, allow_automatic_package_install)?;
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

pub(crate) fn render_diagram(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    diagram: VideoDiagramSpec,
) -> Result<VideoSceneRender, String> {
    let project = video_production::get_project(workspace, project_id)?;
    let scene = diagram_scene(project.width, project.height, &diagram)?;
    render_scene(app, workspace, project_id, scene)
}

pub(crate) fn render_scene(
    app: &AppHandle,
    workspace: &Workspace,
    project_id: &str,
    scene: VideoSceneSpec,
) -> Result<VideoSceneRender, String> {
    validate_scene(&scene)?;
    let project = video_production::get_project(workspace, project_id)?;
    let layout = layout_report(&scene, project.width, project.height);
    if !layout.passed {
        let summary = layout
            .issues
            .iter()
            .filter(|issue| issue.severity == "error")
            .take(3)
            .map(|issue| issue.message.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        return Err(format!(
            "Generated-scene layout preflight failed before rendering. {summary}"
        ));
    }
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

        ffmpeg_encode_frames(
            app,
            &frames_dir,
            fps,
            &output,
            project.resource_policy.allow_automatic_package_install,
        )?;

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
        let _ = video_production::record_scene_render(
            workspace,
            project_id,
            &scene.id,
            scene.duration_seconds,
            &source_relative,
            &output_relative,
        );
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
            layout,
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
        diagram_scene, element_progress, layout_report, render_svg_png, scene_slug, scene_svg,
        validate_diagram, validate_scene, VideoDiagramEdge, VideoDiagramNode, VideoDiagramSpec,
        VideoSceneElement, VideoSceneSpec,
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

    fn sample_diagram() -> VideoDiagramSpec {
        VideoDiagramSpec {
            id: "request-flow".to_string(),
            template: "flow_diagram".to_string(),
            duration_seconds: 4.0,
            title: Some("Request flow".to_string()),
            nodes: vec![
                VideoDiagramNode {
                    id: "client".to_string(),
                    label: "Client".to_string(),
                    detail: Some("Sends request".to_string()),
                    group: None,
                },
                VideoDiagramNode {
                    id: "api".to_string(),
                    label: "API".to_string(),
                    detail: Some("Validates input".to_string()),
                    group: None,
                },
                VideoDiagramNode {
                    id: "service".to_string(),
                    label: "Service".to_string(),
                    detail: Some("Returns result".to_string()),
                    group: None,
                },
            ],
            edges: vec![
                VideoDiagramEdge {
                    from: "client".to_string(),
                    to: "api".to_string(),
                    label: Some("HTTP".to_string()),
                },
                VideoDiagramEdge {
                    from: "api".to_string(),
                    to: "service".to_string(),
                    label: Some("call".to_string()),
                },
            ],
        }
    }

    #[test]
    fn semantic_diagram_compiles_to_preflight_safe_scene() {
        let diagram = sample_diagram();
        validate_diagram(&diagram).unwrap();
        let scene = diagram_scene(1920, 1080, &diagram).unwrap();
        validate_scene(&scene).unwrap();
        let report = layout_report(&scene, 1920, 1080);
        assert!(report.passed, "{:?}", report.issues);
        assert_eq!(
            scene
                .elements
                .iter()
                .filter(|element| element.kind == "arrow")
                .count(),
            2
        );
        assert!(scene
            .elements
            .iter()
            .any(|element| element.text.as_deref() == Some("Validates input")));
    }

    #[test]
    fn semantic_diagram_rejects_unknown_relationship_targets() {
        let mut diagram = sample_diagram();
        diagram.edges[0].to = "missing".to_string();
        assert!(validate_diagram(&diagram).is_err());
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
    fn layout_preflight_catches_overflow_and_collisions_before_render() {
        let scene = sample_scene();
        assert!(layout_report(&scene, 1920, 1080).passed);

        let mut overflow = sample_scene();
        overflow.elements[1].x = 1880.0;
        let report = layout_report(&overflow, 1920, 1080);
        assert!(!report.passed);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == "outOfBounds"));

        let mut collision = sample_scene();
        let mut duplicate = collision.elements[1].clone();
        duplicate.text = Some("Second label".to_string());
        collision.elements.push(duplicate);
        let report = layout_report(&collision, 1920, 1080);
        assert!(!report.passed);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == "textCollision"));

        let mut constrained = sample_scene();
        constrained.elements[1].width = 80.0;
        let report = layout_report(&constrained, 1920, 1080);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == "textOverflow"));
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
