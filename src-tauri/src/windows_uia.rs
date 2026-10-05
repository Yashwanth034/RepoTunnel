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

fn signed_backend_id(path: &str, control_type_id: i32, name: &str, automation_id: &str) -> String {
    format!(
        "{path}#{}",
        element_signature(path, control_type_id, name, automation_id)
    )
}

fn parse_signed_backend_id(value: &str) -> Result<(&str, &str), String> {
    let (path, signature) = value
        .rsplit_once('#')
        .ok_or_else(|| "Invalid Windows UI Automation element identity.".to_string())?;
    if path.is_empty()
        || signature.len() != 12
        || !signature.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Invalid Windows UI Automation element identity.".to_string());
    }
    Ok((path, signature))
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
            UIInvokePattern, UIPatternType, UISelectionItemPattern, UITogglePattern, UIValuePattern,
        },
        types::{TreeScope, UIProperty},
        UIAutomation, UIElement, UITreeWalker,
    };

    use super::{
        element_signature, parse_application_pid, parse_signed_backend_id,
        role_from_control_type_id, signed_backend_id, MAX_CHILDREN_PER_NODE, MAX_DEPTH, MAX_NODES,
        MAX_SEQUENCE_STEPS, MAX_SEQUENCE_TEXT_BYTES, MAX_TEXT, MAX_TOTAL_WAIT_MS, MAX_TYPE_BYTES,
        MAX_WAIT_MS,
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

    fn cached_signature(path: &str, element: &UIElement) -> String {
        element_signature(
            path,
            cached_control_type_id(element),
            &cached_name(element),
            &cached_automation_id(element),
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
        if has_click && enabled && !offscreen {
            actions.push("click".to_string());
        }

        let mut text = String::new();
        if let Ok(value) = element.get_cached_pattern::<UIValuePattern>() {
            let readonly = value.cached_is_readonly().unwrap_or(true);
            if readonly {
                states.push("read-only".to_string());
            } else if !sensitive && enabled {
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

    fn cached_element_json(path: &str, element: &UIElement) -> Value {
        let control_type_id = cached_control_type_id(element);
        let name = cached_name(element);
        let automation_id = cached_automation_id(element);
        let sensitive = element.is_cached_password().unwrap_or(false);
        let (states, actions, text) = cached_states_actions_text(element, sensitive);
        json!({
            "id": signed_backend_id(path, control_type_id, &name, &automation_id),
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
            .unwrap_or_default();
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

    fn walk_cached(
        walker: &UITreeWalker,
        cache: &UICacheRequest,
        element: &UIElement,
        path: &str,
        depth: usize,
        wanted: usize,
        elements: &mut Vec<Value>,
        truncated: &mut bool,
    ) {
        if elements.len() >= wanted {
            *truncated = true;
            return;
        }
        elements.push(cached_element_json(path, element));
        if depth >= MAX_DEPTH {
            return;
        }
        let children = walker
            .get_children_build_cache(element, cache)
            .unwrap_or_default();
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
                depth + 1,
                wanted,
                elements,
                truncated,
            );
        }
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
                walk_cached(
                    &walker,
                    &cache,
                    window,
                    &path,
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
        let (path, expected_signature) = parse_signed_backend_id(backend_id)?;
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

    fn perform_type(element: &UIElement, text: &str, clear_first: bool) -> Result<String, String> {
        if text.len() > MAX_TYPE_BYTES || text.as_bytes().contains(&0) {
            return Err(format!(
                "Windows semantic typing may contain at most {MAX_TYPE_BYTES} bytes and no NUL bytes."
            ));
        }
        if element.is_password().unwrap_or(false) {
            return Err(
                "RepoTunnel blocks semantic typing into Windows password fields.".to_string(),
            );
        }
        if !element.is_enabled().unwrap_or(false) {
            return Err("That Windows UI Automation element is disabled.".to_string());
        }
        let pattern = element.get_pattern::<UIValuePattern>().map_err(|_| {
            "That Windows UI Automation element does not expose the Value pattern.".to_string()
        })?;
        if pattern.is_readonly().unwrap_or(true) {
            return Err("That Windows UI Automation field is read-only.".to_string());
        }
        let value = if clear_first {
            text.to_string()
        } else {
            let mut current = pattern.get_value().unwrap_or_default();
            current.push_str(text);
            current
        };
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
                let window = windows.first().ok_or_else(|| {
                    "That Windows UI Automation application has no current top-level window."
                        .to_string()
                })?;
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
pub(crate) use runtime::{action, inspect, list, semantic_sequence, WindowsUiaApplication};

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
        let id = signed_backend_id("w0.2.1", 50_000, "Save", "save-button");
        let (path, signature) = parse_signed_backend_id(&id).expect("signed backend id");
        assert_eq!(path, "w0.2.1");
        assert_eq!(
            signature,
            element_signature("w0.2.1", 50_000, "Save", "save-button")
        );
        assert!(parse_signed_backend_id("w0.2.1").is_err());
        assert!(parse_signed_backend_id("w0.2.1#bad").is_err());
    }

    #[test]
    fn application_ids_are_process_scoped() {
        assert_eq!(parse_application_pid("uia-pid-4242"), Ok(4242));
        assert!(parse_application_pid("uia-pid-0").is_err());
        assert!(parse_application_pid("desktop-app").is_err());
    }
}
