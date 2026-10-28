use sha2::{Digest, Sha256};

#[cfg(target_os = "macos")]
const MAX_NODES: usize = 800;
#[cfg(target_os = "macos")]
const MAX_DEPTH: usize = 10;
#[cfg(target_os = "macos")]
const MAX_CHILDREN_PER_NODE: usize = 200;
#[cfg(target_os = "macos")]
const MAX_TEXT: usize = 600;
#[cfg(target_os = "macos")]
const MAX_SEQUENCE_STEPS: usize = 64;
#[cfg(target_os = "macos")]
const MAX_WAIT_MS: u64 = 2_000;
#[cfg(target_os = "macos")]
const MAX_TOTAL_WAIT_MS: u64 = 10_000;
#[cfg(target_os = "macos")]
const MAX_TYPE_BYTES: usize = 32_768;
#[cfg(target_os = "macos")]
const MAX_SEQUENCE_TEXT_BYTES: usize = 131_072;

fn role_from_ax_role(role: &str) -> &'static str {
    match role {
        "AXButton" => "button",
        "AXCheckBox" => "check box",
        "AXRadioButton" => "radio button",
        "AXComboBox" | "AXPopUpButton" => "combo box",
        "AXTextField" | "AXTextArea" | "AXSearchField" => "entry",
        "AXLink" => "link",
        "AXImage" => "image",
        "AXList" => "list",
        "AXRow" => "list item",
        "AXCell" => "grid cell",
        "AXMenu" => "menu",
        "AXMenuBar" => "menu bar",
        "AXMenuItem" => "menu item",
        "AXTabGroup" => "tab list",
        "AXOutline" => "tree",
        "AXGroup" => "group",
        "AXWindow" | "AXSheet" | "AXDrawer" => "frame",
        "AXApplication" => "application",
        "AXSlider" => "slider",
        "AXStaticText" => "text",
        "AXToolbar" => "tool bar",
        "AXScrollArea" => "scroll area",
        "AXScrollBar" => "scroll bar",
        "AXProgressIndicator" => "progress bar",
        _ => "unknown",
    }
}

// Translate native AX capabilities to the same semantic operations used by
// Linux and Windows. A numeric scrollbar must never masquerade as a text
// editor, and AXPress must expose the standard "click" action.
#[cfg(any(test, target_os = "macos"))]
fn macos_element_enabled(flag: Option<bool>) -> bool {
    // Native accessibility providers can deny or omit AXEnabled. Such
    // elements are never granted click/type/scroll permissions by default.
    flag == Some(true)
}

#[cfg(any(test, target_os = "macos"))]
fn macos_semantic_capabilities(
    role: &str,
    enabled: bool,
    sensitive: bool,
    can_press: bool,
    writable_text_value: bool,
    writable_numeric_scroll: bool,
) -> Vec<String> {
    if !enabled || sensitive {
        return Vec::new();
    }
    let mut actions = Vec::new();
    if can_press {
        actions.push("press".to_string());
        actions.push("click".to_string());
    }
    if matches!(role, "AXTextField" | "AXTextArea" | "AXSearchField") && writable_text_value {
        actions.push("type".to_string());
    }
    if role == "AXScrollBar" && writable_numeric_scroll {
        actions.push("scroll".to_string());
    }
    actions
}

fn element_signature(path: &str, role: &str, name: &str, identifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update([0]);
    hasher.update(role.as_bytes());
    hasher.update([0]);
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(identifier.as_bytes());
    let digest = hasher.finalize();
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn signed_backend_id(
    path: &str,
    root_identity: &str,
    role: &str,
    name: &str,
    identifier: &str,
) -> String {
    format!(
        "{path}@{root_identity}#{}",
        element_signature(path, role, name, identifier)
    )
}

fn parse_signed_backend_id(value: &str) -> Result<(&str, &str, &str), String> {
    let (scoped_path, signature) = value
        .rsplit_once('#')
        .ok_or_else(|| "Invalid macOS Accessibility element identity.".to_string())?;
    let (path, root_identity) = scoped_path.rsplit_once('@').ok_or_else(|| {
        "Native Accessibility element identity lacks its owning window.".to_string()
    })?;
    if path.is_empty()
        || root_identity.len() != 12
        || !root_identity.bytes().all(|byte| byte.is_ascii_hexdigit())
        || signature.len() != 12
        || !signature.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Invalid macOS Accessibility element identity.".to_string());
    }
    Ok((path, root_identity, signature))
}

fn parse_application_pid(application_id: &str) -> Result<i32, String> {
    application_id
        .strip_prefix("ax-pid-")
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| "Invalid macOS Accessibility application ID.".to_string())
}

#[cfg(target_os = "macos")]
mod runtime {
    use std::{
        thread,
        time::{Duration, Instant},
    };

    use axuielement::AXUIElement;
    use objc2_app_kit::{NSApplicationActivationPolicy, NSWorkspace};
    use serde_json::{json, Value};

    use super::{
        element_signature, macos_element_enabled, macos_semantic_capabilities,
        parse_application_pid, parse_signed_backend_id, role_from_ax_role, signed_backend_id,
        MAX_CHILDREN_PER_NODE, MAX_DEPTH, MAX_NODES, MAX_SEQUENCE_STEPS, MAX_SEQUENCE_TEXT_BYTES,
        MAX_TEXT, MAX_TOTAL_WAIT_MS, MAX_TYPE_BYTES, MAX_WAIT_MS,
    };

    const AX_ROLE: &str = "AXRole";
    const AX_SUBROLE: &str = "AXSubrole";
    const AX_TITLE: &str = "AXTitle";
    const AX_DESCRIPTION: &str = "AXDescription";
    const AX_HELP: &str = "AXHelp";
    const AX_IDENTIFIER: &str = "AXIdentifier";
    const AX_VALUE: &str = "AXValue";
    const AX_ENABLED: &str = "AXEnabled";
    const AX_FOCUSED: &str = "AXFocused";
    const AX_POSITION: &str = "AXPosition";
    const AX_SIZE: &str = "AXSize";
    const AX_WINDOWS: &str = "AXWindows";
    const AX_MIN_VALUE: &str = "AXMinValue";
    const AX_MAX_VALUE: &str = "AXMaxValue";
    const AX_ORIENTATION: &str = "AXOrientation";
    const AX_VERTICAL_ORIENTATION: &str = "AXVerticalOrientation";
    const AX_HORIZONTAL_ORIENTATION: &str = "AXHorizontalOrientation";

    const AX_PRESS: &str = "AXPress";
    const AX_CONFIRM: &str = "AXConfirm";
    const AX_PICK: &str = "AXPick";
    const AX_RAISE: &str = "AXRaise";

    #[derive(Clone, Debug)]
    pub(crate) struct MacosAxApplication {
        pub(crate) id: String,
        pub(crate) name: String,
        pub(crate) running: bool,
        pub(crate) accessibility: bool,
        pub(crate) window_count: usize,
    }

    fn ax_error(context: &str, error: impl std::fmt::Display) -> String {
        format!("{context}: {error}")
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

    fn string_attribute(element: &AXUIElement, name: &str) -> String {
        element
            .string_attribute(name)
            .ok()
            .flatten()
            .map(clean)
            .unwrap_or_default()
    }

    fn role(element: &AXUIElement) -> String {
        string_attribute(element, AX_ROLE)
    }

    fn title(element: &AXUIElement) -> String {
        string_attribute(element, AX_TITLE)
    }

    fn identifier(element: &AXUIElement) -> String {
        string_attribute(element, AX_IDENTIFIER)
    }

    fn sensitive(element: &AXUIElement) -> bool {
        let role = role(element).to_ascii_lowercase();
        let subrole = string_attribute(element, AX_SUBROLE).to_ascii_lowercase();
        let title = title(element).to_ascii_lowercase();
        let description = string_attribute(element, AX_DESCRIPTION).to_ascii_lowercase();
        let combined = format!("{role} {subrole} {title} {description}");
        [
            "secure",
            "password",
            "passcode",
            "pin",
            "credential",
            "token",
            "one-time",
            "otp",
        ]
        .iter()
        .any(|hint| combined.contains(hint))
    }

    fn element_bounds(element: &AXUIElement) -> Option<Value> {
        let position = element.point_attribute(AX_POSITION).ok().flatten()?;
        let size = element.size_attribute(AX_SIZE).ok().flatten()?;
        if size.width <= 0.0 || size.height <= 0.0 {
            return None;
        }
        Some(json!({
            "x": position.x,
            "y": position.y,
            "width": size.width,
            "height": size.height,
        }))
    }

    fn element_states_actions_text(
        element: &AXUIElement,
        is_sensitive: bool,
    ) -> (Vec<String>, Vec<String>, String) {
        let enabled = macos_element_enabled(element.bool_attribute(AX_ENABLED).ok().flatten());
        let focused = element
            .bool_attribute(AX_FOCUSED)
            .ok()
            .flatten()
            .unwrap_or(false);
        let focusable = element.is_attribute_settable(AX_FOCUSED).unwrap_or(false);
        let value_settable = element.is_attribute_settable(AX_VALUE).unwrap_or(false);

        let mut states = vec![if enabled { "enabled" } else { "disabled" }.to_string()];
        if element_bounds(element).is_some() {
            states.push("visible".to_string());
        }
        if focused {
            states.push("focused".to_string());
        }
        if focusable {
            states.push("focusable".to_string());
        }
        let native_role = role(element);
        let can_type = matches!(
            native_role.as_str(),
            "AXTextField" | "AXTextArea" | "AXSearchField"
        ) && value_settable
            && !is_sensitive
            && element
                .string_attribute(AX_VALUE)
                .is_ok_and(|value| value.is_some());
        let can_scroll = native_role == "AXScrollBar"
            && value_settable
            && element
                .f64_attribute(AX_VALUE)
                .is_ok_and(|value| value.is_some());
        let native_actions = element.action_names().unwrap_or_default();
        let can_press = native_actions
            .iter()
            .any(|action| matches!(action.as_str(), AX_PRESS | AX_CONFIRM | AX_PICK));
        let actions = macos_semantic_capabilities(
            &native_role,
            enabled,
            is_sensitive,
            can_press,
            can_type,
            can_scroll,
        );
        if actions.iter().any(|action| action == "type") {
            states.push("editable".to_string());
        }

        let text = if is_sensitive {
            String::new()
        } else {
            element
                .string_attribute(AX_VALUE)
                .ok()
                .flatten()
                .map(bounded_text)
                .unwrap_or_default()
        };

        (states, actions, text)
    }

    fn element_json(path: &str, root_identity: &str, element: &AXUIElement) -> Value {
        let raw_role = role(element);
        let name = title(element);
        let identifier = identifier(element);
        let is_sensitive = sensitive(element);
        let (states, actions, text) = element_states_actions_text(element, is_sensitive);
        let description = {
            let value = string_attribute(element, AX_DESCRIPTION);
            if value.is_empty() {
                string_attribute(element, AX_HELP)
            } else {
                value
            }
        };
        json!({
            "id": signed_backend_id(path, root_identity, &raw_role, &name, &identifier),
            "role": role_from_ax_role(&raw_role),
            "name": name,
            "description": if is_sensitive { String::new() } else { description },
            "text": if is_sensitive { String::new() } else { text },
            "states": states,
            "actions": actions,
            "bounds": element_bounds(element),
            "sensitive": is_sensitive,
        })
    }

    fn application_element(pid: i32) -> Result<AXUIElement, String> {
        if pid == std::process::id() as i32 {
            return Err(
                "RepoTunnel cannot grant desktop control over its own process.".to_string(),
            );
        }
        let element = AXUIElement::from_pid(pid).ok_or_else(|| {
            "Could not create macOS Accessibility application element.".to_string()
        })?;
        let _ = element.set_timeout(0.75);
        Ok(element)
    }

    fn app_windows(pid: i32) -> Result<Vec<AXUIElement>, String> {
        let application = application_element(pid)?;
        application
            .element_array_attribute(AX_WINDOWS)
            .map_err(|error| ax_error("Could not read macOS Accessibility windows", error))
    }

    pub(crate) fn list() -> Result<Vec<MacosAxApplication>, String> {
        let workspace = NSWorkspace::sharedWorkspace();
        let own_pid = std::process::id() as i32;
        let mut applications = Vec::new();

        for application in workspace.runningApplications().iter() {
            if application.isTerminated()
                || application.activationPolicy() == NSApplicationActivationPolicy::Prohibited
            {
                continue;
            }
            let pid = application.processIdentifier();
            if pid <= 0 || pid == own_pid {
                continue;
            }
            let name = application
                .localizedName()
                .map(|value| value.to_string())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| format!("macOS application {pid}"));
            let ax = AXUIElement::from_pid(pid);
            let (accessibility, window_count) = match ax {
                Some(element) => {
                    let _ = element.set_timeout(0.35);
                    match element.element_array_attribute(AX_WINDOWS) {
                        Ok(windows) => (true, windows.len()),
                        Err(_) => (false, 0),
                    }
                }
                None => (false, 0),
            };
            applications.push(MacosAxApplication {
                id: format!("ax-pid-{pid}"),
                name,
                running: true,
                accessibility,
                window_count,
            });
        }

        Ok(applications)
    }

    fn walk(
        element: &AXUIElement,
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
        elements.push(element_json(path, root_identity, element));
        if depth >= MAX_DEPTH {
            *truncated = true;
            return;
        }
        let children = match element.children() {
            Ok(children) => children,
            Err(_) => {
                // An Accessibility provider error is not an empty tree.
                // Fail closed for native semantic coordinate hit-testing.
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
            walk(
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

    pub(crate) fn capture_window_png(
        application_id: &str,
        selected_window_id: &str,
        max_width: u32,
    ) -> Result<crate::macos_window_capture::NativeWindowFrame, String> {
        let pid = parse_application_pid(application_id)?;
        let windows = app_windows(pid)?;
        let signed_ids = windows
            .iter()
            .enumerate()
            .map(|(index, window)| {
                format!(
                    "ax-window-{}",
                    element_signature(
                        &format!("w{index}"),
                        &role(window),
                        &title(window),
                        &identifier(window),
                    )
                )
            })
            .collect::<Vec<_>>();
        let intrinsic = windows
            .iter()
            .map(|window| {
                element_signature(
                    "top-level",
                    &role(window),
                    &title(window),
                    &identifier(window),
                )
            })
            .collect::<Vec<_>>();
        let index = crate::desktop_control::select_native_unambiguous_window(
            &signed_ids,
            &intrinsic,
            Some(selected_window_id),
        )?;
        let window = &windows[index];
        let name = title(window);
        let bounds = element_bounds(window).ok_or_else(|| {
            "macOS native capture has no approved AX window geometry.".to_string()
        })?;
        let coordinate = |key: &str| -> Result<i32, String> {
            let value = bounds
                .get(key)
                .and_then(Value::as_f64)
                .ok_or_else(|| format!("macOS native window {key} is unavailable."))?;
            if !value.is_finite()
                || value.fract() != 0.0
                || value < i32::MIN as f64
                || value > i32::MAX as f64
            {
                return Err(format!(
                    "macOS native window {key} is not an exact coordinate."
                ));
            }
            Ok(value as i32)
        };
        let size = |key: &str| -> Result<u32, String> {
            let value = bounds
                .get(key)
                .and_then(Value::as_f64)
                .ok_or_else(|| format!("macOS native window {key} is unavailable."))?;
            if !value.is_finite() || value.fract() != 0.0 || !(1.0..=4096.0).contains(&value) {
                return Err(format!(
                    "macOS native window {key} is not a safe pixel dimension."
                ));
            }
            Ok(value as u32)
        };
        let geometry = (
            coordinate("x")?,
            coordinate("y")?,
            size("width")?,
            size("height")?,
        );
        let image = crate::macos_window_capture::capture_exact_window(
            pid as u32, &name, geometry, max_width,
        )?;
        // No stale/reordered AX window may inherit the WindowServer image:
        // inspect the same signed window ID after the screenshot completes.
        let current = app_windows(pid)?;
        let current_ids = current
            .iter()
            .enumerate()
            .map(|(index, window)| {
                format!(
                    "ax-window-{}",
                    element_signature(
                        &format!("w{index}"),
                        &role(window),
                        &title(window),
                        &identifier(window),
                    )
                )
            })
            .collect::<Vec<_>>();
        let current_intrinsic = current
            .iter()
            .map(|window| {
                element_signature(
                    "top-level",
                    &role(window),
                    &title(window),
                    &identifier(window),
                )
            })
            .collect::<Vec<_>>();
        let current_index = crate::desktop_control::select_native_unambiguous_window(
            &current_ids,
            &current_intrinsic,
            Some(selected_window_id),
        )?;
        if current_index != index
            || title(&current[current_index]) != name
            || element_bounds(&current[current_index]) != Some(bounds)
        {
            return Err("macOS AX window identity or geometry changed during capture.".into());
        }
        Ok(image)
    }

    pub(crate) fn owned_window_count(application_id: &str) -> Result<usize, String> {
        let pid = parse_application_pid(application_id)?;
        Ok(app_windows(pid)?.len())
    }

    pub(crate) fn inspect(application_id: &str, limit: usize) -> Result<Value, String> {
        let pid = parse_application_pid(application_id)?;
        let windows = app_windows(pid)?;
        if windows.is_empty() {
            return Err(
                "That macOS Accessibility application has no current accessible windows."
                    .to_string(),
            );
        }

        let wanted = limit.clamp(20, MAX_NODES);
        let mut window_values = Vec::new();
        let mut elements = Vec::new();
        let mut truncated = false;
        let mut application_name = String::new();

        for (index, window) in windows.iter().enumerate() {
            let path = format!("w{index}");
            let raw_role = role(window);
            let name = title(window);
            let id = identifier(window);
            if application_name.is_empty() && !name.is_empty() {
                application_name = name.clone();
            }
            window_values.push(json!({
                "windowId": format!(
                    "ax-window-{}",
                    element_signature(&path, &raw_role, &name, &id)
                ),
                "title": name,
                "bounds": element_bounds(window),
            }));
            if elements.len() < wanted {
                let root_identity = element_signature("top-level", &raw_role, &name, &id);
                walk(
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

        if application_name.is_empty() {
            application_name = format!("macOS application {pid}");
        }

        Ok(json!({
            "applicationId": application_id,
            "name": application_name,
            "semanticAvailable": true,
            "windows": window_values,
            "elements": elements,
            "truncated": truncated,
            "message": Value::Null,
        }))
    }

    fn current_signature(path: &str, element: &AXUIElement) -> String {
        element_signature(path, &role(element), &title(element), &identifier(element))
    }

    fn resolve_element(pid: i32, backend_id: &str) -> Result<AXUIElement, String> {
        let (path, expected_root_identity, expected_signature) =
            parse_signed_backend_id(backend_id)?;
        let mut parts = path.split('.');
        let window_index = parts
            .next()
            .and_then(|part| part.strip_prefix('w'))
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| "Invalid macOS Accessibility element path.".to_string())?;

        let windows = app_windows(pid)?;
        let identities = windows
            .iter()
            .map(|window| {
                element_signature(
                    "top-level",
                    &role(window),
                    &title(window),
                    &identifier(window),
                )
            })
            .collect::<Vec<_>>();
        crate::desktop_control::validate_native_semantic_window_owner(
            &identities,
            window_index,
            expected_root_identity,
        )?;
        let mut element = windows.into_iter().nth(window_index).ok_or_else(|| {
            "That macOS Accessibility window is no longer present. Inspect it again.".to_string()
        })?;
        for part in parts {
            let index = part
                .parse::<usize>()
                .map_err(|_| "Invalid macOS Accessibility element path.".to_string())?;
            element = element
                .children()
                .unwrap_or_default()
                .into_iter()
                .nth(index)
                .ok_or_else(|| {
                    "That macOS Accessibility element is no longer present. Inspect it again."
                        .to_string()
                })?;
        }

        if current_signature(path, &element) != expected_signature {
            return Err(
                "That macOS Accessibility element changed since inspection. Inspect it again."
                    .to_string(),
            );
        }
        if element.pid().ok() != Some(pid) {
            return Err("macOS Accessibility element escaped the permitted process.".to_string());
        }
        Ok(element)
    }

    fn perform_click(element: &AXUIElement) -> Result<String, String> {
        if sensitive(element) {
            return Err(
                "RepoTunnel blocks direct native actions on sensitive macOS controls.".into(),
            );
        }
        if !macos_element_enabled(element.bool_attribute(AX_ENABLED).ok().flatten()) {
            return Err("That macOS Accessibility element is disabled or cannot be authenticated as enabled.".to_string());
        }
        let actions = element.action_names().unwrap_or_default();
        let action = [AX_PRESS, AX_CONFIRM, AX_PICK]
            .into_iter()
            .find(|wanted| actions.iter().any(|available| available == wanted))
            .ok_or_else(|| {
                "That macOS Accessibility element exposes no supported semantic click action."
                    .to_string()
            })?;
        element
            .perform_action(action)
            .map_err(|error| ax_error("macOS Accessibility action failed", error))?;
        Ok(format!(
            "Performed native macOS Accessibility action {action}."
        ))
    }

    // Adjust only a signed, current, app-owned Accessibility scroll bar.
    // Never synthesize a CGEvent scroll wheel into the global desktop.
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
                "macOS semantic scroll requires one bounded 120-unit notch on one axis.".into(),
            );
        }
        let pid = parse_application_pid(application_id)?;
        let element = resolve_element(pid, element_id)?;
        if role(&element) != "AXScrollBar"
            || sensitive(&element)
            || !element
                .bool_attribute(AX_ENABLED)
                .ok()
                .flatten()
                .unwrap_or(false)
            || !element.is_attribute_settable(AX_VALUE).unwrap_or(false)
        {
            return Err(
                "macOS semantic scroll requires an enabled, non-sensitive writable AXScrollBar."
                    .into(),
            );
        }
        let orientation = string_attribute(&element, AX_ORIENTATION);
        let delta =
            match orientation.as_str() {
                AX_VERTICAL_ORIENTATION if delta_x == 0 => delta_y,
                AX_HORIZONTAL_ORIENTATION if delta_y == 0 => delta_x,
                _ => return Err(
                    "macOS semantic scroll axis does not match the signed AXScrollBar orientation."
                        .into(),
                ),
            };
        let current = element
            .f64_attribute(AX_VALUE)
            .map_err(|error| ax_error("macOS scrollbar value read failed", error))?
            .ok_or_else(|| "macOS scrollbar has no numeric accessibility value.".to_string())?;
        let minimum = element
            .f64_attribute(AX_MIN_VALUE)
            .map_err(|error| ax_error("macOS scrollbar minimum read failed", error))?
            .ok_or_else(|| "macOS scrollbar has no verifiable minimum value.".to_string())?;
        let maximum = element
            .f64_attribute(AX_MAX_VALUE)
            .map_err(|error| ax_error("macOS scrollbar maximum read failed", error))?
            .ok_or_else(|| "macOS scrollbar has no verifiable maximum value.".to_string())?;
        if !current.is_finite()
            || !minimum.is_finite()
            || !maximum.is_finite()
            || minimum >= maximum
            || current < minimum
            || current > maximum
        {
            return Err("macOS scrollbar range is invalid or unverifiable.".into());
        }
        let next = (current + f64::from(delta) / 120.0 * (maximum - minimum) / 20.0)
            .clamp(minimum, maximum);
        if next == current {
            return Err("macOS scrollbar is already at its permitted end.".into());
        }
        element.set_f64_attribute(AX_VALUE, next).map_err(|error| {
            ax_error("macOS signed Accessibility scrollbar write failed", error)
        })?;
        Ok(json!({
            "applicationId": application_id,
            "action": "scroll",
            "deltaX": delta_x,
            "deltaY": delta_y,
            "nativeScoped": true
        }))
    }

    fn perform_type(
        element: &AXUIElement,
        text: &str,
        clear_first: bool,
    ) -> Result<String, String> {
        if text.len() > MAX_TYPE_BYTES || text.as_bytes().contains(&0) {
            return Err(format!(
                "macOS semantic typing may contain at most {MAX_TYPE_BYTES} bytes and no NUL bytes."
            ));
        }
        if sensitive(element) {
            return Err(
                "RepoTunnel blocks semantic typing into macOS secure credential fields."
                    .to_string(),
            );
        }
        if !macos_element_enabled(element.bool_attribute(AX_ENABLED).ok().flatten()) {
            return Err("That macOS Accessibility element is disabled or cannot be authenticated as enabled.".to_string());
        }
        if !matches!(
            role(element).as_str(),
            "AXTextField" | "AXTextArea" | "AXSearchField"
        ) {
            return Err("macOS semantic typing only permits editable text controls, not sliders, scrollbars or other numeric values.".into());
        }
        if !element.is_attribute_settable(AX_VALUE).unwrap_or(false)
            || element.string_attribute(AX_VALUE).is_err()
        {
            return Err(
                "That macOS text control does not expose a verified writable string value."
                    .to_string(),
            );
        }
        let value = if clear_first {
            text.to_string()
        } else {
            let mut current = element
                .string_attribute(AX_VALUE)
                .map_err(|error| ax_error("Could not read existing macOS text value", error))?
                .ok_or_else(|| {
                    "macOS text field has no verified string value to append to.".to_string()
                })?;
            current.push_str(text);
            current
        };
        if value.len() > MAX_TYPE_BYTES {
            return Err("macOS combined native text value exceeds its 32,768-byte limit.".into());
        }
        element
            .set_string_attribute(AX_VALUE, &value)
            .map_err(|error| ax_error("macOS Accessibility value write failed", error))?;
        Ok(format!(
            "Entered {} bytes through the native macOS Accessibility value.",
            text.len()
        ))
    }

    fn perform_semantic_action(
        application_id: &str,
        action: &str,
        element_id: &str,
        text: Option<&str>,
        clear_first: bool,
    ) -> Result<Value, String> {
        let pid = parse_application_pid(application_id)?;
        let element = resolve_element(pid, element_id)?;
        let detail = match action {
            "click" => perform_click(&element)?,
            "type" => perform_type(
                &element,
                text.ok_or_else(|| "macOS semantic type requires text.".to_string())?,
                clear_first,
            )?,
            _ => return Err("macOS semantic action must be click or type.".to_string()),
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
        match action {
            "activate" => {
                let windows = app_windows(pid)?;
                let signed_ids = windows
                    .iter()
                    .enumerate()
                    .map(|(index, window)| {
                        format!(
                            "ax-window-{}",
                            element_signature(
                                &format!("w{index}"),
                                &role(window),
                                &title(window),
                                &identifier(window),
                            )
                        )
                    })
                    .collect::<Vec<_>>();
                let intrinsic = windows
                    .iter()
                    .map(|window| {
                        element_signature(
                            "top-level",
                            &role(window),
                            &title(window),
                            &identifier(window),
                        )
                    })
                    .collect::<Vec<_>>();
                let index = crate::desktop_control::select_native_unambiguous_window(
                    &signed_ids,
                    &intrinsic,
                    element_id,
                )?;
                let window = &windows[index];
                if window.perform_action(AX_RAISE).is_err() {
                    window
                        .set_bool_attribute(AX_FOCUSED, true)
                        .map_err(|error| ax_error("macOS Accessibility focus failed", error))?;
                }
                Ok(json!({
                    "applicationId": application_id,
                    "action": "activate",
                    "detail": "Focused the permitted macOS Accessibility application.",
                }))
            }
            "click" | "type" => perform_semantic_action(
                application_id,
                action,
                element_id.ok_or_else(|| {
                    "macOS click/type requires an inspected semantic element ID.".to_string()
                })?,
                text,
                clear_first,
            ),
            "key" | "scroll" => Err(
                "macOS Stage 11 uses semantic Accessibility only for key/scroll; use a semantic click/type ref now. Native keyboard/scroll fallback is intentionally not enabled in this adapter."
                    .to_string(),
            ),
            _ => Err("Unsupported macOS desktop action.".to_string()),
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
                "macOS semantic sequence requires 1..{MAX_SEQUENCE_STEPS} steps."
            ));
        }

        let started = Instant::now();
        let mut total_wait_ms = 0_u64;
        let mut total_text_bytes = 0_usize;
        let mut completed = 0_usize;
        let mut results = Vec::new();

        for (index, step) in steps.iter().enumerate() {
            before_step().map_err(|error| format!("SEQUENCE_STEP_{}: {error}", index + 1))?;
            if started.elapsed() > Duration::from_secs(20) {
                return Err(format!(
                    "SEQUENCE_TIMEOUT: macOS semantic sequence exceeded 20 seconds before step {}.",
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
                "click" => perform_semantic_action(
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
                    perform_semantic_action(
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
                        "SEQUENCE_STEP_{}: unsupported macOS semantic sequence operation.",
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

#[cfg(target_os = "macos")]
pub(crate) use runtime::{
    action, capture_window_png, inspect, list, owned_window_count, scroll, semantic_sequence,
    semantic_sequence_with_guard,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_macos_ax_roles_to_shared_roles() {
        assert_eq!(role_from_ax_role("AXButton"), "button");
        assert_eq!(role_from_ax_role("AXCheckBox"), "check box");
        assert_eq!(role_from_ax_role("AXTextField"), "entry");
        assert_eq!(role_from_ax_role("AXMenuItem"), "menu item");
        assert_eq!(role_from_ax_role("AXWindow"), "frame");
        assert_eq!(role_from_ax_role("SomethingElse"), "unknown");
    }

    #[test]
    fn backend_ids_are_signed_and_reject_malformed_values() {
        let owner = element_signature("top-level", "AXWindow", "Editor One", "window-one");
        let id = signed_backend_id("w0.2.1", &owner, "AXButton", "Save", "save-button");
        let (path, root_identity, signature) =
            parse_signed_backend_id(&id).expect("signed backend id");
        assert_eq!(path, "w0.2.1");
        assert_eq!(root_identity, owner);
        assert_eq!(
            signature,
            element_signature("w0.2.1", "AXButton", "Save", "save-button")
        );
        let other_owner = element_signature("top-level", "AXWindow", "Editor Two", "window-two");
        assert_ne!(owner, other_owner);
        assert_ne!(
            signed_backend_id("w0.2.1", &owner, "AXButton", "Save", "save-button"),
            signed_backend_id("w0.2.1", &other_owner, "AXButton", "Save", "save-button"),
            "same Save button in another root window must have a different semantic ID"
        );
        assert!(parse_signed_backend_id("w0.2.1").is_err());
        assert!(
            parse_signed_backend_id("w0.2.1#0123456789ab").is_err(),
            "legacy index-only IDs must not authorize native GUI actions"
        );
        assert!(parse_signed_backend_id("w0.2.1@bad#0123456789ab").is_err());
    }

    #[test]
    fn macos_native_capabilities_map_only_safe_click_type_and_scroll() {
        assert!(
            !super::macos_element_enabled(None),
            "unreadable AXEnabled must never allow action execution"
        );
        assert!(!super::macos_element_enabled(Some(false)));
        assert!(super::macos_element_enabled(Some(true)));
        assert_eq!(
            super::macos_semantic_capabilities("AXButton", true, false, true, false, false),
            vec!["press", "click"]
        );
        assert_eq!(
            super::macos_semantic_capabilities("AXTextField", true, false, false, true, false),
            vec!["type"]
        );
        assert_eq!(
            super::macos_semantic_capabilities("AXTextArea", true, false, false, true, false),
            vec!["type"]
        );
        assert_eq!(
            super::macos_semantic_capabilities("AXScrollBar", true, false, false, true, true),
            vec!["scroll"],
            "numeric scrollbars must not be advertised as text fields"
        );
        assert!(
            super::macos_semantic_capabilities("AXSlider", true, false, false, true, false)
                .is_empty()
        );
        assert!(
            super::macos_semantic_capabilities("AXTextField", true, true, true, true, true)
                .is_empty(),
            "secure fields must expose no interactive actions"
        );
        assert!(
            super::macos_semantic_capabilities("AXButton", false, false, true, false, false)
                .is_empty()
        );
    }

    #[test]
    fn application_ids_are_process_scoped() {
        assert_eq!(parse_application_pid("ax-pid-4242"), Ok(4242));
        assert!(parse_application_pid("ax-pid-0").is_err());
        assert!(parse_application_pid("desktop-app").is_err());
    }
}
