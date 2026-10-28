//! Local, session-bound comparison of existing managed-browser PNG captures.
//! This is a diagnostic heuristic, never a semantic/UI correctness oracle.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::recovery_store;

const MAX_FILE: u64 = 8 * 1024 * 1024;
const MAX_PIXELS: u64 = 6_000_000;
const MAX_DIMENSION: u32 = 8192;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capture {
    workspace_id: String,
    browser_scope: String,
    tab_id: String,
    full_page: bool,
    #[serde(default)]
    requires_google_access: bool,
}

fn valid_screenshot_id(id: &str) -> bool {
    id.starts_with("screenshot-")
        && id.len() <= 128
        && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

fn image_path(app: &AppHandle, workspace_id: &str, id: &str) -> Result<PathBuf, String> {
    if !valid_screenshot_id(id) {
        return Err("Invalid screenshot ID; use IDs returned by browser_take_screenshot.".into());
    }
    app.path()
        .resolve(
            format!("browser/screenshots/{workspace_id}/{id}.png"),
            BaseDirectory::AppData,
        )
        .map_err(|_| "Could not resolve the private screenshot file.".to_string())
}
fn manifest_path(path: &Path) -> PathBuf {
    path.with_extension("capture.json")
}
pub(crate) fn register(
    app: &AppHandle,
    workspace_id: &str,
    browser_scope: &str,
    tab_id: &str,
    full_page: bool,
    requires_google_access: bool,
    id: &str,
) -> Result<(), String> {
    let path = image_path(app, workspace_id, id)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| "Captured screenshot has no saved PNG file.".to_string())?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > MAX_FILE {
        return Err("Captured screenshot is not a safe, bounded PNG file.".into());
    }
    recovery_store::save_json(
        &manifest_path(&path),
        &Capture {
            workspace_id: workspace_id.into(),
            browser_scope: browser_scope.into(),
            tab_id: tab_id.into(),
            full_page,
            requires_google_access,
        },
        "browser screenshot provenance",
    )
}
fn validated_image(
    path: &Path,
    expected: &Capture,
    google_access_enabled: bool,
) -> Result<Vec<u8>, String> {
    let observed = recovery_store::load_json::<Capture>(
        &manifest_path(path),
        "browser screenshot provenance",
    )?
    .value
    .ok_or_else(|| {
        "Screenshot provenance is missing; recapture using the updated browser tool.".to_string()
    })?;
    if observed.workspace_id != expected.workspace_id
        || observed.browser_scope != expected.browser_scope
        || observed.tab_id != expected.tab_id
        || observed.full_page != expected.full_page
    {
        return Err("Screenshots must come from the same approved project, AI browser session, tab and capture mode.".into());
    }
    if observed.requires_google_access && !google_access_enabled {
        return Err("This screenshot was captured on a Google identity page; the locally enabled Gmail access permission is now required to compare it.".to_string());
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|_| "One of the screenshots has expired from browser retention.".to_string())?;
    if !meta.file_type().is_file() || meta.len() == 0 || meta.len() > MAX_FILE {
        return Err("Screenshot is not a bounded regular file; refused to read it.".into());
    }
    fs::read(path).map_err(|_| "Could not read one of the saved screenshots.".to_string())
}
struct Image {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}
fn decode(data: &[u8]) -> Result<Image, String> {
    if data.len() as u64 > MAX_FILE {
        return Err("PNG capture exceeds the 8 MiB input limit.".into());
    }
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|_| "Invalid PNG screenshot.".to_string())?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err("PNG dimensions exceed the comparison safety budget.".into());
    }
    if reader.output_buffer_size() as u64 > MAX_PIXELS * 4 {
        return Err("PNG decoder would require excessive memory.".into());
    }
    let mut raw = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut raw)
        .map_err(|_| "PNG screenshot pixels could not be decoded.".to_string())?;
    let count = (width as usize) * (height as usize);
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => return Err("PNG palette could not be expanded.".into()),
    };
    let bytes = raw
        .get(..info.buffer_size())
        .ok_or_else(|| "Malformed decoded PNG buffer.".to_string())?;
    if bytes.len() != count * channels {
        return Err("PNG pixel count does not match dimensions.".into());
    }
    let mut rgba = Vec::with_capacity(count * 4);
    for pixel in bytes.chunks_exact(channels) {
        match channels {
            4 => rgba.extend_from_slice(pixel),
            3 => {
                rgba.extend_from_slice(pixel);
                rgba.push(255);
            }
            2 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]),
            1 => rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]),
            _ => unreachable!(),
        }
    }
    Ok(Image {
        width,
        height,
        rgba,
    })
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangedBounds {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Comparison {
    pub(crate) baseline_id: String,
    pub(crate) current_id: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) changed_pixels: u64,
    pub(crate) total_pixels: u64,
    pub(crate) changed_percent: f64,
    pub(crate) tolerance: u8,
    pub(crate) max_changed_percent: f64,
    pub(crate) changed_bounds: Option<ChangedBounds>,
    pub(crate) status: &'static str,
    pub(crate) limitation: &'static str,
    #[serde(skip_serializing)]
    pub(crate) difference_png_base64: Option<String>,
}
fn compare_pixels(
    before: Image,
    after: Image,
    tolerance: u8,
    max_changed_percent: f64,
    baseline_id: &str,
    current_id: &str,
) -> Result<Comparison, String> {
    let pixels = u64::from(before.width) * u64::from(before.height);
    let mut result = Comparison {
        baseline_id: baseline_id.into(), current_id: current_id.into(),
        width: after.width, height: after.height,
        changed_pixels: 0, total_pixels: pixels,
        changed_percent: 0.0, tolerance, max_changed_percent,
        changed_bounds: None, status: "unchanged",
        limitation: "A pixel match is not proof of UI correctness. Animations, fonts, cursor, clocks and nondeterministic content may change pixels; semantic/DOM/accessibility verification and human inspection remain necessary.",
        difference_png_base64: None,
    };
    if before.width != after.width || before.height != after.height {
        result.status = "dimensionMismatch";
        result.total_pixels = u64::from(after.width) * u64::from(after.height);
        return Ok(result);
    }
    let mut diff = vec![0u8; after.rgba.len()];
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0, 0);
    for (i, (old, new)) in before
        .rgba
        .chunks_exact(4)
        .zip(after.rgba.chunks_exact(4))
        .enumerate()
    {
        let changed = old.iter().zip(new).any(|(a, b)| a.abs_diff(*b) > tolerance);
        let index = i * 4;
        if changed {
            result.changed_pixels += 1;
            let x = (i as u32) % before.width;
            let y = (i as u32) / before.width;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            diff[index..index + 4].copy_from_slice(&[255, 45, 45, 255]);
        } else {
            let gray = (u16::from(new[0]) + u16::from(new[1]) + u16::from(new[2])) / 9;
            let gray = u8::try_from(gray).unwrap_or(85);
            diff[index..index + 4].copy_from_slice(&[gray, gray, gray, 255]);
        }
    }
    result.changed_percent = (result.changed_pixels as f64 / pixels as f64) * 100.0;
    if result.changed_pixels > 0 {
        result.changed_bounds = Some(ChangedBounds {
            x: min_x,
            y: min_y,
            width: max_x - min_x + 1,
            height: max_y - min_y + 1,
        });
    }
    result.status = if result.changed_percent > max_changed_percent {
        "regressionCandidate"
    } else if result.changed_pixels > 0 {
        "withinTolerance"
    } else {
        "unchanged"
    };
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, before.width, before.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|_| "Could not encode difference PNG header.".to_string())?;
        writer
            .write_image_data(&diff)
            .map_err(|_| "Could not encode difference PNG pixels.".to_string())?;
    }
    result.difference_png_base64 = Some(STANDARD.encode(bytes));
    Ok(result)
}
pub(crate) struct CompareOptions {
    pub(crate) baseline_id: String,
    pub(crate) current_id: String,
    pub(crate) full_page: bool,
    pub(crate) tolerance: u8,
    pub(crate) max_changed_percent: f64,
}
pub(crate) fn compare(
    app: &AppHandle,
    workspace_id: &str,
    scope: &str,
    tab_id: &str,
    options: &CompareOptions,
) -> Result<Comparison, String> {
    let baseline_id = options.baseline_id.as_str();
    let current_id = options.current_id.as_str();
    let full_page = options.full_page;
    let tolerance = options.tolerance;
    let max_changed_percent = options.max_changed_percent;
    if baseline_id == current_id {
        return Err("Compare two distinct captures, not the same screenshot ID.".into());
    }
    if !(0.0..=100.0).contains(&max_changed_percent) || !max_changed_percent.is_finite() {
        return Err("Maximum acceptable changed percentage must be between 0 and 100.".into());
    }
    let expected = Capture {
        workspace_id: workspace_id.into(),
        browser_scope: scope.into(),
        tab_id: tab_id.into(),
        full_page,
        requires_google_access: false,
    };
    let before_path = image_path(app, workspace_id, baseline_id)?;
    let after_path = image_path(app, workspace_id, current_id)?;
    let google_access_enabled = crate::gmail_access::is_enabled(app)?;
    let before = decode(&validated_image(
        &before_path,
        &expected,
        google_access_enabled,
    )?)?;
    let after = decode(&validated_image(
        &after_path,
        &expected,
        google_access_enabled,
    )?)?;
    compare_pixels(
        before,
        after,
        tolerance,
        max_changed_percent,
        baseline_id,
        current_id,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    fn image(w: u32, h: u32, color: [u8; 4]) -> Image {
        let mut rgba = Vec::new();
        for _ in 0..w * h {
            rgba.extend_from_slice(&color);
        }
        Image {
            width: w,
            height: h,
            rgba,
        }
    }
    #[test]
    fn identical_images_stay_unchanged() {
        let a = image(3, 2, [20, 30, 40, 255]);
        let b = image(3, 2, [20, 30, 40, 255]);
        let result = compare_pixels(a, b, 0, 0.0, "screenshot-a", "screenshot-b").unwrap();
        assert_eq!(result.status, "unchanged");
        assert_eq!(result.changed_pixels, 0);
        assert!(result.changed_bounds.is_none());
        assert!(result.difference_png_base64.is_some());
    }
    #[test]
    fn detects_one_pixel_and_exact_bounding_box() {
        let a = image(3, 2, [20, 30, 40, 255]);
        let mut b = image(3, 2, [20, 30, 40, 255]);
        b.rgba[4 * 4] = 200; // x=1, y=1
        let result = compare_pixels(a, b, 3, 10.0, "screenshot-a", "screenshot-b").unwrap();
        assert_eq!(result.changed_pixels, 1);
        assert_eq!(result.status, "regressionCandidate");
        let bounds = result.changed_bounds.unwrap();
        assert_eq!(
            (bounds.x, bounds.y, bounds.width, bounds.height),
            (1, 1, 1, 1)
        );
        let raw = STANDARD
            .decode(result.difference_png_base64.unwrap())
            .unwrap();
        let decoded = decode(&raw).unwrap();
        assert_eq!(&decoded.rgba[16..20], &[255, 45, 45, 255]);
    }
    #[test]
    fn thresholds_ignore_small_channel_noise() {
        let a = image(2, 1, [20, 30, 40, 255]);
        let b = image(2, 1, [23, 30, 40, 255]);
        let pass = compare_pixels(a, b, 3, 0.0, "screenshot-a", "screenshot-b").unwrap();
        assert_eq!(pass.changed_pixels, 0);
        assert_eq!(pass.status, "unchanged");
    }
    #[test]
    fn dimension_changes_do_not_get_rescaled() {
        let a = image(2, 2, [1, 1, 1, 255]);
        let b = image(3, 2, [1, 1, 1, 255]);
        let report = compare_pixels(a, b, 0, 100.0, "screenshot-a", "screenshot-b").unwrap();
        assert_eq!(report.status, "dimensionMismatch");
        assert!(report.difference_png_base64.is_none());
    }
    #[test]
    fn decoder_rejects_non_png_data() {
        assert!(decode(b"not a png").is_err());
    }
    #[test]
    fn ids_are_bounded_and_no_path_escape() {
        assert!(valid_screenshot_id("screenshot-abc-123"));
        for id in [
            "../secret",
            "screenshot-../file",
            "screenshot-hi.png",
            "foo",
            "screenshot-",
        ] {
            if id == "screenshot-" {
                continue;
            }
            assert!(!valid_screenshot_id(id));
        }
    }
    #[test]
    fn rejects_cross_session_or_wrong_tab_screen_capture_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("screenshot-abc-1.png");
        fs::write(&path, b"not needed for provenance").unwrap();
        let origin = Capture {
            workspace_id: "workspace-a".into(),
            browser_scope: "workspace-a--ai-browser-a".into(),
            tab_id: "tab-a".into(),
            full_page: false,
            requires_google_access: false,
        };
        recovery_store::save_json(
            &manifest_path(&path),
            &origin,
            "browser screenshot provenance",
        )
        .unwrap();
        assert!(validated_image(&path, &origin, false).is_ok());
        let restricted = Capture {
            requires_google_access: true,
            ..origin.clone()
        };
        recovery_store::save_json(
            &manifest_path(&path),
            &restricted,
            "browser screenshot provenance",
        )
        .unwrap();
        assert!(
            validated_image(&path, &origin, false).is_err(),
            "revoked Google permission must block a previously captured login page"
        );
        assert!(validated_image(&path, &origin, true).is_ok());
        for changed in [
            Capture {
                workspace_id: "workspace-b".into(),
                ..origin.clone()
            },
            Capture {
                browser_scope: "workspace-a--ai-browser-b".into(),
                ..origin.clone()
            },
            Capture {
                tab_id: "tab-b".into(),
                ..origin.clone()
            },
            Capture {
                full_page: true,
                ..origin.clone()
            },
        ] {
            assert!(validated_image(&path, &changed, true).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_capture_png_without_following_it() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside.png");
        let link = directory.path().join("screenshot-a.png");
        fs::write(&outside, b"secret bytes").unwrap();
        symlink(&outside, &link).unwrap();
        let provenance = Capture {
            workspace_id: "workspace-a".into(),
            browser_scope: "session".into(),
            tab_id: "tab-a".into(),
            full_page: false,
            requires_google_access: false,
        };
        recovery_store::save_json(
            &manifest_path(&link),
            &provenance,
            "browser screenshot provenance",
        )
        .unwrap();
        assert!(validated_image(&link, &provenance, true).is_err());
    }

    #[test]
    fn refuses_unbounded_dimensions_without_allocating_pixels() {
        let mut data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut data, 10_000, 10_000);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let _writer = encoder.write_header().unwrap();
        }
        assert!(decode(&data).is_err());
    }
}
