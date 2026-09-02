#!/usr/bin/env python3
import base64
import io
import json
import os
import re
import sys
import time

from Xlib import X, XK, display, protocol
from Xlib.ext import xtest
from PIL import Image

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
        translated = window.translate_coords(root, 0, 0)
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


def inspect_windows():
    d = open_display()
    try:
        active = active_window(d)
        active_id = int(active.id) if active is not None else None
        windows = []
        for win, title in client_windows(d):
            info = window_info(d, win, title)
            info["active"] = int(win.id) == active_id
            windows.append(info)
        return {
            "activeWindowId": f"0x{active_id:x}" if active_id is not None else None,
            "activeTitle": active_title(d),
            "windows": windows,
            "semanticAvailable": False,
            "message": "Use window-relative coordinates for precise isolated-app control. Semantic accessibility is not currently exposed by this isolated app.",
        }
    finally:
        d.close()


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
        d = open_display()
        try:
            width, height = root_size(d)
            return {"width": width, "height": height, "windowCount": len(client_windows(d)), "activeTitle": active_title(d)}
        finally:
            d.close()
    if op == "frame":
        return frame(req)
    if op == "inspect":
        return inspect_windows()
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
