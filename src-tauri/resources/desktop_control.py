#!/usr/bin/env python3
import base64
import hashlib
import io
import json
import os
import re
import sys
import time

def accessibility_bus_available():
    if os.environ.get("AT_SPI_BUS_ADDRESS"):
        return True
    runtime_dir = os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    return os.path.exists(os.path.join(runtime_dir, "at-spi", "bus_0"))


if accessibility_bus_available():
    try:
        import pyatspi
    except Exception:
        pyatspi = None
else:
    pyatspi = None

try:
    from Xlib import X, XK, display, protocol
    from Xlib.ext import xtest
except Exception:
    X = XK = display = protocol = xtest = None

try:
    from PIL import Image
except Exception:
    Image = None

MAX_ELEMENTS = 800
MAX_TEXT = 600
PROTECTED_NAMES = ("repotunnel",)
SENSITIVE_HINTS = ("password", "passwd", "passcode", "pin", "secret", "credential", "token", "api key")


def reply(result=None, error=None):
    payload = {"ok": error is None}
    if error is None:
        payload["result"] = result if result is not None else {}
    else:
        payload["error"] = str(error)
    print(json.dumps(payload, ensure_ascii=False))


def clean(value):
    return " ".join(str(value or "").replace("\x00", " ").split())


def slug(value):
    value = clean(value).lower()
    value = re.sub(r"[^a-z0-9._-]+", "-", value).strip("-._")
    return value[:160] or "desktop-app"


def protected(value):
    low = clean(value).lower()
    return any(item in low for item in PROTECTED_NAMES)


def x_display():
    if display is None:
        return None
    try:
        return display.Display()
    except Exception:
        return None


def atom_text(d, window, atom_name):
    try:
        atom = d.intern_atom(atom_name)
        prop = window.get_full_property(atom, X.AnyPropertyType)
        if not prop or prop.value is None:
            return ""
        raw = prop.value
        if isinstance(raw, bytes):
            return raw.decode("utf-8", "replace")
        return clean(raw)
    except Exception:
        return ""


def atom_int(d, window, atom_name):
    try:
        atom = d.intern_atom(atom_name)
        prop = window.get_full_property(atom, X.AnyPropertyType)
        if not prop or prop.value is None or len(prop.value) == 0:
            return None
        return int(prop.value[0])
    except Exception:
        return None


def window_bounds(window):
    try:
        geom = window.get_geometry()
        translated = window.translate_coords(window.query_tree().root, 0, 0)
        return {
            "x": int(translated.x), "y": int(translated.y),
            "width": int(geom.width), "height": int(geom.height),
        }
    except Exception:
        return None


def list_x_windows():
    d = x_display()
    if d is None:
        return []
    try:
        root = d.screen().root
        client_atom = d.intern_atom("_NET_CLIENT_LIST")
        prop = root.get_full_property(client_atom, X.AnyPropertyType)
        ids = list(prop.value) if prop and prop.value is not None else []
        out = []
        for xid in ids:
            try:
                w = d.create_resource_object("window", int(xid))
                wm_class = w.get_wm_class() or ()
                class_name = clean(wm_class[-1] if wm_class else "")
                instance = clean(wm_class[0] if wm_class else "")
                title = atom_text(d, w, "_NET_WM_NAME") or clean(w.get_wm_name())
                pid = atom_int(d, w, "_NET_WM_PID")
                bounds = window_bounds(w)
                if not bounds or bounds["width"] <= 1 or bounds["height"] <= 1:
                    continue
                identity = class_name or instance or title or f"window-{int(xid):x}"
                app_id = slug(identity)
                if protected(identity) or protected(title):
                    continue
                out.append({
                    "windowId": f"0x{int(xid):x}", "pid": pid, "title": title,
                    "className": class_name, "instance": instance,
                    "applicationId": app_id, "bounds": bounds,
                })
            except Exception:
                continue
        return out
    finally:
        try:
            d.close()
        except Exception:
            pass


def a11y_apps():
    if pyatspi is None:
        return []
    try:
        desktop = pyatspi.Registry.getDesktop(0)
    except Exception:
        return []
    apps = []
    for i in range(getattr(desktop, "childCount", 0)):
        try:
            app = desktop.getChildAtIndex(i)
            name = clean(getattr(app, "name", ""))
            if protected(name):
                continue
            pid = None
            try:
                pid = int(app.get_process_id())
            except Exception:
                try:
                    pid = int(app.get_process_id)
                except Exception:
                    pass
            apps.append({"accessible": app, "name": name or "Desktop application", "pid": pid})
        except Exception:
            continue
    return apps


def discover():
    windows = list_x_windows()
    a11y = a11y_apps()
    by_pid = {item["pid"]: item for item in a11y if item.get("pid")}
    grouped = {}
    for win in windows:
        app_id = win["applicationId"]
        entry = grouped.setdefault(app_id, {
            "id": app_id,
            "name": win["className"] or win["instance"] or win["title"] or app_id,
            "pid": win.get("pid"), "accessibility": False, "windows": [],
        })
        entry["windows"].append(win)
        a = by_pid.get(win.get("pid"))
        if a:
            entry["accessibility"] = True
            if a["name"]:
                entry["name"] = a["name"]
            entry["a11y"] = a["accessible"]
    used_pids = {item.get("pid") for item in grouped.values()}
    for a in a11y:
        if a.get("pid") in used_pids:
            continue
        app_id = slug(a["name"])
        if protected(app_id):
            continue
        grouped.setdefault(app_id, {
            "id": app_id, "name": a["name"], "pid": a.get("pid"),
            "accessibility": True, "windows": [], "a11y": a["accessible"],
        })
    return grouped


def public_app(entry):
    return {
        "id": entry["id"], "name": clean(entry["name"]), "running": True,
        "accessibility": bool(entry.get("accessibility")),
        "windowCount": len(entry.get("windows", [])),
    }


def find_app(app_id):
    apps = discover()
    entry = apps.get(app_id)
    if not entry:
        raise RuntimeError("That desktop application is not currently running.")
    if protected(entry.get("id")) or protected(entry.get("name")):
        raise RuntimeError("RepoTunnel cannot grant desktop control over its own application.")
    return entry


def role_name(node):
    try:
        return clean(node.getRoleName())
    except Exception:
        return "unknown"


def node_sensitive(node):
    role = role_name(node).lower()
    label = (clean(getattr(node, "name", "")) + " " + clean(getattr(node, "description", ""))).lower()
    if "password" in role:
        return True
    return any(hint in label for hint in SENSITIVE_HINTS)


def state_names(node):
    names = []
    try:
        state = node.getState()
        for value in state.getStates():
            try:
                names.append(clean(pyatspi.stateToString(value)))
            except Exception:
                pass
    except Exception:
        pass
    return [name for name in names if name]


def node_actions(node):
    actions = []
    try:
        action = node.queryAction()
        for i in range(action.nActions):
            actions.append(clean(action.getName(i)))
    except Exception:
        pass
    return [item for item in actions if item]


def node_bounds(node):
    try:
        component = node.queryComponent()
        ext = component.getExtents(pyatspi.DESKTOP_COORDS)
        if ext.width <= 0 or ext.height <= 0:
            return None
        return {"x": int(ext.x), "y": int(ext.y), "width": int(ext.width), "height": int(ext.height)}
    except Exception:
        return None


def node_text(node, sensitive):
    if sensitive:
        return ""
    try:
        text = node.queryText()
        count = min(int(text.characterCount), MAX_TEXT)
        return clean(text.getText(0, count))
    except Exception:
        return ""


def signature(path, node):
    raw = f"{path}|{role_name(node)}|{clean(getattr(node, 'name', ''))}"
    return hashlib.sha256(raw.encode("utf-8", "replace")).hexdigest()[:12]


def element_id(path, node):
    return f"{path}#{signature(path, node)}"


def inspect_app(app_id, limit):
    entry = find_app(app_id)
    app = entry.get("a11y")
    windows = [{k: v for k, v in w.items() if k in ("windowId", "title", "bounds")} for w in entry.get("windows", [])]
    if app is None:
        return {
            "applicationId": app_id, "name": entry["name"], "semanticAvailable": False,
            "windows": windows, "elements": [], "truncated": False,
            "message": "This app is not exposing an accessibility tree. Use a window screenshot and window-scoped coordinate click/scroll fallback.",
        }
    elements = []
    truncated = False
    wanted = max(20, min(int(limit or 300), MAX_ELEMENTS))

    def walk(node, path, depth):
        nonlocal truncated
        if len(elements) >= wanted:
            truncated = True
            return
        sensitive = node_sensitive(node)
        role = role_name(node)
        name = clean(getattr(node, "name", ""))
        description = clean(getattr(node, "description", ""))
        actions = node_actions(node)
        bounds = node_bounds(node)
        states = state_names(node)
        text = node_text(node, sensitive)
        useful = depth <= 1 or name or description or actions or text or role.lower() in (
            "push button", "button", "menu", "menu item", "check box", "radio button",
            "text", "entry", "combo box", "page tab", "tree item", "list item", "slider",
        )
        if useful:
            elements.append({
                "id": element_id(path, node), "role": role, "name": name,
                "description": description, "text": text, "states": states,
                "actions": actions, "bounds": bounds, "sensitive": sensitive,
            })
        if depth >= 10:
            return
        try:
            count = min(int(node.childCount), 200)
        except Exception:
            count = 0
        for index in range(count):
            if len(elements) >= wanted:
                truncated = True
                return
            try:
                child = node.getChildAtIndex(index)
                walk(child, f"{path}.{index}", depth + 1)
            except Exception:
                continue

    try:
        count = min(int(app.childCount), 80)
    except Exception:
        count = 0
    for wi in range(count):
        try:
            walk(app.getChildAtIndex(wi), f"w{wi}", 0)
        except Exception:
            continue
    return {
        "applicationId": app_id, "name": entry["name"], "semanticAvailable": True,
        "windows": windows, "elements": elements, "truncated": truncated, "message": None,
    }


def resolve_element(entry, encoded):
    app = entry.get("a11y")
    if app is None:
        raise RuntimeError("This application is not exposing semantic accessibility elements.")
    if "#" not in encoded:
        raise RuntimeError("Invalid desktop element id. Inspect the app again and use an element id from that result.")
    path, expected = encoded.rsplit("#", 1)
    if not path.startswith("w"):
        raise RuntimeError("Invalid desktop element id.")
    parts = path[1:].split(".")
    try:
        node = app.getChildAtIndex(int(parts[0]))
        for part in parts[1:]:
            node = node.getChildAtIndex(int(part))
    except Exception:
        raise RuntimeError("That UI element is no longer present. Inspect the app again before acting.")
    if signature(path, node) != expected:
        raise RuntimeError("That UI element changed since inspection. Inspect the app again before acting.")
    return node


def x_window(entry, requested=None):
    windows = entry.get("windows", [])
    if requested:
        for win in windows:
            if win["windowId"].lower() == str(requested).lower():
                return win
        raise RuntimeError("That window does not belong to the enabled application anymore.")
    if not windows:
        raise RuntimeError("No controllable X11 window was found for this application.")
    return windows[0]


def active_window_id(d):
    try:
        root = d.screen().root
        atom = d.intern_atom("_NET_ACTIVE_WINDOW")
        prop = root.get_full_property(atom, X.AnyPropertyType)
        if prop and prop.value is not None and len(prop.value):
            return int(prop.value[0])
    except Exception:
        pass
    return None


def top_level_window_id(window):
    try:
        root = window.query_tree().root
        current = window
        while True:
            parent = current.query_tree().parent
            if parent is None or parent.id == root.id:
                return int(current.id)
            current = parent
    except Exception:
        return int(window.id)


def window_ancestor_ids(window):
    ids = []
    try:
        root = window.query_tree().root
        current = window
        for _ in range(32):
            ids.append(int(current.id))
            parent = current.query_tree().parent
            if parent is None or parent.id == root.id:
                break
            current = parent
    except Exception:
        if not ids:
            ids.append(int(window.id))
    return ids


def window_process_id(d, window):
    try:
        current = window
        root = window.query_tree().root
        for _ in range(32):
            pid = atom_int(d, current, "_NET_WM_PID")
            if pid:
                return int(pid)
            parent = current.query_tree().parent
            if parent is None or parent.id == root.id:
                break
            current = parent
    except Exception:
        pass
    return None


def window_belongs_to_entry(d, entry, window):
    expected_pid = entry.get("pid")
    if expected_pid and window_process_id(d, window) == int(expected_pid):
        return True
    candidate_ids = set(window_ancestor_ids(window))
    for owned in entry.get("windows", []):
        try:
            owned_window = d.create_resource_object("window", int(str(owned["windowId"]), 16))
            owned_ids = set(window_ancestor_ids(owned_window))
            if candidate_ids & owned_ids:
                return True
        except Exception:
            continue
    return False


def ensure_active_window(d, entry, window):
    active_id = active_window_id(d)
    if active_id == int(window.id):
        return
    if active_id is not None:
        try:
            active = d.create_resource_object("window", int(active_id))
            if window_belongs_to_entry(d, entry, active):
                return
        except Exception:
            pass
    raise RuntimeError("The enabled application did not become the active window, so RepoTunnel refused keyboard or pointer input.")


def deepest_pointer_window(root):
    try:
        current = root.query_pointer().child
        if current is None:
            return None
        for _ in range(32):
            child = current.query_pointer().child
            if child is None or int(child.id) == int(current.id):
                break
            current = child
        return current
    except Exception:
        return None


def ensure_pointer_owned(d, entry, window):
    root = d.screen().root
    pointed = deepest_pointer_window(root)
    if pointed is not None and window_belongs_to_entry(d, entry, pointed):
        return
    # Some window managers report the decoration/frame as the pointed window.
    # Accept that frame only when it is an ancestor of one of this application's windows.
    if pointed is not None:
        pointed_ids = set(window_ancestor_ids(pointed))
        for owned in entry.get("windows", []):
            try:
                owned_window = d.create_resource_object("window", int(str(owned["windowId"]), 16))
                if pointed_ids & set(window_ancestor_ids(owned_window)):
                    return
            except Exception:
                continue
    raise RuntimeError("Another window is covering the requested point. RepoTunnel refused to send input outside the enabled application.")


def activate_window(d, entry, xid):
    root = d.screen().root
    w = d.create_resource_object("window", int(str(xid), 16))
    atom = d.intern_atom("_NET_ACTIVE_WINDOW")
    event = protocol.event.ClientMessage(window=w, client_type=atom, data=(32, [2, int(time.time()), 0, 0, 0]))
    root.send_event(event, event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
    d.sync()
    time.sleep(0.08)
    ensure_active_window(d, entry, w)
    return w


def activate_target(entry, win):
    d = x_display()
    if d is None or protocol is None:
        raise RuntimeError("Window activation is unavailable in this desktop session.")
    try:
        bounds = win.get("bounds") or {}
        screen = d.screen()
        offscreen = (
            int(bounds.get("x", 0)) < 0
            or int(bounds.get("y", 0)) < 0
            or int(bounds.get("x", 0)) + int(bounds.get("width", 0)) > int(screen.width_in_pixels)
            or int(bounds.get("y", 0)) + int(bounds.get("height", 0)) > int(screen.height_in_pixels)
        )
        w = d.create_resource_object("window", int(str(win["windowId"]), 16))
        if offscreen:
            root = screen.root
            state_atom = d.intern_atom("_NET_WM_STATE")
            max_vert = d.intern_atom("_NET_WM_STATE_MAXIMIZED_VERT")
            max_horz = d.intern_atom("_NET_WM_STATE_MAXIMIZED_HORZ")
            event = protocol.event.ClientMessage(
                window=w,
                client_type=state_atom,
                data=(32, [1, int(max_vert), int(max_horz), 1, 0]),
            )
            root.send_event(event, event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
            d.sync()
            time.sleep(0.08)
        activate_window(d, entry, win["windowId"])
    finally:
        d.close()
    return "Activated the enabled application window and normalized it on-screen when needed."


def click_point(entry, win, x, y):
    d = x_display()
    if d is None or xtest is None:
        raise RuntimeError("Window-scoped mouse fallback is unavailable in this desktop session.")
    bounds = win["bounds"]
    if not (bounds["x"] <= x < bounds["x"] + bounds["width"] and bounds["y"] <= y < bounds["y"] + bounds["height"]):
        raise RuntimeError("Refusing to click outside the enabled application's window.")
    try:
        window = activate_window(d, entry, win["windowId"])
        xtest.fake_input(d, X.MotionNotify, x=x, y=y)
        d.sync()
        ensure_pointer_owned(d, entry, window)
        xtest.fake_input(d, X.ButtonPress, 1)
        xtest.fake_input(d, X.ButtonRelease, 1)
        d.sync()
    finally:
        d.close()


def semantic_click(entry, encoded):
    node = resolve_element(entry, encoded)
    actions = node_actions(node)
    if actions:
        preferred = ("click", "press", "activate", "open", "toggle", "select")
        try:
            action = node.queryAction()
            names = [clean(action.getName(i)).lower() for i in range(action.nActions)]
            index = next((names.index(name) for name in preferred if name in names), 0)
            if action.doAction(index):
                return "Invoked the element's accessibility action."
        except Exception:
            pass
    bounds = node_bounds(node)
    if not bounds:
        raise RuntimeError("This element has no clickable accessibility action or visible bounds.")
    win = next((w for w in entry.get("windows", []) if w["bounds"]["x"] <= bounds["x"] < w["bounds"]["x"] + w["bounds"]["width"] and w["bounds"]["y"] <= bounds["y"] < w["bounds"]["y"] + w["bounds"]["height"]), None)
    if not win:
        raise RuntimeError("RepoTunnel could not prove that this element is inside the enabled app window.")
    click_point(entry, win, bounds["x"] + bounds["width"] // 2, bounds["y"] + bounds["height"] // 2)
    return "Clicked the inspected element inside the enabled application window."


def type_text(entry, encoded, text, clear_first):
    node = resolve_element(entry, encoded)
    if node_sensitive(node):
        raise RuntimeError("RepoTunnel blocks AI typing into password, PIN, credential, token, and other sensitive fields.")
    try:
        node.queryComponent().grabFocus()
    except Exception:
        pass
    try:
        editable = node.queryEditableText()
    except Exception:
        raise RuntimeError("That inspected UI element is not an editable text field.")
    text = str(text or "")
    if len(text) > 32768:
        raise RuntimeError("Desktop typing is limited to 32,768 characters per action.")
    try:
        if clear_first:
            editable.setTextContents(text)
        else:
            offset = 0
            try:
                t = node.queryText()
                offset = int(t.caretOffset)
            except Exception:
                try:
                    offset = int(node.queryText().characterCount)
                except Exception:
                    pass
            editable.insertText(offset, text, len(text))
    except Exception as exc:
        raise RuntimeError(f"The application refused semantic text editing: {exc}")
    return f"Entered {len(text)} characters into a verified non-sensitive field."


def parse_shortcut(value):
    value = clean(value)
    if not value or len(value) > 80:
        raise RuntimeError("Enter a short keyboard shortcut such as Ctrl+S, Escape, F5, or Enter.")
    parts = [p.strip() for p in value.split("+") if p.strip()]
    if not parts:
        raise RuntimeError("Keyboard shortcut cannot be empty.")
    mods = []
    key = parts[-1]
    mod_map = {"ctrl": "Control_L", "control": "Control_L", "alt": "Alt_L", "shift": "Shift_L", "super": "Super_L", "meta": "Super_L"}
    for part in parts[:-1]:
        mapped = mod_map.get(part.lower())
        if not mapped:
            raise RuntimeError("Only Ctrl, Alt, Shift and Super are supported shortcut modifiers.")
        mods.append(mapped)
    named = {"enter": "Return", "return": "Return", "escape": "Escape", "esc": "Escape", "tab": "Tab", "space": "space", "backspace": "BackSpace", "delete": "Delete", "up": "Up", "down": "Down", "left": "Left", "right": "Right", "home": "Home", "end": "End", "pageup": "Page_Up", "pagedown": "Page_Down"}
    low = key.lower()
    key = named.get(low, key)
    safe_plain = key in named.values() or re.fullmatch(r"F(?:[1-9]|1[0-2])", key, re.I)
    if len(key) == 1 and key.isprintable() and not any(m in mods for m in ("Control_L", "Alt_L", "Super_L")):
        raise RuntimeError("Plain printable keys are blocked. Use semantic desktop typing for text, or a modifier shortcut such as Ctrl+S.")
    if not safe_plain and len(key) != 1 and not re.fullmatch(r"F(?:[1-9]|1[0-2])", key, re.I):
        raise RuntimeError("That key is not in RepoTunnel's desktop shortcut allowlist.")
    return mods, key


def send_shortcut(entry, win, shortcut):
    d = x_display()
    if d is None or xtest is None or XK is None:
        raise RuntimeError("Keyboard shortcut control is unavailable in this desktop session.")
    mods, key = parse_shortcut(shortcut)
    try:
        window = activate_window(d, entry, win["windowId"])
        ensure_active_window(d, entry, window)
        mod_codes = []
        for mod in mods:
            code = d.keysym_to_keycode(XK.string_to_keysym(mod))
            if not code:
                raise RuntimeError(f"Could not resolve modifier {mod}.")
            mod_codes.append(code)
            xtest.fake_input(d, X.KeyPress, code)
        keysym = XK.string_to_keysym(key)
        if not keysym and len(key) == 1:
            keysym = XK.string_to_keysym(key.lower())
        code = d.keysym_to_keycode(keysym)
        if not code:
            raise RuntimeError(f"Could not resolve key {key}.")
        xtest.fake_input(d, X.KeyPress, code)
        xtest.fake_input(d, X.KeyRelease, code)
        for code in reversed(mod_codes):
            xtest.fake_input(d, X.KeyRelease, code)
        d.sync()
    finally:
        d.close()
    return f"Sent shortcut {shortcut} to the enabled application window."


def scroll_window(entry, win, dx, dy, x_ratio, y_ratio):
    d = x_display()
    if d is None or xtest is None:
        raise RuntimeError("Window-scoped scrolling is unavailable in this desktop session.")
    bounds = win["bounds"]
    xr = max(0.02, min(float(x_ratio if x_ratio is not None else 0.5), 0.98))
    yr = max(0.02, min(float(y_ratio if y_ratio is not None else 0.5), 0.98))
    x = bounds["x"] + int(bounds["width"] * xr)
    y = bounds["y"] + int(bounds["height"] * yr)
    try:
        window = activate_window(d, entry, win["windowId"])
        xtest.fake_input(d, X.MotionNotify, x=x, y=y)
        d.sync()
        ensure_pointer_owned(d, entry, window)
        for delta, neg, pos in ((int(dy or 0), 4, 5), (int(dx or 0), 6, 7)):
            button = pos if delta > 0 else neg
            count = min(30, max(0, (abs(delta) + 119) // 120))
            for _ in range(count):
                xtest.fake_input(d, X.ButtonPress, button)
                xtest.fake_input(d, X.ButtonRelease, button)
        d.sync()
    finally:
        d.close()
    return "Scrolled inside the enabled application window."


def screenshot(entry, win):
    if Image is None:
        raise RuntimeError("Desktop screenshot support is unavailable because Pillow is missing.")
    d = x_display()
    if d is None:
        raise RuntimeError("Desktop screenshot support requires the current X11 desktop session.")
    try:
        window = d.create_resource_object("window", int(str(win["windowId"]), 16))
        geom = window.get_geometry()
        width = int(geom.width)
        height = int(geom.height)
        raw = window.get_image(0, 0, width, height, X.ZPixmap, 0xFFFFFFFF)
        if raw is None or not raw.data:
            raise RuntimeError("The enabled application did not return drawable window pixels.")
        pixels = max(1, width * height)
        bytes_per_pixel = len(raw.data) // pixels
        if bytes_per_pixel >= 4:
            image = Image.frombytes("RGB", (width, height), raw.data, "raw", "BGRX")
        elif bytes_per_pixel == 3:
            image = Image.frombytes("RGB", (width, height), raw.data, "raw", "BGR")
        else:
            raise RuntimeError("This window pixel format is not safely supported by RepoTunnel.")
    except Exception as exc:
        if isinstance(exc, RuntimeError):
            raise
        raise RuntimeError(f"Could not capture the enabled application's own window: {exc}")
    finally:
        d.close()
    stream = io.BytesIO()
    image.save(stream, format="PNG", optimize=True)
    data = stream.getvalue()
    return {
        "applicationId": entry["id"], "windowId": win["windowId"], "mimeType": "image/png",
        "sizeBytes": len(data), "data": base64.b64encode(data).decode("ascii"),
        "width": int(image.width), "height": int(image.height),
    }


def main(req):
    operation = req.get("operation")
    if operation == "list":
        apps = [public_app(v) for v in discover().values()]
        apps.sort(key=lambda item: item["name"].lower())
        probe = x_display()
        window_fallback = probe is not None
        if probe is not None:
            probe.close()
        return {"applications": apps, "semanticAvailable": pyatspi is not None, "windowFallbackAvailable": window_fallback}
    app_id = slug(req.get("applicationId"))
    entry = find_app(app_id)
    if operation == "inspect":
        return inspect_app(app_id, req.get("limit"))
    if operation == "activate":
        win = x_window(entry, req.get("windowId"))
        return {"applicationId": app_id, "action": "activate", "detail": activate_target(entry, win)}
    if operation == "click":
        encoded = req.get("elementId")
        if encoded:
            detail = semantic_click(entry, str(encoded))
        else:
            win = x_window(entry, req.get("windowId"))
            xr = float(req.get("xRatio", 0.5)); yr = float(req.get("yRatio", 0.5))
            if not (0 <= xr <= 1 and 0 <= yr <= 1):
                raise RuntimeError("Coordinate clicks use window-relative ratios from 0 to 1.")
            b = win["bounds"]
            click_point(entry, win, b["x"] + int(b["width"] * xr), b["y"] + int(b["height"] * yr))
            detail = "Clicked a window-relative point inside the enabled application."
        return {"applicationId": app_id, "action": "click", "detail": detail}
    if operation == "type":
        if not req.get("elementId"):
            raise RuntimeError("Desktop typing requires a semantic element id from inspect; coordinate-based blind typing is blocked.")
        detail = type_text(entry, str(req.get("elementId")), req.get("text", ""), bool(req.get("clearFirst", False)))
        return {"applicationId": app_id, "action": "type", "detail": detail}
    if operation == "key":
        win = x_window(entry, req.get("windowId"))
        return {"applicationId": app_id, "action": "key", "detail": send_shortcut(entry, win, req.get("shortcut", ""))}
    if operation == "scroll":
        win = x_window(entry, req.get("windowId"))
        detail = scroll_window(entry, win, req.get("deltaX", 0), req.get("deltaY", 0), req.get("xRatio"), req.get("yRatio"))
        return {"applicationId": app_id, "action": "scroll", "detail": detail}
    if operation == "screenshot":
        return screenshot(entry, x_window(entry, req.get("windowId")))
    raise RuntimeError("Unsupported desktop control operation.")


if __name__ == "__main__":
    try:
        request = json.loads(sys.stdin.read() or "{}")
        reply(main(request))
    except Exception as exc:
        reply(error=exc)
