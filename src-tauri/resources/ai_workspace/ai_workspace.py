#!/usr/bin/env python3
import base64
import hashlib
import io
import json
import os
import re
import sys
import time

from Xlib import X, XK, display, protocol
from Xlib.ext import xtest
from PIL import Image

try:
    import pyatspi
except Exception:
    pyatspi = None

MAX_SEMANTIC_ELEMENTS = 800
MAX_SEMANTIC_TEXT = 600
SENSITIVE_FIELDS = ("password", "passwd", "passcode", "pin", "secret", "credential", "token", "api key", "otp", "one-time")
SENSITIVE = ("password", "passwd", "passcode", "pin", "secret", "credential", "token", "api key", "sign in", "login")
SHIFT_BASE = {
    "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6", "&": "7", "*": "8", "(": "9", ")": "0",
    "_": "-", "+": "=", "{": "[", "}": "]", "|": "\\", ":": ";", '"': "'", "<": ",", ">": ".", "?": "/", "~": "`",
}
PLAIN_BASE = {
    "-": "minus", "=": "equal", "[": "bracketleft", "]": "bracketright", "\\": "backslash",
    ";": "semicolon", "'": "apostrophe", ",": "comma", ".": "period", "/": "slash", "`": "grave",
}


def reply(result=None, error=None):
    payload = {"ok": error is None}
    payload["result" if error is None else "error"] = result if error is None else str(error)
    print(json.dumps(payload, ensure_ascii=False))


def open_display(name=None):
    try:
        return display.Display(name)
    except Exception as exc:
        raise RuntimeError(f"Could not connect to AI Workspace display: {exc}")


def atom_text(d, window, name):
    try:
        atom = d.intern_atom(name)
        prop = window.get_full_property(atom, X.AnyPropertyType)
        if not prop or prop.value is None:
            return ""
        raw = prop.value
        return raw.decode("utf-8", "replace") if isinstance(raw, bytes) else str(raw)
    except Exception:
        return ""


def atom_int(d, window, name):
    try:
        atom = d.intern_atom(name)
        prop = window.get_full_property(atom, X.AnyPropertyType)
        if not prop or prop.value is None or len(prop.value) == 0:
            return None
        return int(prop.value[0])
    except Exception:
        return None


def window_bounds(window):
    try:
        geom = window.get_geometry()
        root = window.query_tree().root
        # Xlib Window.translate_coords() treats the receiver as the
        # destination window and its first argument as the source window.
        # Translate the client window origin into root coordinates; reversing
        # these produces negative/off-screen positions when windows are tiled.
        translated = root.translate_coords(window, 0, 0)
        return {
            "x": int(translated.x),
            "y": int(translated.y),
            "width": int(geom.width),
            "height": int(geom.height),
        }
    except Exception:
        return None


def window_info(d, window, title=""):
    return {
        "windowId": f"0x{int(window.id):x}",
        "title": title or atom_text(d, window, "_NET_WM_NAME") or str(window.get_wm_name() or ""),
        "pid": atom_int(d, window, "_NET_WM_PID"),
        "bounds": window_bounds(window),
    }


def client_windows(d):
    root = d.screen().root
    atom = d.intern_atom("_NET_CLIENT_LIST")
    prop = root.get_full_property(atom, X.AnyPropertyType)
    ids = list(prop.value) if prop and prop.value is not None else []
    out = []
    for xid in ids:
        try:
            xid = int(xid)
            if xid == 0:
                continue
            win = d.create_resource_object("window", xid)
            title = atom_text(d, win, "_NET_WM_NAME") or str(win.get_wm_name() or "")
            out.append((win, title))
        except Exception:
            pass
    return out


def active_window(d):
    root = d.screen().root
    atom = d.intern_atom("_NET_ACTIVE_WINDOW")
    prop = root.get_full_property(atom, X.AnyPropertyType)
    if prop and prop.value is not None and len(prop.value):
        try:
            xid = int(prop.value[0])
            # EWMH uses XID 0 to mean that no client window is active. Never
            # turn that sentinel into a resource object: querying it creates an
            # asynchronous BadWindow error that can surface during a later sync.
            if xid == 0:
                return None
            return d.create_resource_object("window", xid)
        except Exception:
            return None
    return None


def active_title(d):
    win = active_window(d)
    if win is None:
        return ""
    # The active-window property can briefly point at an X11 window that was
    # destroyed between reading _NET_ACTIVE_WINDOW and resolving its title
    # (for example immediately after Alt+F4). Treat that normal lifecycle race
    # as "no active title" so a following sequence wait can observe the new
    # window count/state instead of failing with Xlib BadWindow.
    try:
        return atom_text(d, win, "_NET_WM_NAME") or str(win.get_wm_name() or "")
    except Exception:
        return ""


def target_window(d, requested=None):
    windows = client_windows(d)
    if requested:
        wanted = str(requested).lower()
        for win, title in windows:
            if f"0x{int(win.id):x}".lower() == wanted:
                return win, title
        raise RuntimeError("That AI Workspace window is no longer available. Inspect the workspace again.")
    active = active_window(d)
    if active is not None:
        title = atom_text(d, active, "_NET_WM_NAME") or str(active.get_wm_name() or "")
        return active, title
    if not windows:
        raise RuntimeError("No controllable window is open inside AI Workspace.")
    return windows[-1]


def clean(value):
    return " ".join(str(value or "").replace("\x00", " ").split())


def semantic_role_name(node):
    try:
        return clean(node.getRoleName())
    except Exception:
        return "unknown"


def semantic_sensitive(node):
    role = semantic_role_name(node).lower()
    label = (
        clean(getattr(node, "name", ""))
        + " "
        + clean(getattr(node, "description", ""))
    ).lower()
    if "password" in role:
        return True
    return any(hint in label for hint in SENSITIVE_FIELDS)


def semantic_states(node):
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


def semantic_actions(node):
    actions = []
    try:
        action = node.queryAction()
        for index in range(action.nActions):
            actions.append(clean(action.getName(index)))
    except Exception:
        pass
    return [item for item in actions if item]


def semantic_bounds(node):
    try:
        component = node.queryComponent()
        ext = component.getExtents(pyatspi.DESKTOP_COORDS)
        if ext.width <= 0 or ext.height <= 0:
            return None
        return {
            "x": int(ext.x),
            "y": int(ext.y),
            "width": int(ext.width),
            "height": int(ext.height),
        }
    except Exception:
        return None


def semantic_text(node, sensitive):
    if sensitive:
        return ""
    try:
        text = node.queryText()
        count = min(int(text.characterCount), MAX_SEMANTIC_TEXT)
        return clean(text.getText(0, count))
    except Exception:
        return ""


def semantic_signature(path, node):
    raw = f"{path}|{semantic_role_name(node)}|{clean(getattr(node, 'name', ''))}"
    return hashlib.sha256(raw.encode("utf-8", "replace")).hexdigest()[:12]


def semantic_element_id(path, node):
    return f"{path}#{semantic_signature(path, node)}"


def inspect_semantics(limit, allowed_pids=None):
    allowed_pids = {
        int(pid) for pid in (allowed_pids or [])
        if isinstance(pid, int) or str(pid).isdigit()
    }
    if pyatspi is None or not os.environ.get("DBUS_SESSION_BUS_ADDRESS"):
        return {
            "semanticAvailable": False,
            "elements": [],
            "truncated": False,
            "message": "Private AI Workspace accessibility is unavailable; use isolated-window coordinates and screenshots.",
        }
    try:
        desktop = pyatspi.Registry.getDesktop(0)
    except Exception:
        return {
            "semanticAvailable": False,
            "elements": [],
            "truncated": False,
            "message": "Private AI Workspace accessibility bus is not ready; use isolated-window coordinates and screenshots.",
        }

    wanted = max(20, min(int(limit or 300), MAX_SEMANTIC_ELEMENTS))
    elements = []
    truncated = False

    def walk(node, path, depth):
        nonlocal truncated
        if len(elements) >= wanted:
            truncated = True
            return
        sensitive = semantic_sensitive(node)
        role = semantic_role_name(node)
        name = clean(getattr(node, "name", ""))
        description = clean(getattr(node, "description", ""))
        actions = semantic_actions(node)
        bounds = semantic_bounds(node)
        states = semantic_states(node)
        text = semantic_text(node, sensitive)
        useful = depth <= 1 or name or description or actions or text or role.lower() in (
            "push button",
            "button",
            "menu",
            "menu item",
            "check box",
            "radio button",
            "text",
            "entry",
            "combo box",
            "page tab",
            "tree item",
            "list item",
            "slider",
        )
        if useful:
            elements.append({
                "id": semantic_element_id(path, node),
                "role": role,
                "name": name,
                "description": description,
                "text": text,
                "states": states,
                "actions": actions,
                "bounds": bounds,
                "sensitive": sensitive,
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
                walk(node.getChildAtIndex(index), f"{path}.{index}", depth + 1)
            except Exception:
                continue

    try:
        app_count = min(int(desktop.childCount), 32)
    except Exception:
        app_count = 0
    for app_index in range(app_count):
        if len(elements) >= wanted:
            truncated = True
            break
        try:
            app = desktop.getChildAtIndex(app_index)
            app_name = clean(getattr(app, "name", "")).lower()
            if "repotunnel" in app_name or app_name == "metacity":
                continue
            app_pid = None
            try:
                app_pid = int(app.get_process_id())
            except Exception:
                try:
                    app_pid = int(app.get_process_id)
                except Exception:
                    pass
            if allowed_pids and app_pid not in allowed_pids:
                continue
            window_count = min(int(app.childCount), 80)
        except Exception:
            continue
        for window_index in range(window_count):
            if len(elements) >= wanted:
                truncated = True
                break
            try:
                walk(
                    app.getChildAtIndex(window_index),
                    f"a{app_index}.w{window_index}",
                    0,
                )
            except Exception:
                continue

    return {
        "semanticAvailable": True,
        "elements": elements,
        "truncated": truncated,
        "message": None if elements else "The private accessibility bus is available, but this app has not exposed semantic elements yet.",
    }


def resolve_semantic_element(encoded):
    if pyatspi is None:
        raise RuntimeError("Private AI Workspace accessibility is unavailable.")
    encoded = str(encoded or "")
    if "#" not in encoded:
        raise RuntimeError("Invalid AI Workspace semantic element identity.")
    path, expected = encoded.rsplit("#", 1)
    parts = path.split(".")
    if len(parts) < 2 or not parts[0].startswith("a") or not parts[1].startswith("w"):
        raise RuntimeError("Invalid AI Workspace semantic element identity.")
    try:
        desktop = pyatspi.Registry.getDesktop(0)
        app = desktop.getChildAtIndex(int(parts[0][1:]))
        node = app.getChildAtIndex(int(parts[1][1:]))
        for part in parts[2:]:
            node = node.getChildAtIndex(int(part))
    except Exception:
        raise RuntimeError(
            "That AI Workspace semantic element is no longer present. Inspect semantics again before acting."
        )
    if semantic_signature(path, node) != expected:
        raise RuntimeError(
            "That AI Workspace semantic element changed since inspection. Inspect semantics again before acting."
        )
    return node


def semantic_allowed_window(node, allowed_window_ids):
    allowed = {str(value) for value in (allowed_window_ids or []) if str(value)}
    if not allowed:
        return None
    bounds = semantic_bounds(node)
    if not bounds:
        raise RuntimeError(
            "RepoTunnel could not prove that this semantic element belongs to the selected AI Workspace app session."
        )
    center_x = bounds["x"] + bounds["width"] // 2
    center_y = bounds["y"] + bounds["height"] // 2
    d = open_display()
    try:
        for win, title in client_windows(d):
            window_id = f"0x{int(win.id):x}"
            if window_id not in allowed:
                continue
            wb = window_bounds(win)
            if not wb:
                continue
            if (
                wb["x"] <= center_x < wb["x"] + wb["width"]
                and wb["y"] <= center_y < wb["y"] + wb["height"]
            ):
                return (window_id, wb)
    finally:
        d.close()
    raise RuntimeError(
        "RepoTunnel blocked a semantic action because the element is outside this AI's app-session windows."
    )


def semantic_click_element(encoded, allowed_window_ids=None):
    node = resolve_semantic_element(encoded)
    allowed_owned = semantic_allowed_window(node, allowed_window_ids)
    actions = semantic_actions(node)
    if actions:
        preferred = ("click", "press", "activate", "open", "toggle", "select")
        try:
            action = node.queryAction()
            names = [clean(action.getName(i)).lower() for i in range(action.nActions)]
            index = next((names.index(name) for name in preferred if name in names), 0)
            if action.doAction(index):
                return {"detail": "Invoked the element accessibility action."}
        except Exception:
            pass

    bounds = semantic_bounds(node)
    if not bounds:
        raise RuntimeError(
            "This AI Workspace element has no clickable accessibility action or visible bounds."
        )
    center_x = bounds["x"] + bounds["width"] // 2
    center_y = bounds["y"] + bounds["height"] // 2
    d = open_display()
    try:
        owned = None
        for win, title in client_windows(d):
            window_id = f"0x{int(win.id):x}"
            if allowed_owned is not None and window_id != allowed_owned[0]:
                continue
            wb = window_bounds(win)
            if not wb:
                continue
            if (
                wb["x"] <= center_x < wb["x"] + wb["width"]
                and wb["y"] <= center_y < wb["y"] + wb["height"]
            ):
                owned = (win, title, wb)
                break
        if owned is None:
            raise RuntimeError(
                "RepoTunnel could not prove that this semantic element is inside an isolated application window."
            )
        win, _, wb = owned
        focus_target(d, f"0x{int(win.id):x}")
        local_x = max(0, min(wb["width"] - 1, center_x - wb["x"]))
        local_y = max(0, min(wb["height"] - 1, center_y - wb["y"]))
        win.warp_pointer(local_x, local_y)
        d.sync()
        xtest.fake_input(d, X.ButtonPress, 1)
        xtest.fake_input(d, X.ButtonRelease, 1)
        d.sync()
    finally:
        d.close()
    return {"detail": "Clicked the verified semantic element inside the isolated application window."}


def semantic_type_element(encoded, text, clear_first, allowed_window_ids=None):
    node = resolve_semantic_element(encoded)
    semantic_allowed_window(node, allowed_window_ids)
    if semantic_sensitive(node):
        raise RuntimeError(
            "RepoTunnel blocks semantic typing into password, PIN, credential, token, and other sensitive fields."
        )
    text = str(text or "")
    if len(text.encode("utf-8")) > 32768:
        raise RuntimeError("AI Workspace semantic typing is limited to 32768 UTF-8 bytes per action.")
    try:
        node.queryComponent().grabFocus()
    except Exception:
        pass
    try:
        editable = node.queryEditableText()
    except Exception:
        raise RuntimeError("That AI Workspace semantic element is not an editable text field.")
    try:
        if clear_first:
            editable.setTextContents(text)
        else:
            offset = 0
            try:
                current = node.queryText()
                offset = int(current.caretOffset)
            except Exception:
                try:
                    offset = int(node.queryText().characterCount)
                except Exception:
                    pass
            editable.insertText(offset, text, len(text))
    except Exception as exc:
        raise RuntimeError(f"The isolated application refused semantic text editing: {exc}")
    return {"characters": len(text), "semantic": True}


def semantic_sequence(steps, allowed_window_ids=None):
    if not isinstance(steps, list) or not 1 <= len(steps) <= 64:
        raise RuntimeError("AI Workspace semantic sequence requires 1..64 steps.")

    started = time.monotonic()
    total_wait_ms = 0
    total_text_bytes = 0
    completed = 0
    results = []

    for index, step in enumerate(steps):
        if time.monotonic() - started > 20.0:
            return {
                "success": False,
                "stepCount": len(steps),
                "completedSteps": completed,
                "failedStep": index,
                "error": (
                    f"SEQUENCE_TIMEOUT: AI Workspace semantic sequence exceeded "
                    f"20000 ms before step {index + 1}."
                ),
                "elapsedMs": round((time.monotonic() - started) * 1000),
                "results": results,
            }
        if not isinstance(step, dict):
            return {
                "success": False,
                "stepCount": len(steps),
                "completedSteps": completed,
                "failedStep": index,
                "error": (
                    f"SEQUENCE_STEP_{index + 1}: AI Workspace semantic sequence "
                    "step must be an object."
                ),
                "elapsedMs": round((time.monotonic() - started) * 1000),
                "results": results,
            }
        operation = str(step.get("operation") or "")
        try:
            if operation == "wait":
                wait_ms = int(step.get("waitMs") or 0)
                if wait_ms < 0 or wait_ms > 2000:
                    raise RuntimeError("Wait must be between 0 and 2000 ms.")
                total_wait_ms += wait_ms
                if total_wait_ms > 10000:
                    raise RuntimeError("Total sequence wait time exceeds 10000 ms.")
                if wait_ms:
                    time.sleep(wait_ms / 1000.0)
                result = {"waitedMs": wait_ms}
            elif operation == "click":
                result = semantic_click_element(
                    step.get("elementId"),
                    allowed_window_ids,
                )
            elif operation == "type":
                text = str(step.get("text") or "")
                total_text_bytes += len(text.encode("utf-8"))
                if total_text_bytes > 131072:
                    raise RuntimeError("Total sequence typed text exceeds 131072 bytes.")
                result = semantic_type_element(
                    step.get("elementId"),
                    text,
                    bool(step.get("clearFirst", False)),
                    allowed_window_ids,
                )
            else:
                raise RuntimeError(
                    f"Unsupported AI Workspace semantic sequence operation: {operation or '<empty>'}."
                )
            completed += 1
            results.append({
                "index": index,
                "operation": operation,
                "result": result,
            })
        except Exception as exc:
            raise RuntimeError(f"SEQUENCE_STEP_{index + 1}: {exc}")

    return {
        "stepCount": len(steps),
        "completedSteps": completed,
        "elapsedMs": round((time.monotonic() - started) * 1000),
        "results": results,
    }


def inspect_windows(limit=300, allowed_pids=None):
    allowed_pids = {
        int(pid) for pid in (allowed_pids or [])
        if isinstance(pid, int) or str(pid).isdigit()
    }
    d = open_display()
    try:
        active = active_window(d)
        active_id = int(active.id) if active is not None else None
        active_name = active_title(d)
        windows = []
        for win, title in client_windows(d):
            info = window_info(d, win, title)
            if allowed_pids and info.get("pid") not in allowed_pids:
                continue
            info["active"] = int(win.id) == active_id
            windows.append(info)
        if allowed_pids and not any(item.get("active") for item in windows):
            active_id = None
            active_name = ""
    finally:
        d.close()

    semantic = inspect_semantics(limit, allowed_pids)
    return {
        "activeWindowId": f"0x{active_id:x}" if active_id is not None else None,
        "activeTitle": active_name,
        "windows": windows,
        "semanticAvailable": bool(semantic.get("semanticAvailable")),
        "elements": semantic.get("elements") or [],
        "truncated": bool(semantic.get("truncated")),
        "message": semantic.get("message"),
    }


def ensure_non_sensitive(d):
    title = active_title(d).lower()
    if any(item in title for item in SENSITIVE):
        raise RuntimeError("RepoTunnel blocked typing into a credential or authentication window inside AI Workspace.")


def hide_host(req):
    title_token = str(req.get("titleToken") or "RepoTunnel AI Workspace")
    d = open_display(req.get("displayName"))
    try:
        matches = []
        for _ in range(20):
            matches = [(w, title) for w, title in client_windows(d) if title_token.lower() in title.lower()]
            if matches:
                break
            time.sleep(0.05)
        if not matches:
            return {"hidden": False, "message": "Xephyr host window was not visible after retrying."}
        hidden = 0
        for win, _ in matches:
            try:
                win.configure(x=-30000, y=-30000)
                win.iconify(d.get_default_screen())
                hidden += 1
            except Exception:
                try:
                    win.configure(x=-30000, y=-30000)
                    hidden += 1
                except Exception:
                    pass
        d.sync()
        return {"hidden": hidden > 0, "count": hidden}
    finally:
        d.close()


def root_size(d):
    geom = d.screen().root.get_geometry()
    return int(geom.width), int(geom.height)


def ensure_window_visible(d, win):
    bounds = window_bounds(win)
    if not bounds:
        return None
    screen_width, screen_height = root_size(d)
    left = max(0, bounds["x"])
    top = max(0, bounds["y"])
    right = min(screen_width, bounds["x"] + bounds["width"])
    bottom = min(screen_height, bounds["y"] + bounds["height"])
    visible_width = max(0, right - left)
    visible_height = max(0, bottom - top)
    area = max(1, bounds["width"] * bounds["height"])
    visible_area = visible_width * visible_height
    if visible_area * 4 < area * 3:
        x = max(0, (screen_width - min(bounds["width"], screen_width)) // 2)
        y = max(0, (screen_height - min(bounds["height"], screen_height)) // 2)
        try:
            win.configure(x=x, y=y)
            d.sync()
            time.sleep(0.04)
            bounds = window_bounds(win) or bounds
        except Exception:
            pass
    return bounds


def frame(req):
    d = open_display()
    try:
        window_id = req.get("windowId")
        if window_id:
            target, _ = target_window(d, window_id)
            geom = target.get_geometry()
            width, height = int(geom.width), int(geom.height)
            raw = target.get_image(0, 0, width, height, X.ZPixmap, 0xFFFFFFFF)
        else:
            target = d.screen().root
            width, height = root_size(d)
            raw = target.get_image(0, 0, width, height, X.ZPixmap, 0xFFFFFFFF)
        if raw is None or not raw.data:
            raise RuntimeError("AI Workspace display did not return pixels yet.")
        bpp = len(raw.data) // max(1, width * height)
        if bpp >= 4:
            image = Image.frombytes("RGB", (width, height), raw.data, "raw", "BGRX")
        elif bpp == 3:
            image = Image.frombytes("RGB", (width, height), raw.data, "raw", "BGR")
        else:
            raise RuntimeError("Unsupported virtual display pixel format.")
        max_width = int(req.get("maxWidth") or width)
        if 320 <= max_width < width:
            new_height = max(1, round(height * (max_width / width)))
            image = image.resize((max_width, new_height), Image.Resampling.BILINEAR)
        fmt = str(req.get("format") or "jpeg").lower()
        stream = io.BytesIO()
        if fmt == "png":
            image.save(stream, format="PNG", compress_level=3)
            mime = "image/png"
        else:
            image.save(stream, format="JPEG", quality=max(45, min(int(req.get("quality") or 72), 90)), optimize=True)
            mime = "image/jpeg"
        data = stream.getvalue()
        return {
            "mimeType": mime,
            "width": int(image.width),
            "height": int(image.height),
            "sourceWidth": width,
            "sourceHeight": height,
            "sizeBytes": len(data),
            "data": base64.b64encode(data).decode("ascii"),
            "activeTitle": active_title(d),
            "windowId": window_id,
        }
    finally:
        d.close()


def focus_target(d, requested=None):
    win, title = target_window(d, requested)
    bounds = ensure_window_visible(d, win)
    root = d.screen().root
    atom = d.intern_atom("_NET_ACTIVE_WINDOW")
    event = protocol.event.ClientMessage(window=win, client_type=atom, data=(32, [2, int(time.time()), 0, 0, 0]))
    root.send_event(event, event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
    try:
        win.set_input_focus(X.RevertToParent, X.CurrentTime)
    except Exception:
        pass
    d.sync()
    time.sleep(0.02)
    return win, title, window_bounds(win) or bounds


def place_window(req):
    window_id = req.get("windowId")
    if not window_id:
        raise RuntimeError("AI Workspace window placement requires windowId.")
    d = open_display()
    try:
        win, _ = target_window(d, window_id)
        screen_width, screen_height = root_size(d)
        x = max(0, min(int(req.get("x") or 0), max(0, screen_width - 1)))
        y = max(0, min(int(req.get("y") or 0), max(0, screen_height - 1)))
        width = max(240, min(int(req.get("width") or screen_width), screen_width))
        height = max(160, min(int(req.get("height") or screen_height), screen_height))
        if x + width > screen_width:
            width = max(240, screen_width - x)
        if y + height > screen_height:
            height = max(160, screen_height - y)
        win.configure(x=x, y=y, width=width, height=height)
        d.sync()
        time.sleep(0.04)
        return {
            "windowId": f"0x{int(win.id):x}",
            "bounds": window_bounds(win),
        }
    finally:
        d.close()


def activate(req):
    d = open_display()
    try:
        win, title, bounds = focus_target(d, req.get("windowId"))
        return {"activated": True, "title": title, "windowId": f"0x{int(win.id):x}", "bounds": bounds}
    finally:
        d.close()


def pointer_action(req):
    action = req.get("action")
    d = open_display()
    try:
        width, height = root_size(d)
        xr = max(0.0, min(float(req.get("xRatio", 0.5)), 1.0))
        yr = max(0.0, min(float(req.get("yRatio", 0.5)), 1.0))
        window_id = req.get("windowId")
        bounds = None
        if window_id:
            win, _, bounds = focus_target(d, window_id)
            if not bounds:
                raise RuntimeError("Could not resolve the requested AI Workspace window bounds.")
            local_x = min(bounds["width"] - 1, max(0, int(bounds["width"] * xr)))
            local_y = min(bounds["height"] - 1, max(0, int(bounds["height"] * yr)))
            win.warp_pointer(local_x, local_y)
            x = bounds["x"] + local_x
            y = bounds["y"] + local_y
        else:
            x = max(0, min(width - 1, int(width * xr)))
            y = max(0, min(height - 1, int(height * yr)))
            d.screen().root.warp_pointer(x, y)
        d.sync()
        if action == "click":
            count = max(1, min(int(req.get("count") or 1), 3))
            for _ in range(count):
                xtest.fake_input(d, X.ButtonPress, 1)
                xtest.fake_input(d, X.ButtonRelease, 1)
                d.sync()
                time.sleep(0.07)
        elif action == "scroll":
            dy = int(req.get("deltaY") or 0)
            dx = int(req.get("deltaX") or 0)
            for delta, negative, positive in ((dy, 4, 5), (dx, 6, 7)):
                button = positive if delta > 0 else negative
                for _ in range(min(30, max(0, (abs(delta) + 119) // 120))):
                    xtest.fake_input(d, X.ButtonPress, button)
                    xtest.fake_input(d, X.ButtonRelease, button)
        d.sync()
        return {"x": x, "y": y, "action": action, "windowId": window_id, "windowBounds": bounds}
    finally:
        d.close()


def parse_shortcut(value):
    value = str(value or "").strip()
    if not value or len(value) > 80:
        raise RuntimeError("Enter a short keyboard shortcut such as Ctrl+S, Escape, Enter, or F5.")
    parts = [part.strip() for part in value.split("+") if part.strip()]
    mods, key = parts[:-1], parts[-1]
    mod_map = {"ctrl": "Control_L", "control": "Control_L", "alt": "Alt_L", "shift": "Shift_L", "super": "Super_L", "meta": "Super_L"}
    mapped = []
    for mod in mods:
        if mod.lower() not in mod_map:
            raise RuntimeError("Unsupported shortcut modifier.")
        mapped.append(mod_map[mod.lower()])
    named = {"enter": "Return", "return": "Return", "esc": "Escape", "escape": "Escape", "tab": "Tab", "space": "space", "backspace": "BackSpace", "delete": "Delete", "up": "Up", "down": "Down", "left": "Left", "right": "Right", "home": "Home", "end": "End", "pageup": "Page_Up", "pagedown": "Page_Down"}
    key = named.get(key.lower(), key)
    if len(key) > 1 and key not in named.values() and not re.fullmatch(r"F(?:[1-9]|1[0-2])", key, re.I):
        raise RuntimeError("That key is not in RepoTunnel's AI Workspace shortcut allowlist.")
    return mapped, key


def keycode(d, name):
    sym = XK.string_to_keysym(name)
    code = d.keysym_to_keycode(sym)
    if not code:
        raise RuntimeError(f"Could not resolve key {name}.")
    return code


def shortcut(req):
    d = open_display()
    try:
        if req.get("windowId"):
            focus_target(d, req.get("windowId"))
        mods, key = parse_shortcut(req.get("shortcut"))
        codes = []
        for mod in mods:
            code = keycode(d, mod)
            codes.append(code)
            xtest.fake_input(d, X.KeyPress, code)
        code = keycode(d, key)
        xtest.fake_input(d, X.KeyPress, code)
        xtest.fake_input(d, X.KeyRelease, code)
        for code in reversed(codes):
            xtest.fake_input(d, X.KeyRelease, code)
        d.sync()
        return {"shortcut": req.get("shortcut")}
    finally:
        d.close()


def type_text(req):
    text = str(req.get("text") or "")
    if len(text) > 32768:
        raise RuntimeError("AI Workspace typing is limited to 32,768 characters per action. Send larger documents in additional type actions; total document length is not limited.")
    d = open_display()
    try:
        if req.get("windowId"):
            focus_target(d, req.get("windowId"))
        ensure_non_sensitive(d)
        shift_code = keycode(d, "Shift_L")

        # XTest can enqueue key events much faster than real GUI applications can
        # consume them. Large unthrottled bursts may therefore be acknowledged by
        # X11 while applications such as LibreOffice, IDEs, and terminals receive
        # only a prefix. Pace every app through the same bounded batch delivery
        # path so long text remains reliable without app-specific workarounds.
        batch_chars = 12
        batch_delay = 0.006
        delivered = 0

        for ch in text:
            if ch == "\n":
                base, shifted = "Return", False
            elif ch == "\t":
                base, shifted = "Tab", False
            elif ch == " ":
                base, shifted = "space", False
            elif ch in SHIFT_BASE:
                base, shifted = SHIFT_BASE[ch], True
            elif ch.isascii() and ch.isprintable():
                base, shifted = ch.lower() if ch.isalpha() else ch, ch.isalpha() and ch.isupper()
            else:
                raise RuntimeError("AI Workspace typing currently supports ASCII text only.")

            base = PLAIN_BASE.get(base, base)
            code = keycode(d, base)
            if shifted:
                xtest.fake_input(d, X.KeyPress, shift_code)
            xtest.fake_input(d, X.KeyPress, code)
            xtest.fake_input(d, X.KeyRelease, code)
            if shifted:
                xtest.fake_input(d, X.KeyRelease, shift_code)

            delivered += 1
            if delivered % batch_chars == 0 or ch in ("\n", "\t"):
                d.sync()
                time.sleep(batch_delay)

        d.sync()
        # Give the target application's event loop one final scheduling window
        # before the helper exits, especially after multi-thousand-character input.
        if text:
            time.sleep(batch_delay)
        return {
            "characters": len(text),
            "batches": (len(text) + batch_chars - 1) // batch_chars if text else 0,
            "paced": True,
        }
    finally:
        d.close()


def wait_step(req):
    wait_ms = max(0, min(int(req.get("waitMs") or 0), 2000))
    timeout_ms = max(wait_ms, min(int(req.get("timeoutMs") or 3000), 5000))
    title_contains = str(req.get("titleContains") or "").strip().lower()
    min_windows = req.get("windowCountAtLeast")
    max_windows = req.get("windowCountAtMost")
    if len(title_contains) > 200:
        raise RuntimeError("AI Workspace wait title is too long.")
    if min_windows is not None:
        min_windows = max(0, min(int(min_windows), 100))
    if max_windows is not None:
        max_windows = max(0, min(int(max_windows), 100))
    if min_windows is not None and max_windows is not None and min_windows > max_windows:
        raise RuntimeError("AI Workspace wait window bounds are invalid.")

    started = time.monotonic()
    if wait_ms:
        time.sleep(wait_ms / 1000.0)

    # A plain bounded delay needs no X11 polling.
    if not title_contains and min_windows is None and max_windows is None:
        return {"waitedMs": round((time.monotonic() - started) * 1000), "matched": True}

    d = open_display()
    try:
        deadline = started + (timeout_ms / 1000.0)
        while True:
            try:
                title = active_title(d)
                count = len(client_windows(d))
                matched = True
                if title_contains and title_contains not in title.lower():
                    matched = False
                if min_windows is not None and count < min_windows:
                    matched = False
                if max_windows is not None and count > max_windows:
                    matched = False
                if matched:
                    return {
                        "waitedMs": round((time.monotonic() - started) * 1000),
                        "matched": True,
                        "activeTitle": title,
                        "windowCount": count,
                    }
                d.sync()
            except Exception:
                # When the last app window closes, RepoTunnel may tear down the
                # nested X display before this polling connection finishes its
                # next property read. For an explicit wait-for-zero-windows, a
                # disappearing display is exactly the requested terminal state.
                if max_windows == 0 and min_windows is None and not title_contains:
                    return {
                        "waitedMs": round((time.monotonic() - started) * 1000),
                        "matched": True,
                        "activeTitle": "",
                        "windowCount": 0,
                        "displayClosed": True,
                    }
                if time.monotonic() >= deadline:
                    raise
            if time.monotonic() >= deadline:
                raise RuntimeError("AI Workspace wait condition timed out.")
            time.sleep(0.04)
    finally:
        d.close()


def sequence(req):
    steps = req.get("steps")
    if not isinstance(steps, list) or not steps:
        raise RuntimeError("AI Workspace sequence requires at least one step.")
    if len(steps) > 64:
        raise RuntimeError("AI Workspace sequence is limited to 64 steps per request.")

    allowed_window_ids = {
        str(value) for value in (req.get("allowedWindowIds") or []) if str(value)
    }
    default_window_id = str(req.get("windowId") or "")
    if allowed_window_ids and default_window_id not in allowed_window_ids:
        raise RuntimeError(
            "RepoTunnel blocked an AI Workspace sequence default window outside this app session."
        )

    total_text = 0
    for step in steps:
        if not isinstance(step, dict):
            raise RuntimeError("Every AI Workspace sequence step must be an object.")
        op = str(step.get("operation") or "")
        if op not in ("activate", "click", "key", "type", "scroll", "wait"):
            raise RuntimeError(f"Unsupported AI Workspace sequence operation: {op or '<empty>'}.")
        if op == "type":
            total_text += len(str(step.get("text") or ""))
    if total_text > 131072:
        raise RuntimeError("AI Workspace sequence text is limited to 131,072 characters per request.")

    started = time.monotonic()
    results = []
    for index, step in enumerate(steps):
        if time.monotonic() - started > 20.0:
            raise RuntimeError("AI Workspace sequence exceeded its 20 second execution budget.")
        item = dict(step)
        op = item.get("operation")
        item["windowId"] = item.get("windowId") or req.get("windowId")
        if (
            allowed_window_ids
            and item.get("windowId")
            and str(item.get("windowId")) not in allowed_window_ids
        ):
            raise RuntimeError(
                f"SEQUENCE_STEP_{index + 1}: RepoTunnel blocked a window outside this AI's app session."
            )
        result = wait_step(item) if op == "wait" else main(item)
        results.append({"index": index, "operation": op, "result": result})

    return {
        "stepCount": len(steps),
        "elapsedMs": round((time.monotonic() - started) * 1000),
        "results": results,
    }


def main(req):
    op = req.get("operation")
    if op == "hostHide":
        return hide_host(req)
    if op == "ping":
        allowed_pids = {
            int(pid) for pid in (req.get("allowedPids") or [])
            if isinstance(pid, int) or str(pid).isdigit()
        }
        d = open_display()
        try:
            width, height = root_size(d)
            windows = client_windows(d)
            if allowed_pids:
                windows = [
                    (win, title) for win, title in windows
                    if window_info(d, win, title).get("pid") in allowed_pids
                ]
            return {"width": width, "height": height, "windowCount": len(windows), "activeTitle": active_title(d)}
        finally:
            d.close()
    if op == "frame":
        return frame(req)
    if op == "inspect":
        return inspect_windows(req.get("limit") or 300, req.get("allowedPids"))
    if op == "semanticClick":
        return semantic_click_element(
            req.get("elementId"),
            req.get("allowedWindowIds"),
        )
    if op == "semanticType":
        return semantic_type_element(
            req.get("elementId"),
            req.get("text") or "",
            bool(req.get("clearFirst", False)),
            req.get("allowedWindowIds"),
        )
    if op == "semanticSequence":
        return semantic_sequence(
            req.get("steps"),
            req.get("allowedWindowIds"),
        )
    if op == "placeWindow":
        return place_window(req)
    if op == "activate":
        return activate(req)
    if op in ("click", "scroll"):
        req = dict(req)
        req["action"] = op
        return pointer_action(req)
    if op == "key":
        return shortcut(req)
    if op == "type":
        return type_text(req)
    if op == "sequence":
        return sequence(req)
    raise RuntimeError("Unsupported AI Workspace operation.")


if __name__ == "__main__":
    try:
        reply(main(json.loads(sys.stdin.read() or "{}")))
    except Exception as exc:
        reply(error=exc)
