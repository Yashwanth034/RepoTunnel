use sha2::{Digest, Sha256};

#[cfg(windows)]
const MAX_NODES: usize = 800;
#[cfg(windows)]
const MAX_DEPTH: usize = 10;
#[cfg(windows)]
const MAX_CHILDREN_PER_NODE: usize = 200;
#[cfg(windows)]
const MAX_TEXT: usize = 600;
#[cfg(windows)]
const MAX_SEQUENCE_STEPS: usize = 64;
#[cfg(windows)]
const MAX_WAIT_MS: u64 = 2_000;
#[cfg(windows)]
const MAX_TOTAL_WAIT_MS: u64 = 10_000;
#[cfg(windows)]
const MAX_TYPE_BYTES: usize = 32_768;
#[cfg(windows)]
const MAX_SEQUENCE_TEXT_BYTES: usize = 131_072;

// Only UIA Edit controls are text inputs. ValuePattern also appears on
// sliders, spinners, progress and other numeric widgets; do not write them
// merely because they advertise a writable value.
#[cfg(any(windows, test))]
fn windows_semantic_text_edit_allowed(
    control_type_id: i32,
    enabled: bool,
    offscreen: bool,
    sensitive: bool,
    readonly: bool,
) -> bool {
    control_type_id == 50_004 && enabled && !offscreen && !sensitive && !readonly
}

fn role_from_control_type_id(id: i32) -> &'static str {
    match id {
        50_000 => "button",
        50_001 => "calendar",
        50_002 => "check box",
        50_003 => "combo box",
        50_004 => "entry",
        50_005 => "link",
        50_006 => "image",
        50_007 => "list item",
        50_008 => "list",
        50_009 => "menu",
        50_010 => "menu bar",
        50_011 => "menu item",
        50_012 => "progress bar",
        50_013 => "radio button",
        50_014 => "scroll bar",
        50_015 => "slider",
        50_016 => "spin button",
        50_017 => "status bar",
        50_018 => "tab list",
        50_019 => "page tab",
        50_020 => "text",
        50_021 => "tool bar",
        50_022 => "tool tip",
        50_023 => "tree",
        50_024 => "tree item",
        50_025 => "custom",
        50_026 => "group",
        50_027 => "thumb",
        50_028 => "grid",
        50_029 => "grid cell",
        50_030 => "document",
        50_031 => "split button",
        50_032 => "frame",
        50_033 => "pane",
        50_034 => "header",
        50_035 => "header item",
        50_036 => "table",
        50_037 => "title bar",
        50_038 => "separator",
        50_039 => "semantic zoom",
        50_040 => "app bar",
        _ => "unknown",
    }
}

fn element_signature(path: &str, control_type_id: i32, name: &str, automation_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update([0]);
    hasher.update(control_type_id.to_le_bytes());
    hasher.update([0]);
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(automation_id.as_bytes());
    let digest = hasher.finalize();
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn signed_backend_id(
    path: &str,
    root_identity: &str,
    control_type_id: i32,
    name: &str,
    automation_id: &str,
) -> String {
    format!(
        "{path}@{root_identity}#{}",
        element_signature(path, control_type_id, name, automation_id)
    )
}

fn parse_signed_backend_id(value: &str) -> Result<(&str, &str, &str), String> {
    let (scoped_path, signature) = value
        .rsplit_once('#')
        .ok_or_else(|| "Invalid Windows UI Automation element identity.".to_string())?;
    let (path, root_identity) = scoped_path.rsplit_once('@').ok_or_else(|| {
        "Native UI Automation element identity lacks its owning window.".to_string()
    })?;
    if path.is_empty()
        || root_identity.len() != 12
        || !root_identity.bytes().all(|byte| byte.is_ascii_hexdigit())
        || signature.len() != 12
        || !signature.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Invalid Windows UI Automation element identity.".to_string());
    }
    Ok((path, root_identity, signature))
}

// Encode only a verified native HWND's own pixels. We intentionally use
// the existing flate2 dependency instead of importing a screen/desktop
// screenshot library that can expose unrelated windows.
#[cfg(any(windows, test))]
fn encode_native_window_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Write as _;
    if width == 0
        || height == 0
        || (width as u64) * (height as u64) > 8_388_608
        || usize::try_from((width as u64) * (height as u64) * 4).ok() != Some(rgba.len())
    {
        return Err("Native window pixels have invalid or excessive dimensions.".into());
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], value: &[u8]) -> Result<(), String> {
        let len = u32::try_from(value.len())
            .map_err(|_| "Native window PNG section exceeded its byte budget.".to_string())?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(value);
        let mut crc = !0_u32;
        for &byte in kind.iter().chain(value) {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
            }
        }
        out.extend_from_slice(&(!crc).to_be_bytes());
        Ok(())
    }
    let stride = width as usize * 4;
    let mut scanlines = Vec::with_capacity(rgba.len() + height as usize);
    for line in rgba.chunks_exact(stride) {
        scanlines.push(0); // PNG filter None
        scanlines.extend_from_slice(line);
    }
    let mut compressor = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    compressor
        .write_all(&scanlines)
        .map_err(|error| format!("Native window PNG compression failed: {error}"))?;
    let compressed = compressor
        .finish()
        .map_err(|error| format!("Native window PNG output failed: {error}"))?;
    let mut output = Vec::with_capacity(compressed.len() + 80);
    output.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut output, b"IHDR", &ihdr)?;
    chunk(&mut output, b"IDAT", &compressed)?;
    chunk(&mut output, b"IEND", &[])?;
    Ok(output)
}

// DIB sections request top-down BGRA (negative height), so there is no
// display-coordinate crop, channel ambiguity or bottom-up desktop flip.
#[cfg(any(windows, test))]
fn native_window_bgra_to_rgba(
    width: u32,
    height: u32,
    max_width: u32,
    bgra: &[u8],
) -> Result<(u32, u32, Vec<u8>), String> {
    if width == 0
        || height == 0
        || (width as u64) * (height as u64) > 8_388_608
        || usize::try_from((width as u64) * (height as u64) * 4).ok() != Some(bgra.len())
    {
        return Err("Native HWND bitmap dimensions or buffer length are invalid.".into());
    }
    let output_width = width.min(max_width.clamp(1, 4096));
    let output_height =
        (u64::from(height) * u64::from(output_width) / u64::from(width)).max(1) as u32;
    let mut rgba = Vec::with_capacity(output_width as usize * output_height as usize * 4);
    for y in 0..output_height {
        for x in 0..output_width {
            let sx = x as usize * width as usize / output_width as usize;
            let sy = y as usize * height as usize / output_height as usize;
            let i = (sy * width as usize + sx) * 4;
            rgba.extend_from_slice(&[bgra[i + 2], bgra[i + 1], bgra[i], 255]);
        }
    }
    Ok((output_width, output_height, rgba))
}

#[cfg(windows)]
pub(crate) struct NativeWindowFrame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
    pub(crate) png: Vec<u8>,
}

fn parse_application_pid(application_id: &str) -> Result<i32, String> {
    application_id
        .strip_prefix("uia-pid-")
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| "Invalid Windows UI Automation application ID.".to_string())
}

#[cfg(windows)]
mod runtime {
    use std::{
        collections::BTreeMap,
        thread,
        time::{Duration, Instant},
    };

    use serde_json::{json, Value};
    use uiautomation::{
        core::UICacheRequest,
        patterns::{
            UIInvokePattern, UIPatternType, UIScrollPattern, UISelectionItemPattern,
            UITogglePattern, UIValuePattern,
        },
        types::{ScrollAmount, TreeScope, UIProperty},
        UIAutomation, UIElement, UITreeWalker,
    };

    use super::{
        element_signature, parse_application_pid, parse_signed_backend_id,
        role_from_control_type_id, signed_backend_id, windows_semantic_text_edit_allowed,
        MAX_CHILDREN_PER_NODE, MAX_DEPTH, MAX_NODES, MAX_SEQUENCE_STEPS, MAX_SEQUENCE_TEXT_BYTES,
        MAX_TEXT, MAX_TOTAL_WAIT_MS, MAX_TYPE_BYTES, MAX_WAIT_MS,
    };

    #[derive(Clone, Debug)]
    pub(crate) struct WindowsUiaApplication {
        pub(crate) id: String,
        pub(crate) name: String,
        pub(crate) running: bool,
        pub(crate) accessibility: bool,
        pub(crate) window_count: usize,
    }

    fn uia_error(context: &str, error: impl std::fmt::Display) -> String {
        format!("{context}: {error}")
    }

    fn create_snapshot_cache(automation: &UIAutomation) -> Result<UICacheRequest, String> {
        let cache = automation
            .create_cache_request()
            .map_err(|error| uia_error("Could not create Windows UIA cache request", error))?;
        for property in [
            UIProperty::Name,
            UIProperty::AutomationId,
            UIProperty::ClassName,
            UIProperty::HelpText,
            UIProperty::ProcessId,
            UIProperty::ControlType,
            UIProperty::BoundingRectangle,
            UIProperty::IsEnabled,
            UIProperty::IsOffscreen,
            UIProperty::IsKeyboardFocusable,
            UIProperty::HasKeyboardFocus,
            UIProperty::IsPassword,
        ] {
            cache.add_property(property).map_err(|error| {
                uia_error("Could not configure Windows UIA property cache", error)
            })?;
        }
        for pattern in [
            UIPatternType::Invoke,
            UIPatternType::Value,
            UIPatternType::Toggle,
            UIPatternType::SelectionItem,
            UIPatternType::Scroll,
        ] {
            cache.add_pattern(pattern).map_err(|error| {
                uia_error("Could not configure Windows UIA pattern cache", error)
            })?;
        }
        cache
            .set_tree_scope(TreeScope::Element)
            .map_err(|error| uia_error("Could not scope Windows UIA cache request", error))?;
        cache
            .set_tree_filter(
                automation
                    .get_control_view_condition()
                    .map_err(|error| uia_error("Could not get Windows UIA Control View", error))?,
            )
            .map_err(|error| {
                uia_error("Could not set Windows UIA Control View cache filter", error)
            })?;
        Ok(cache)
    }

    fn automation() -> Result<(UIAutomation, UITreeWalker, UICacheRequest), String> {
        let automation = UIAutomation::new()
            .map_err(|error| uia_error("Could not initialize Windows UI Automation", error))?;
        let walker = automation.get_control_view_walker().map_err(|error| {
            uia_error("Could not create Windows UIA Control View walker", error)
        })?;
        let cache = create_snapshot_cache(&automation)?;
        Ok((automation, walker, cache))
    }

    fn cached_control_type_id(element: &UIElement) -> i32 {
        element
            .get_cached_control_type()
            .map(|value| value as i32)
            .unwrap_or_default()
    }

    fn current_control_type_id(element: &UIElement) -> i32 {
        element
            .get_control_type()
            .map(|value| value as i32)
            .unwrap_or_default()
    }

    fn clean(value: String) -> String {
        value
            .replace('\0', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn bounded_text(value: String) -> String {
        value.chars().take(MAX_TEXT).collect::<String>()
    }

    fn cached_name(element: &UIElement) -> String {
        element.get_cached_name().map(clean).unwrap_or_default()
    }

    fn cached_automation_id(element: &UIElement) -> String {
        element
            .get_cached_automation_id()
            .map(clean)
            .unwrap_or_default()
    }

    fn current_signature(path: &str, element: &UIElement) -> String {
        element_signature(
            path,
            current_control_type_id(element),
            &element.get_name().map(clean).unwrap_or_default(),
            &element.get_automation_id().map(clean).unwrap_or_default(),
        )
    }

    fn cached_bounds(element: &UIElement) -> Option<Value> {
        let rect = element.get_cached_bounding_rectangle().ok()?;
        let width = rect.get_width();
        let height = rect.get_height();
        if width <= 0 || height <= 0 {
            return None;
        }
        Some(json!({
            "x": rect.get_left(),
            "y": rect.get_top(),
            "width": width,
            "height": height,
        }))
    }

    fn cached_states_actions_text(
        element: &UIElement,
        sensitive: bool,
    ) -> (Vec<String>, Vec<String>, String) {
        let enabled = element.is_cached_enabled().unwrap_or(false);
        let offscreen = element.is_cached_offscreen().unwrap_or(true);
        let focusable = element.is_cached_keyboard_focusable().unwrap_or(false);
        let focused = element.has_cached_keyboard_focus().unwrap_or(false);

        let mut states = vec![
            if enabled { "enabled" } else { "disabled" }.to_string(),
            if offscreen { "offscreen" } else { "visible" }.to_string(),
        ];
        if focusable {
            states.push("focusable".to_string());
        }
        if focused {
            states.push("focused".to_string());
        }

        let mut actions = Vec::<String>::new();
        let has_click = element.get_cached_pattern::<UIInvokePattern>().is_ok()
            || element.get_cached_pattern::<UITogglePattern>().is_ok()
            || element
                .get_cached_pattern::<UISelectionItemPattern>()
                .is_ok();
        if has_click && enabled && !offscreen && !sensitive {
            actions.push("click".to_string());
        }
        if enabled
            && !offscreen
            && !sensitive
            && element.get_cached_pattern::<UIScrollPattern>().is_ok()
        {
            actions.push("scroll".to_string());
        }

        let mut text = String::new();
        if let Ok(value) = element.get_cached_pattern::<UIValuePattern>() {
            let readonly = value.cached_is_readonly().unwrap_or(true);
            if readonly {
                states.push("read-only".to_string());
            } else if windows_semantic_text_edit_allowed(
                cached_control_type_id(element),
                enabled,
                offscreen,
                sensitive,
                readonly,
            ) {
                states.push("editable".to_string());
                actions.push("type".to_string());
            }
            if !sensitive {
                text = value
                    .get_cached_value()
                    .map(bounded_text)
                    .unwrap_or_default();
            }
        }

        (states, actions, text)
    }

    fn cached_element_json(path: &str, root_identity: &str, element: &UIElement) -> Value {
        let control_type_id = cached_control_type_id(element);
        let name = cached_name(element);
        let automation_id = cached_automation_id(element);
        // A provider failure cannot establish that a control is non-secret.
        let sensitive = element.is_cached_password().unwrap_or(true);
        let (states, actions, text) = cached_states_actions_text(element, sensitive);
        json!({
            "id": signed_backend_id(path, root_identity, control_type_id, &name, &automation_id),
            "role": role_from_control_type_id(control_type_id),
            "name": name,
            "description": element.get_cached_help_text().map(clean).unwrap_or_default(),
            "text": if sensitive { String::new() } else { text },
            "states": states,
            "actions": actions,
            "bounds": cached_bounds(element),
            "sensitive": sensitive,
        })
    }

    fn app_windows(
        automation: &UIAutomation,
        walker: &UITreeWalker,
        cache: &UICacheRequest,
        pid: i32,
    ) -> Result<Vec<UIElement>, String> {
        if pid == std::process::id() as i32 {
            return Err(
                "RepoTunnel cannot grant desktop control over its own process.".to_string(),
            );
        }
        let root = automation
            .get_root_element()
            .map_err(|error| uia_error("Could not read Windows UIA root", error))?;
        let children = walker
            .get_children_build_cache(&root, cache)
            .ok_or_else(|| "Could not inspect Windows app-owned top-level windows.".to_string())?;
        Ok(children
            .into_iter()
            .filter(|element| element.get_cached_process_id().ok() == Some(pid))
            .collect())
    }

    pub(crate) fn list() -> Result<Vec<WindowsUiaApplication>, String> {
        let (automation, walker, cache) = automation()?;
        let root = automation
            .get_root_element()
            .map_err(|error| uia_error("Could not read Windows UIA root", error))?;
        let children = walker
            .get_children_build_cache(&root, &cache)
            .unwrap_or_default();
        let own_pid = std::process::id() as i32;
        let mut grouped = BTreeMap::<i32, WindowsUiaApplication>::new();

        for window in children {
            let pid = window.get_cached_process_id().unwrap_or_default();
            if pid <= 0 || pid == own_pid {
                continue;
            }
            let offscreen = window.is_cached_offscreen().unwrap_or(true);
            if offscreen {
                continue;
            }
            let title = cached_name(&window);
            let entry = grouped.entry(pid).or_insert_with(|| WindowsUiaApplication {
                id: format!("uia-pid-{pid}"),
                name: if title.is_empty() {
                    format!("Windows application {pid}")
                } else {
                    title.clone()
                },
                running: true,
                accessibility: true,
                window_count: 0,
            });
            entry.window_count = entry.window_count.saturating_add(1);
            if entry.name.starts_with("Windows application ") && !title.is_empty() {
                entry.name = title;
            }
        }

        Ok(grouped.into_values().collect())
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_cached(
        walker: &UITreeWalker,
        cache: &UICacheRequest,
        element: &UIElement,
        path: &str,
        root_identity: &str,
        depth: usize,
        wanted: usize,
        elements: &mut Vec<Value>,
        truncated: &mut bool,
    ) {
        if elements.len() >= wanted {
            *truncated = true;
            return;
        }
        elements.push(cached_element_json(path, root_identity, element));
        if depth >= MAX_DEPTH {
            // This level can still have uninspected descendants; do not
            // present depth-limited trees as complete for coordinate input.
            *truncated = true;
            return;
        }
        let children = match walker.get_children_build_cache(element, cache) {
            Some(children) => children,
            None => {
                // A failed child enumeration does not mean no children.
                // Mark the snapshot incomplete so coordinate hit-testing
                // cannot click through unobserved sensitive controls.
                *truncated = true;
                return;
            }
        };
        if children.len() > MAX_CHILDREN_PER_NODE {
            *truncated = true;
        }
        for (index, child) in children.into_iter().take(MAX_CHILDREN_PER_NODE).enumerate() {
            if elements.len() >= wanted {
                *truncated = true;
                return;
            }
            walk_cached(
                walker,
                cache,
                &child,
                &format!("{path}.{index}"),
                root_identity,
                depth + 1,
                wanted,
                elements,
                truncated,
            );
        }
    }

    // Win32 PrintWindow draws a single HWND into an off-screen DIB. Never use
    // BitBlt from DISPLAY, capture_rect, or a desktop-region screenshot: those
    // APIs could include other users' windows even when the rectangle matches
    // the verified app's accessible bounds.
    pub(crate) fn capture_window_png(
        application_id: &str,
        selected_window_id: &str,
        max_width: u32,
    ) -> Result<super::NativeWindowFrame, String> {
        use std::{ffi::c_void, ptr};
        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct Rect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        #[repr(C)]
        struct BitmapInfoHeader {
            size: u32,
            width: i32,
            height: i32,
            planes: u16,
            bit_count: u16,
            compression: u32,
            image_size: u32,
            x_pixels_per_meter: i32,
            y_pixels_per_meter: i32,
            colors_used: u32,
            colors_important: u32,
        }
        #[repr(C)]
        struct BitmapInfo {
            header: BitmapInfoHeader,
            colors: [u32; 1],
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
            fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
            fn IsWindow(hwnd: *mut c_void) -> i32;
            fn IsWindowVisible(hwnd: *mut c_void) -> i32;
            fn IsIconic(hwnd: *mut c_void) -> i32;
            fn PrintWindow(hwnd: *mut c_void, dc: *mut c_void, flags: u32) -> i32;
        }
        #[link(name = "gdi32")]
        unsafe extern "system" {
            fn CreateCompatibleDC(dc: *mut c_void) -> *mut c_void;
            fn CreateDIBSection(
                dc: *mut c_void,
                info: *const BitmapInfo,
                usage: u32,
                bits: *mut *mut c_void,
                section: *mut c_void,
                offset: u32,
            ) -> *mut c_void;
            fn SelectObject(dc: *mut c_void, object: *mut c_void) -> *mut c_void;
            fn DeleteObject(object: *mut c_void) -> i32;
            fn DeleteDC(dc: *mut c_void) -> i32;
        }
        struct OwnedDib {
            dc: *mut c_void,
            bitmap: *mut c_void,
            previous: *mut c_void,
        }
        impl Drop for OwnedDib {
            fn drop(&mut self) {
                unsafe {
                    if !self.previous.is_null() && self.previous as isize != -1 {
                        SelectObject(self.dc, self.previous);
                    }
                    DeleteObject(self.bitmap);
                    DeleteDC(self.dc);
                }
            }
        }

        let pid = parse_application_pid(application_id)? as u32;
        let (automation, walker, cache) = automation()?;
        let windows = app_windows(&automation, &walker, &cache, pid as i32)?;
        let signed_ids = windows
            .iter()
            .enumerate()
            .map(|(index, window)| {
                format!(
                    "uia-window-{}",
                    element_signature(
                        &format!("w{index}"),
                        cached_control_type_id(window),
                        &cached_name(window),
                        &cached_automation_id(window),
                    )
                )
            })
            .collect::<Vec<_>>();
        let intrinsic = windows
            .iter()
            .map(|window| {
                element_signature(
                    "top-level",
                    cached_control_type_id(window),
                    &cached_name(window),
                    &cached_automation_id(window),
                )
            })
            .collect::<Vec<_>>();
        let selected = crate::desktop_control::select_native_unambiguous_window(
            &signed_ids,
            &intrinsic,
            Some(selected_window_id),
        )?;
        let window = &windows[selected];
        let window_pid = window.get_process_id().map_err(|error| {
            format!("Native Windows capture lost the verified application PID: {error}")
        })?;
        if window_pid != pid {
            return Err("Native Windows capture target escaped its verified process.".into());
        }
        let handle: isize = window
            .get_native_window_handle()
            .map_err(|error| format!("Native Windows capture cannot identify HWND: {error}"))?
            .into();
        let hwnd = ptr::with_exposed_provenance_mut::<c_void>(handle as usize);
        let mut kernel_pid = 0_u32;
        if hwnd.is_null()
            || unsafe { IsWindow(hwnd) } == 0
            || unsafe { IsWindowVisible(hwnd) } == 0
            || unsafe { IsIconic(hwnd) } != 0
            || unsafe { GetWindowThreadProcessId(hwnd, &mut kernel_pid) } == 0
            || kernel_pid != pid
        {
            return Err(
                "Native Windows capture HWND is hidden, invalid or belongs to a different process."
                    .into(),
            );
        }
        let mut rect = Rect::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
            return Err(
                "Native Windows capture could not verify the selected HWND geometry.".into(),
            );
        }
        let width = rect
            .right
            .checked_sub(rect.left)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| "Native Windows capture geometry is invalid.".to_string())?;
        let height = rect
            .bottom
            .checked_sub(rect.top)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| "Native Windows capture geometry is invalid.".to_string())?;
        if width == 0
            || height == 0
            || width > 4096
            || height > 4096
            || u64::from(width) * u64::from(height) > 8_388_608
        {
            return Err("Native Windows capture exceeds the safe per-window pixel limit.".into());
        }
        let info = BitmapInfo {
            header: BitmapInfoHeader {
                size: std::mem::size_of::<BitmapInfoHeader>() as u32,
                width: width as i32,
                height: -(height as i32),
                planes: 1,
                bit_count: 32,
                compression: 0,
                image_size: 0,
                x_pixels_per_meter: 0,
                y_pixels_per_meter: 0,
                colors_used: 0,
                colors_important: 0,
            },
            colors: [0],
        };
        let dc = unsafe { CreateCompatibleDC(ptr::null_mut()) };
        if dc.is_null() {
            return Err("Native Windows capture could not allocate an isolated memory DC.".into());
        }
        let mut bits = ptr::null_mut();
        let bitmap = unsafe { CreateDIBSection(dc, &info, 0, &mut bits, ptr::null_mut(), 0) };
        if bitmap.is_null() || bits.is_null() {
            unsafe {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                DeleteDC(dc);
            }
            return Err(
                "Native Windows capture could not allocate its off-screen window bitmap.".into(),
            );
        }
        let previous = unsafe { SelectObject(dc, bitmap) };
        if previous.is_null() || previous as isize == -1 {
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(dc);
            }
            return Err("Native Windows capture cannot attach its private bitmap.".into());
        }
        let image = OwnedDib {
            dc,
            bitmap,
            previous,
        };
        if unsafe { PrintWindow(hwnd, image.dc, 2) } == 0 {
            return Err("Native Windows target rejected window-only PrintWindow capture; desktop fallback is prohibited.".into());
        }
        let mut after_pid = 0_u32;
        let mut after_rect = Rect::default();
        if unsafe { IsWindow(hwnd) } == 0
            || unsafe { GetWindowThreadProcessId(hwnd, &mut after_pid) } == 0
            || after_pid != pid
            || unsafe { GetWindowRect(hwnd, &mut after_rect) } == 0
            || after_rect.left != rect.left
            || after_rect.top != rect.top
            || after_rect.right != rect.right
            || after_rect.bottom != rect.bottom
            || window.get_process_id().ok() != Some(pid)
        {
            return Err("Native Windows HWND owner or geometry changed during capture.".into());
        }
        let length = (width as usize) * (height as usize) * 4;
        let bgra = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), length) };
        let (target_width, target_height, rgba) =
            super::native_window_bgra_to_rgba(width, height, max_width, bgra)?;
        Ok(super::NativeWindowFrame {
            width: target_width,
            height: target_height,
            source_width: width,
            source_height: height,
            png: super::encode_native_window_png(target_width, target_height, &rgba)?,
        })
    }

    pub(crate) fn owned_window_count(application_id: &str) -> Result<usize, String> {
        let pid = parse_application_pid(application_id)?;
        let (automation, walker, cache) = automation()?;
        Ok(app_windows(&automation, &walker, &cache, pid)?.len())
    }

    pub(crate) fn inspect(application_id: &str, limit: usize) -> Result<Value, String> {
        let pid = parse_application_pid(application_id)?;
        let (automation, walker, cache) = automation()?;
        let windows = app_windows(&automation, &walker, &cache, pid)?;
        if windows.is_empty() {
            return Err(
                "That Windows UI Automation application is not currently running.".to_string(),
            );
        }

        let wanted = limit.clamp(20, MAX_NODES);
        let mut window_values = Vec::new();
        let mut elements = Vec::new();
        let mut truncated = false;
        let mut app_name = String::new();

        for (index, window) in windows.iter().enumerate() {
            if app_name.is_empty() {
                app_name = cached_name(window);
            }
            let path = format!("w{index}");
            let control_type_id = cached_control_type_id(window);
            let name = cached_name(window);
            let automation_id = cached_automation_id(window);
            window_values.push(json!({
                "windowId": format!(
                    "uia-window-{}",
                    element_signature(&path, control_type_id, &name, &automation_id)
                ),
                "title": name,
                "bounds": cached_bounds(window),
            }));
            if elements.len() < wanted {
                let root_identity =
                    element_signature("top-level", control_type_id, &name, &automation_id);
                walk_cached(
                    &walker,
                    &cache,
                    window,
                    &path,
                    &root_identity,
                    0,
                    wanted,
                    &mut elements,
                    &mut truncated,
                );
            } else {
                truncated = true;
            }
        }

        if app_name.is_empty() {
            app_name = format!("Windows application {pid}");
        }

        Ok(json!({
            "applicationId": application_id,
            "name": app_name,
            "semanticAvailable": true,
            "windows": window_values,
            "elements": elements,
            "truncated": truncated,
            "message": Value::Null,
        }))
    }

    fn resolve_element_with(
        automation: &UIAutomation,
        walker: &UITreeWalker,
        pid: i32,
        backend_id: &str,
    ) -> Result<UIElement, String> {
        if pid == std::process::id() as i32 {
            return Err(
                "RepoTunnel cannot grant desktop control over its own process.".to_string(),
            );
        }
        let (path, expected_root_identity, expected_signature) =
            parse_signed_backend_id(backend_id)?;
        let mut parts = path.split('.');
        let window_index = parts
            .next()
            .and_then(|part| part.strip_prefix('w'))
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| "Invalid Windows UI Automation element path.".to_string())?;

        let root = automation
            .get_root_element()
            .map_err(|error| uia_error("Could not read Windows UIA root", error))?;
        let windows = walker
            .get_children(&root)
            .unwrap_or_default()
            .into_iter()
            .filter(|element| element.get_process_id().ok() == Some(pid as u32))
            .collect::<Vec<_>>();
        let identities = windows
            .iter()
            .map(|window| {
                element_signature(
                    "top-level",
                    current_control_type_id(window),
                    &window.get_name().map(clean).unwrap_or_default(),
                    &window.get_automation_id().map(clean).unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();
        crate::desktop_control::validate_native_semantic_window_owner(
            &identities,
            window_index,
            expected_root_identity,
        )?;
        let mut element = windows.get(window_index).cloned().ok_or_else(|| {
            "That Windows UI Automation window is no longer present. Inspect it again.".to_string()
        })?;

        for part in parts {
            let index = part
                .parse::<usize>()
                .map_err(|_| "Invalid Windows UI Automation element path.".to_string())?;
            let children = walker.get_children(&element).unwrap_or_default();
            element = children.get(index).cloned().ok_or_else(|| {
                "That Windows UI Automation element is no longer present. Inspect it again."
                    .to_string()
            })?;
        }

        if current_signature(path, &element) != expected_signature {
            return Err(
                "That Windows UI Automation element changed since inspection. Inspect it again."
                    .to_string(),
            );
        }
        if element.get_process_id().ok() != Some(pid as u32) {
            return Err("Windows UI Automation element escaped the permitted process.".to_string());
        }
        Ok(element)
    }

    fn perform_click(element: &UIElement) -> Result<String, String> {
        if element.is_password().unwrap_or(true) {
            return Err(
                "Windows UIA click is blocked for sensitive or unverifiable controls.".into(),
            );
        }
        if !element.is_enabled().unwrap_or(false) {
            return Err("That Windows UI Automation element is disabled.".to_string());
        }
        if element.is_offscreen().unwrap_or(true) {
            return Err("That Windows UI Automation element is off-screen.".to_string());
        }
        if let Ok(pattern) = element.get_pattern::<UIInvokePattern>() {
            pattern
                .invoke()
                .map_err(|error| uia_error("Windows UIA Invoke failed", error))?;
            return Ok("Invoked the Windows UI Automation element.".to_string());
        }
        if let Ok(pattern) = element.get_pattern::<UITogglePattern>() {
            pattern
                .toggle()
                .map_err(|error| uia_error("Windows UIA Toggle failed", error))?;
            return Ok("Toggled the Windows UI Automation element.".to_string());
        }
        if let Ok(pattern) = element.get_pattern::<UISelectionItemPattern>() {
            pattern
                .select()
                .map_err(|error| uia_error("Windows UIA SelectionItem failed", error))?;
            return Ok("Selected the Windows UI Automation element.".to_string());
        }
        Err(
            "That Windows UI Automation element exposes no supported semantic click pattern."
                .to_string(),
        )
    }

    // Only UI Automation scrolls the exact signed, app-owned element.
    // Restrict every request to one discrete notch in each direction.
    pub(crate) fn scroll(
        application_id: &str,
        element_id: &str,
        delta_x: i32,
        delta_y: i32,
    ) -> Result<Value, String> {
        if (delta_x == 0 && delta_y == 0)
            || ![-120, 0, 120].contains(&delta_x)
            || ![-120, 0, 120].contains(&delta_y)
            || (delta_x != 0 && delta_y != 0)
        {
            return Err(
                "Windows semantic scroll requires one bounded 120-unit notch per axis.".into(),
            );
        }
        let pid = parse_application_pid(application_id)?;
        let automation = UIAutomation::new()
            .map_err(|error| uia_error("Could not initialize Windows UI Automation", error))?;
        let walker = automation.get_control_view_walker().map_err(|error| {
            uia_error("Could not create Windows UIA Control View walker", error)
        })?;
        let element = resolve_element_with(&automation, &walker, pid, element_id)?;
        if !element.is_enabled().unwrap_or(false)
            || element.is_offscreen().unwrap_or(true)
            || element.is_password().unwrap_or(true)
        {
            return Err("Windows semantic scroll target is disabled, hidden or sensitive.".into());
        }
        let pattern = element
            .get_pattern::<UIScrollPattern>()
            .map_err(|_| "The signed Windows UIA target has no Scroll pattern.".to_string())?;
        let scroll_amount = |delta: i32| match delta {
            -120 => ScrollAmount::SmallDecrement,
            120 => ScrollAmount::SmallIncrement,
            _ => ScrollAmount::NoAmount,
        };
        pattern
            .scroll(scroll_amount(delta_x), scroll_amount(delta_y))
            .map_err(|error| uia_error("Windows UIA scoped Scroll failed", error))?;
        Ok(json!({
            "applicationId": application_id,
            "action": "scroll",
            "deltaX": delta_x,
            "deltaY": delta_y,
            "nativeScoped": true
        }))
    }

    fn perform_type(element: &UIElement, text: &str, clear_first: bool) -> Result<String, String> {
        if text.len() > MAX_TYPE_BYTES || text.as_bytes().contains(&0) {
            return Err(format!(
                "Windows semantic typing may contain at most {MAX_TYPE_BYTES} bytes and no NUL bytes."
            ));
        }
        if element.is_password().unwrap_or(true) {
            return Err(
                "RepoTunnel refuses to type into a Windows password or unverified-sensitive field."
                    .to_string(),
            );
        }
        let control_type_id = element
            .get_control_type()
            .map(|value| value as i32)
            .map_err(|error| {
                uia_error("Could not verify the Windows UIA text control role", error)
            })?;
        let enabled = element.is_enabled().unwrap_or(false);
        let offscreen = element.is_offscreen().unwrap_or(true);
        let pattern = element.get_pattern::<UIValuePattern>().map_err(|_| {
            "That Windows UI Automation element does not expose the Value pattern.".to_string()
        })?;
        let readonly = pattern.is_readonly().unwrap_or(true);
        if !windows_semantic_text_edit_allowed(control_type_id, enabled, offscreen, false, readonly)
        {
            return Err(
                "Windows semantic typing requires a visible, enabled, writable Edit control."
                    .into(),
            );
        }
        let value = if clear_first {
            text.to_string()
        } else {
            let mut current = pattern.get_value().map_err(|error| {
                uia_error("Could not read existing Windows UIA text value", error)
            })?;
            current.push_str(text);
            current
        };
        if value.len() > MAX_TYPE_BYTES {
            return Err("Windows combined native text value exceeds its 32,768-byte limit.".into());
        }
        pattern
            .set_value(&value)
            .map_err(|error| uia_error("Windows UIA Value.SetValue failed", error))?;
        Ok(format!(
            "Entered {} bytes through the Windows UI Automation Value pattern.",
            text.len()
        ))
    }

    fn perform_semantic_action_with(
        automation: &UIAutomation,
        walker: &UITreeWalker,
        application_id: &str,
        action: &str,
        element_id: &str,
        text: Option<&str>,
        clear_first: bool,
    ) -> Result<Value, String> {
        let pid = parse_application_pid(application_id)?;
        let element = resolve_element_with(automation, walker, pid, element_id)?;
        let detail = match action {
            "click" => perform_click(&element)?,
            "type" => perform_type(
                &element,
                text.ok_or_else(|| "Windows semantic type requires text.".to_string())?,
                clear_first,
            )?,
            _ => return Err("Windows semantic action must be click or type.".to_string()),
        };
        Ok(json!({
            "applicationId": application_id,
            "action": action,
            "detail": detail,
        }))
    }

    pub(crate) fn action(
        application_id: &str,
        action: &str,
        element_id: Option<&str>,
        text: Option<&str>,
        clear_first: bool,
    ) -> Result<Value, String> {
        let pid = parse_application_pid(application_id)?;
        let automation = UIAutomation::new()
            .map_err(|error| uia_error("Could not initialize Windows UI Automation", error))?;
        let walker = automation.get_control_view_walker().map_err(|error| {
            uia_error("Could not create Windows UIA Control View walker", error)
        })?;

        match action {
            "activate" => {
                let cache = create_snapshot_cache(&automation)?;
                let windows = app_windows(&automation, &walker, &cache, pid)?;
                let signed_ids = windows
                    .iter()
                    .enumerate()
                    .map(|(index, window)| {
                        format!(
                            "uia-window-{}",
                            element_signature(
                                &format!("w{index}"),
                                cached_control_type_id(window),
                                &cached_name(window),
                                &cached_automation_id(window),
                            )
                        )
                    })
                    .collect::<Vec<_>>();
                let intrinsic = windows
                    .iter()
                    .map(|window| {
                        element_signature(
                            "top-level",
                            cached_control_type_id(window),
                            &cached_name(window),
                            &cached_automation_id(window),
                        )
                    })
                    .collect::<Vec<_>>();
                let index = crate::desktop_control::select_native_unambiguous_window(
                    &signed_ids,
                    &intrinsic,
                    element_id,
                )?;
                let window = &windows[index];
                window
                    .set_focus()
                    .map_err(|error| uia_error("Windows UIA focus failed", error))?;
                Ok(json!({
                    "applicationId": application_id,
                    "action": "activate",
                    "detail": "Focused the permitted Windows UI Automation application.",
                }))
            }
            "click" | "type" => perform_semantic_action_with(
                &automation,
                &walker,
                application_id,
                action,
                element_id.ok_or_else(|| {
                    "Windows click/type requires an inspected semantic element ID.".to_string()
                })?,
                text,
                clear_first,
            ),
            "key" | "scroll" => Err(
                "Windows Stage 10 uses semantic UI Automation only for key/scroll; use a semantic click/type ref now. Native keyboard/scroll fallback is intentionally not enabled in this adapter."
                    .to_string(),
            ),
            _ => Err("Unsupported Windows desktop action.".to_string()),
        }
    }

    pub(crate) fn semantic_sequence(
        application_id: &str,
        steps: &[Value],
    ) -> Result<Value, String> {
        semantic_sequence_with_guard(application_id, steps, || Ok(()))
    }

    pub(crate) fn semantic_sequence_with_guard(
        application_id: &str,
        steps: &[Value],
        mut before_step: impl FnMut() -> Result<(), String>,
    ) -> Result<Value, String> {
        if steps.is_empty() || steps.len() > MAX_SEQUENCE_STEPS {
            return Err(format!(
                "Windows semantic sequence requires 1..{MAX_SEQUENCE_STEPS} steps."
            ));
        }
        let automation = UIAutomation::new()
            .map_err(|error| uia_error("Could not initialize Windows UI Automation", error))?;
        let walker = automation.get_control_view_walker().map_err(|error| {
            uia_error("Could not create Windows UIA Control View walker", error)
        })?;
        let started = Instant::now();
        let mut total_wait_ms = 0_u64;
        let mut total_text_bytes = 0_usize;
        let mut completed = 0_usize;
        let mut results = Vec::new();

        for (index, step) in steps.iter().enumerate() {
            before_step().map_err(|error| format!("SEQUENCE_STEP_{}: {error}", index + 1))?;
            if started.elapsed() > Duration::from_secs(20) {
                return Err(format!(
                    "SEQUENCE_TIMEOUT: Windows semantic sequence exceeded 20 seconds before step {}.",
                    index + 1
                ));
            }
            let operation = step
                .get("operation")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let result = match operation {
                "wait" => {
                    let wait_ms = step
                        .get("waitMs")
                        .and_then(Value::as_u64)
                        .unwrap_or_default();
                    if wait_ms > MAX_WAIT_MS {
                        return Err(format!(
                            "SEQUENCE_STEP_{}: wait exceeds {MAX_WAIT_MS} ms.",
                            index + 1
                        ));
                    }
                    total_wait_ms = total_wait_ms.saturating_add(wait_ms);
                    if total_wait_ms > MAX_TOTAL_WAIT_MS {
                        return Err(format!(
                            "SEQUENCE_STEP_{}: total wait exceeds {MAX_TOTAL_WAIT_MS} ms.",
                            index + 1
                        ));
                    }
                    if wait_ms > 0 {
                        thread::sleep(Duration::from_millis(wait_ms));
                    }
                    json!({"waitedMs": wait_ms})
                }
                "click" => perform_semantic_action_with(
                    &automation,
                    &walker,
                    application_id,
                    "click",
                    step.get("elementId")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            format!(
                                "SEQUENCE_STEP_{}: semantic click requires an element ID.",
                                index + 1
                            )
                        })?,
                    None,
                    false,
                )
                .map_err(|error| format!("SEQUENCE_STEP_{}: {error}", index + 1))?,
                "type" => {
                    let text = step.get("text").and_then(Value::as_str).unwrap_or_default();
                    total_text_bytes = total_text_bytes.saturating_add(text.len());
                    if total_text_bytes > MAX_SEQUENCE_TEXT_BYTES {
                        return Err(format!(
                            "SEQUENCE_STEP_{}: total typed text exceeds {MAX_SEQUENCE_TEXT_BYTES} bytes.",
                            index + 1
                        ));
                    }
                    perform_semantic_action_with(
                        &automation,
                        &walker,
                        application_id,
                        "type",
                        step.get("elementId")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                format!(
                                    "SEQUENCE_STEP_{}: semantic type requires an element ID.",
                                    index + 1
                                )
                            })?,
                        Some(text),
                        step.get("clearFirst")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                    .map_err(|error| format!("SEQUENCE_STEP_{}: {error}", index + 1))?
                }
                _ => {
                    return Err(format!(
                        "SEQUENCE_STEP_{}: unsupported Windows semantic sequence operation.",
                        index + 1
                    ))
                }
            };
            completed += 1;
            results.push(json!({
                "index": index,
                "operation": operation,
                "result": result,
            }));
        }

        Ok(json!({
            "applicationId": application_id,
            "stepCount": steps.len(),
            "completedSteps": completed,
            "elapsedMs": started.elapsed().as_millis(),
            "results": results,
        }))
    }
}

#[cfg(windows)]
pub(crate) use runtime::{
    action, capture_window_png, inspect, list, owned_window_count, scroll, semantic_sequence,
    semantic_sequence_with_guard,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_windows_control_types_to_shared_roles() {
        assert_eq!(role_from_control_type_id(50_000), "button");
        assert_eq!(role_from_control_type_id(50_002), "check box");
        assert_eq!(role_from_control_type_id(50_004), "entry");
        assert_eq!(role_from_control_type_id(50_019), "page tab");
        assert_eq!(role_from_control_type_id(50_024), "tree item");
        assert_eq!(role_from_control_type_id(50_032), "frame");
        assert_eq!(role_from_control_type_id(123), "unknown");
    }

    #[test]
    fn backend_ids_are_signed_and_reject_malformed_values() {
        let owner = element_signature("top-level", 50_032, "Editor One", "window-one");
        let id = signed_backend_id("w0.2.1", &owner, 50_000, "Save", "save-button");
        let (path, root_identity, signature) =
            parse_signed_backend_id(&id).expect("signed backend id");
        assert_eq!(path, "w0.2.1");
        assert_eq!(root_identity, owner);
        assert_eq!(
            signature,
            element_signature("w0.2.1", 50_000, "Save", "save-button")
        );
        let other_owner = element_signature("top-level", 50_032, "Editor Two", "window-two");
        assert_ne!(
            owner, other_owner,
            "same Save button in another window is not its owner"
        );
        assert_ne!(
            signed_backend_id("w0.2.1", &owner, 50_000, "Save", "save-button"),
            signed_backend_id("w0.2.1", &other_owner, 50_000, "Save", "save-button"),
            "semantic element IDs must change when their root window changes"
        );
        assert!(parse_signed_backend_id("w0.2.1").is_err());
        assert!(
            parse_signed_backend_id("w0.2.1#0123456789ab").is_err(),
            "old index-only IDs must not regain native GUI access"
        );
        assert!(parse_signed_backend_id("w0.2.1@bad#0123456789ab").is_err());
    }

    #[test]
    fn native_window_only_png_encoder_roundtrips_and_rejects_unbounded_pixels() {
        use std::io::Read as _;
        let rgba = [255_u8, 0, 0, 255, 0, 255, 0, 255];
        let encoded = encode_native_window_png(2, 1, &rgba).expect("window-only PNG");
        assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&encoded[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(encoded[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(encoded[20..24].try_into().unwrap()), 1);
        assert_eq!(&encoded[37..41], b"IDAT");
        let length = u32::from_be_bytes(encoded[33..37].try_into().unwrap()) as usize;
        let mut decoded = Vec::new();
        flate2::read::ZlibDecoder::new(&encoded[41..41 + length])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, [0, 255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(&encoded[encoded.len() - 8..encoded.len() - 4], b"IEND");
        assert!(encode_native_window_png(0, 10, &[]).is_err());
        assert!(encode_native_window_png(1, 1, &[]).is_err());
        assert!(encode_native_window_png(4096, 4096, &[]).is_err());
        assert!(encode_native_window_png(2, 1, &[0; 7]).is_err());
    }

    #[test]
    fn native_window_dib_conversion_scales_only_its_own_verified_bitmap() {
        // A 2x2 top-down Win32 DIB, BGRA order: red, green, blue, white.
        let bgra = [0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
        let (width, height, full) = native_window_bgra_to_rgba(2, 2, 100, &bgra).unwrap();
        assert_eq!((width, height), (2, 2));
        assert_eq!(
            full,
            [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]
        );
        let (width, height, scaled) = native_window_bgra_to_rgba(2, 2, 1, &bgra).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(scaled, [255, 0, 0, 255]);
        assert!(native_window_bgra_to_rgba(2, 2, 0, &[]).is_err());
        assert!(native_window_bgra_to_rgba(8192, 8192, 100, &[]).is_err());
    }

    #[test]
    fn windows_uia_writable_value_is_never_treated_as_text_outside_edit_controls() {
        assert!(
            super::windows_semantic_text_edit_allowed(50_004, true, false, false, false),
            "enabled writable Edit is allowed"
        );
        for role in [50_014, 50_015, 50_016, 50_012, 50_030, 50_025, 0] {
            assert!(
                !super::windows_semantic_text_edit_allowed(role, true, false, false, false),
                "arbitrary writable ValuePattern is not a text input"
            );
        }
        for (enabled, offscreen, sensitive, readonly) in [
            (false, false, false, false),
            (true, true, false, false),
            (true, false, true, false),
            (true, false, false, true),
        ] {
            assert!(!super::windows_semantic_text_edit_allowed(
                50_004, enabled, offscreen, sensitive, readonly
            ));
        }
    }

    #[test]
    fn application_ids_are_process_scoped() {
        assert_eq!(parse_application_pid("uia-pid-4242"), Ok(4242));
        assert!(parse_application_pid("uia-pid-0").is_err());
        assert!(parse_application_pid("desktop-app").is_err());
    }
}
