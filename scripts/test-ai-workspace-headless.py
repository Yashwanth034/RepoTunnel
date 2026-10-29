#!/usr/bin/env python3
"""Real off-screen Linux AI Workspace helper acceptance; never uses the host desktop.

Starts a private Xvfb parent, a nested Xephyr display, and two synthetic X11
windows. Exercises actual pixel capture and X11 owner checks without launching
RepoTunnel, an IDE, the user's apps, or interacting with their DISPLAY.
Run: python3 scripts/test-ai-workspace-headless.py
Requires: Xvfb, Xephyr, xauth, python-xlib, Pillow, Metacity, Zenity.
All GUI processes are synthetic test fixtures or isolated GTK dialogs on the
nested display; no personal desktop applications are launched.
"""
import base64
import io
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import sys
import secrets
import tempfile
import time

from Xlib import X, display, Xatom
from PIL import Image

HELPER = Path(__file__).resolve().parents[1] / "src-tauri/resources/ai_workspace/ai_workspace.py"
TEST_XAUTHORITY = None
DEFAULT_AUTH = object()


def require(condition, message):
    if not condition:
        raise AssertionError(message)


class HeadlessDisplays:
    def __init__(self):
        self.processes = []
        self.devnull = open(os.devnull, "wb")

    def start(self, binary, args, env=None):
        read_fd, write_fd = os.pipe()
        try:
            child = subprocess.Popen(
                [binary, *args, "-displayfd", str(write_fd)],
                stdout=self.devnull,
                stderr=self.devnull,
                stdin=subprocess.DEVNULL,
                env=env,
                pass_fds=(write_fd,),
                start_new_session=True,
            )
            self.processes.append(child)
        finally:
            os.close(write_fd)
        try:
            ready, _, _ = select.select([read_fd], [], [], 12)
            number = os.read(read_fd, 32).strip() if ready else b""
        finally:
            os.close(read_fd)
        require(number.isdigit(), f"{binary} did not create an isolated X display")
        return ":" + number.decode("ascii")

    def start_authenticated(self, binary, args, parent_display, authority_file):
        # A numbered MIT-MAGIC-COOKIE entry must exist *before* Xephyr
        # starts. Random high displays avoid interacting with any host X
        # session; refuse known occupied socket or lock paths.
        for _ in range(32):
            number = 1000 + secrets.randbelow(7000)
            if not Path(f"/tmp/.X11-unix/X{number}").exists() and not Path(f"/tmp/.X{number}-lock").exists():
                break
        else:
            raise RuntimeError("No unused private X11 test display number found")
        child_display = f":{number}"
        cookie = secrets.token_hex(16)
        auth_environment = dict(os.environ, HOME=str(authority_file.parent))
        auth_environment.pop("XAUTHORITY", None)
        add = subprocess.run(
            ["/usr/bin/xauth", "-f", str(authority_file), "add",
             child_display, ".", cookie],
            env=auth_environment, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, timeout=8, check=False,
        )
        require(add.returncode == 0, "Could not generate the private Xephyr Xauthority record")
        authority_file.chmod(0o600)
        child = subprocess.Popen(
            [binary, child_display, *args, "-auth", str(authority_file)],
            env=dict(auth_environment, DISPLAY=parent_display),
            stdin=subprocess.DEVNULL, stdout=self.devnull, stderr=self.devnull,
            start_new_session=True,
        )
        self.processes.append(child)
        for _ in range(100):
            require(child.poll() is None, "Private authenticated Xephyr exited before becoming ready")
            try:
                d = display.Display(child_display)
                d.close()
                return child_display
            except Exception:
                time.sleep(0.1)
        raise RuntimeError("Private authenticated Xephyr did not become ready")

    def spawn_isolated(self, binary, args, env, capture_stderr=False):
        # Only call with DISPLAY pointing to the new Xephyr and its private
        # Xauthority. Never inherit the user's host desktop environment.
        child = subprocess.Popen(
            [binary, *args], stdin=subprocess.DEVNULL,
            stdout=self.devnull, stderr=subprocess.PIPE if capture_stderr else self.devnull,
            env=env, start_new_session=True,
        )
        self.processes.append(child)
        return child

    def stop_isolated(self, child):
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGTERM)
        try:
            child.wait(timeout=4)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=4)

    def close(self):
        for child in reversed(self.processes):
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
        for child in reversed(self.processes):
            try:
                child.wait(timeout=4)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=4)
        self.devnull.close()


def fixture_windows(d):
    root = d.screen().root
    owned_pid = os.getpid()
    foreign_pid = owned_pid + 50_000

    def window(title, pid, x):
        win = root.create_window(
            x, 40, 260, 180, 0, d.screen().root_depth,
            X.InputOutput, X.CopyFromParent,
            background_pixel=d.screen().white_pixel,
            event_mask=X.ExposureMask,
        )
        win.set_wm_name(title)
        win.change_property(d.intern_atom("_NET_WM_NAME"), d.intern_atom("UTF8_STRING"), 8,
                            title.encode("utf-8"))
        win.change_property(d.intern_atom("_NET_WM_PID"), Xatom.CARDINAL, 32, [pid])
        win.map()
        return win

    owned = window("RepoTunnel Synthetic Owned", owned_pid, 30)
    foreign = window("RepoTunnel Synthetic Foreign", foreign_pid, 380)
    root.change_property(d.intern_atom("_NET_CLIENT_LIST"), Xatom.WINDOW, 32,
                         [owned.id, foreign.id])
    root.change_property(d.intern_atom("_NET_ACTIVE_WINDOW"), Xatom.WINDOW, 32, [owned.id])
    d.sync()
    return owned, foreign, owned_pid, foreign_pid


def helper_request(child_display, request, auth_path=DEFAULT_AUTH):
    environment = dict(os.environ, DISPLAY=child_display)
    # Never inherit the user's authentication material or desktop session.
    environment.pop("DBUS_SESSION_BUS_ADDRESS", None)
    environment.pop("XAUTHORITY", None)
    effective_auth = TEST_XAUTHORITY if auth_path is DEFAULT_AUTH else auth_path
    if effective_auth is not None:
        environment["XAUTHORITY"] = str(effective_auth)
    result = subprocess.run(
        [sys.executable, str(HELPER)],
        input=json.dumps(request), capture_output=True, text=True,
        timeout=12, env=environment, check=False,
    )
    require(result.returncode == 0, f"helper crashed: {result.stderr[-1000:]}")
    try:
        return json.loads(result.stdout)
    except ValueError as error:
        raise AssertionError(f"helper emitted invalid JSON: {result.stdout[:500]}") from error


def accepted(child_display, request):
    result = helper_request(child_display, request)
    require(result.get("ok") is True, f"unexpected rejection: {result}")
    return result["result"]


def denied(child_display, request):
    result = helper_request(child_display, request)
    require(result.get("ok") is False, f"unsafe cross-agent operation accepted: {request['operation']}")
    return str(result["error"])


def exercise_task_scoped_gui_tmpfs(servers, child_display, authority, env, private, session_bus_address):
    """Native GTK smoke for the task-scoped bwrap mount plan with private /tmp.

    This does not create a Tauri AppHandle. The task, peer, and real project are
    temporary fixtures, and the GTK window remains on the isolated Xephyr.
    """
    from Xlib import display as xdisplay

    project = Path(private.name) / "scope-project"
    own = Path(private.name) / "scope-private"
    peer = Path(private.name) / "scope-foreign"
    for directory in (project / "src", own, peer):
        directory.mkdir(parents=True, exist_ok=True)
    (project / "src" / "draft").write_text("unchanged-host")
    (project / "forbidden").write_text("read-only")
    (own / "draft").write_text("original-private")
    (peer / "secret").write_text("other-task-private")

    socket = Path("/tmp/.X11-unix") / f"X{child_display[1:]}"
    require(socket.exists(), "private Xephyr must have a live Unix socket")
    command = [
        "--ro-bind", "/", "/",
        "--dev", "/dev",
        "--tmpfs", "/tmp",
        "--dir", "/tmp/.X11-unix",
        "--ro-bind", str(socket), str(socket),
        "--ro-bind", str(authority), str(authority),
        "--ro-bind", str(project), str(project),
        "--bind", str(own / "draft"), str(project / "src" / "draft"),
        "--", "/bin/sh", "-c",
        "printf 'private-gtk-write' > \"$1\"; "
        "if test -e \"$2\"; then exit 72; fi; "
        "if printf blocked > \"$3\" 2>/dev/null; then exit 73; fi; "
        "/usr/bin/gdbus call --address \"$DBUS_SESSION_BUS_ADDRESS\" "
        "--dest org.freedesktop.DBus --object-path /org/freedesktop/DBus "
        "--method org.freedesktop.DBus.ListNames >/dev/null || exit 74; "
        "exec /usr/bin/zenity --info --title=RepoTunnelTaskTmpfs --text=isolated",
        "sh", str(project / "src" / "draft"), str(peer / "secret"), str(project / "forbidden"),
    ]
    if session_bus_address.startswith("unix:path="):
        bus_socket = Path(session_bus_address.removeprefix("unix:path=").split(",")[0])
        require(bus_socket.exists(), "the private D-Bus filesystem socket disappeared")
        insert_at = command.index("--")
        command[insert_at:insert_at] = ["--ro-bind", str(bus_socket), str(bus_socket)]
    env = dict(env, DBUS_SESSION_BUS_ADDRESS=session_bus_address)
    process = servers.spawn_isolated("/usr/bin/bwrap", command, env, capture_stderr=True)
    try:
        deadline = time.monotonic() + 12
        opened = False
        while time.monotonic() < deadline:
            if process.poll() is not None:
                detail = process.stderr.read(4096).decode("utf-8", "replace") if process.stderr else ""
                raise AssertionError(f"private-tmpfs GTK app exited ({process.returncode}): {detail}")
            d = xdisplay.Display(child_display)
            try:
                inspection = accepted(child_display, {"operation": "inspect"})
                opened = any(
                    "RepoTunnelTaskTmpfs" in window.get("title", "")
                    for window in inspection["windows"]
                )
            finally:
                d.close()
            if opened:
                break
            time.sleep(0.1)
        require(opened, "GTK app could not use the selectively exposed Xephyr socket")
        require((own / "draft").read_text() == "private-gtk-write",
                "task GUI did not write to its private source overlay")
        require((project / "src" / "draft").read_text() == "unchanged-host",
                "task GUI changed the real source project")
        require((project / "forbidden").read_text() == "read-only",
                "task GUI mutated an unauthorized project file")
        require((peer / "secret").read_text() == "other-task-private",
                "task GUI modified another task's preserved files")
        print("PASS native GTK task app with private tmpfs, scoped source mount and isolated X11 socket")
    finally:
        servers.stop_isolated(process)


def exercise_real_gui_lifecycle(servers, parent, private):
    """Launch, isolate, terminate and restart actual GTK app processes.

    This exercises the Xephyr + window-manager + native app boundary, not
    RepoTunnel's Tauri AppHandle/session persistence, which needs a separate
    end-to-end crash/restart harness.
    """
    global TEST_XAUTHORITY
    authority = Path(private.name) / "real-app-xephyr.xauth"
    TEST_XAUTHORITY = str(authority)
    os.environ["XAUTHORITY"] = TEST_XAUTHORITY
    child_display = servers.start_authenticated(
        "/usr/bin/Xephyr", ["-screen", "800x600", "-nolisten", "tcp"],
        parent, authority,
    )
    env = dict(
        os.environ,
        DISPLAY=child_display,
        XAUTHORITY=str(authority),
        GDK_BACKEND="x11",
        NO_AT_BRIDGE="1",
    )
    env.pop("DBUS_SESSION_BUS_ADDRESS", None)
    env.pop("AT_SPI_BUS_ADDRESS", None)
    wm = servers.spawn_isolated("/usr/bin/metacity", ["--sm-disable", "--replace"], env)

    def launch(title):
        return servers.spawn_isolated(
            "/usr/bin/zenity", ["--info", f"--title={title}", "--text=isolated test"],
            env,
        )

    def wait_for_window(process, want_window):
        deadline = time.monotonic() + 12
        last = None
        while time.monotonic() < deadline:
            if want_window:
                require(process.poll() is None, "isolated GUI process exited before opening a window")
            reply = accepted(child_display, {
                "operation": "inspect", "allowedPids": [process.pid],
            })
            windows = reply["windows"]
            last = windows
            if bool(windows) == want_window:
                return windows
            time.sleep(0.1)
        raise AssertionError(
            f"native GUI window state never became {want_window} for PID "
            f"{process.pid}: {last}"
        )

    first = launch("RepoTunnel Isolated A")
    second = launch("RepoTunnel Isolated B")
    try:
        first_windows = wait_for_window(first, True)
        second_windows = wait_for_window(second, True)
        require(wm.poll() is None, "isolated window manager exited during GUI launch")
        first_id = first_windows[0]["windowId"]
        second_id = second_windows[0]["windowId"]
        require(first_id != second_id, "two native app sessions shared a window ID")
        first_frame = accepted(child_display, {
            "operation": "frame", "windowId": first_id,
            "allowedPids": [first.pid], "format": "png",
        })
        require(first_frame["mimeType"] == "image/png", "real GTK window capture failed")
        reason = denied(child_display, {
            "operation": "frame", "windowId": second_id,
            "allowedPids": [first.pid], "format": "png",
        })
        require("current process" in reason, "a real foreign GTK window was readable")
        print("PASS two real GTK app processes: independent window/PID scoping and capture")

        servers.stop_isolated(first)
        wait_for_window(first, False)
        wait_for_window(second, True)
        print("PASS real native app exit: stopped app disappears, other app survives")

        restarted = launch("RepoTunnel Isolated Restart")
        try:
            require(restarted.pid != first.pid, "the replacement process reused a live PID")
            replacement = wait_for_window(restarted, True)
            require(replacement, "replacement GUI process has no owned window")
            wait_for_window(second, True)
            require(wm.poll() is None, "isolated window manager died before app restart")
            print("PASS real native app restart: new PID/window and surviving peer")
        finally:
            servers.stop_isolated(restarted)
            wait_for_window(restarted, False)
        bus = subprocess.Popen(
            ["/usr/bin/dbus-daemon", "--session", "--nofork", "--print-address=1"],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=servers.devnull, env=env, start_new_session=True,
        )
        servers.processes.append(bus)
        ready, _, _ = select.select([bus.stdout], [], [], 8)
        require(bool(ready), "isolated D-Bus test session failed to provide an address")
        session_bus_address = bus.stdout.readline().decode("utf-8", "replace").strip()
        require(session_bus_address.startswith("unix:"), "isolated D-Bus returned a nonlocal address")
        exercise_task_scoped_gui_tmpfs(
            servers, child_display, authority, env, private, session_bus_address
        )
        servers.stop_isolated(bus)
    finally:
        servers.stop_isolated(first)
        servers.stop_isolated(second)
        servers.stop_isolated(wm)
    print("PASS real GUI lifecycle: all managed app/WM processes stopped")


def run():
    global TEST_XAUTHORITY
    servers = HeadlessDisplays()
    private = tempfile.TemporaryDirectory(prefix="repotunnel-aiw-x11-auth-")
    original_auth = os.environ.get("XAUTHORITY")
    try:
        parent = servers.start("/usr/bin/Xvfb", ["-screen", "0", "1000x750x24", "-nolisten", "tcp"])
        # Xephyr's host is another temporary Xvfb, NEVER the user's display.
        authority = Path(private.name) / "nested-xephyr.xauth"
        TEST_XAUTHORITY = str(authority)
        os.environ["XAUTHORITY"] = TEST_XAUTHORITY
        child = servers.start_authenticated(
            "/usr/bin/Xephyr", ["-screen", "800x600", "-nolisten", "tcp"],
            parent, authority,
        )
        d = display.Display(child)
        try:
            owned, foreign, pid, foreign_pid = fixture_windows(d)
            require(authority.stat().st_mode & 0o077 == 0,
                    "private Xephyr authority file is readable by other users")
            require(Path(private.name).stat().st_mode & 0o077 == 0,
                    "private Xephyr authorization directory is publicly accessible")
            missing_cookie = helper_request(child, {"operation": "ping"}, auth_path=None)
            require(not missing_cookie.get("ok", True) and
                    "connect" in str(missing_cookie.get("error", "")).lower(),
                    f"unauthenticated client accessed Xephyr: {missing_cookie}")
            wrong_authority = Path(private.name) / "wrong-cookie.xauth"
            subprocess.run(
                ["/usr/bin/xauth", "-f", str(wrong_authority), "add",
                 child, ".", secrets.token_hex(16)],
                env=dict(os.environ, HOME=private.name, XAUTHORITY=str(wrong_authority)),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                timeout=8, check=True,
            )
            wrong_cookie = helper_request(child, {"operation": "ping"}, auth_path=wrong_authority)
            require(not wrong_cookie.get("ok", True) and
                    "connect" in str(wrong_cookie.get("error", "")).lower(),
                    f"invalid MIT-MAGIC-COOKIE-1 accessed Xephyr: {wrong_cookie}")
            print("PASS private Xephyr Xauthority: valid cookie accepted, missing/wrong denied")
            owned_xid = f"0x{owned.id:x}"
            foreign_xid = f"0x{foreign.id:x}"

            inspection = accepted(child, {"operation": "inspect", "allowedPids": [pid]})
            ids = [window["windowId"] for window in inspection["windows"]]
            require(ids == [owned_xid], f"cross-agent X11 window leak: {ids}")
            require(inspection["activeTitle"] == "RepoTunnel Synthetic Owned",
                    "owned window title was not reported")
            require(foreign_xid not in ids, "foreign window leaked into scoped inspection")
            print("PASS real nested-X11 window inspection / foreign window filtered")

            # A foreign active window may exist on the same shared Xephyr.
            # Scoped observation must never report its active title, and
            # explicitly empty access scopes must NOT mean unrestricted.
            d.screen().root.change_property(
                d.intern_atom("_NET_ACTIVE_WINDOW"), Xatom.WINDOW, 32, [foreign.id]
            )
            d.sync()
            ping = accepted(child, {"operation": "ping", "allowedPids": [pid]})
            require(ping["windowCount"] == 1 and ping["activeTitle"] == "",
                    f"foreign active title leaked through scoped ping: {ping}")
            ping_empty = accepted(child, {"operation": "ping", "allowedPids": []})
            require(ping_empty["windowCount"] == 0 and ping_empty["activeTitle"] == "",
                    f"empty PID list unexpectedly granted X11 access: {ping_empty}")
            scoped_inspection = accepted(child, {"operation": "inspect", "allowedPids": [pid]})
            require(scoped_inspection["activeTitle"] == "", "foreign active title leaked in inspect")
            empty_inspection = accepted(child, {"operation": "inspect", "allowedPids": []})
            require(not empty_inspection["windows"] and empty_inspection["activeTitle"] == "",
                    "empty PID list unexpectedly granted read-only inspection")
            d.screen().root.change_property(
                d.intern_atom("_NET_ACTIVE_WINDOW"), Xatom.WINDOW, 32, [owned.id]
            )
            d.sync()
            print("PASS real nested-X11 active-title redaction and empty-scope denial")

            frame = accepted(child, {
                "operation": "frame", "windowId": owned_xid,
                "allowedPids": [pid], "format": "png",
            })
            require(frame["mimeType"] == "image/png", "real frame is not PNG")
            data = base64.b64decode(frame["data"], validate=True)
            image = Image.open(io.BytesIO(data))
            require(image.size == (260, 180), f"wrong owned frame dimensions: {image.size}")
            require(frame["windowId"] == owned_xid, "wrong owned capture")
            print("PASS real Xephyr window pixel capture / image dimensions")

            reason = denied(child, {
                "operation": "frame", "windowId": foreign_xid,
                "allowedPids": [pid], "format": "png",
            })
            require("current process" in reason, f"wrong frame denial: {reason}")
            reason = denied(child, {
                "operation": "click", "windowId": foreign_xid,
                "allowedPids": [pid], "xRatio": 0.4, "yRatio": 0.5,
            })
            require("current process" in reason, f"wrong click denial: {reason}")
            reason = denied(child, {
                "operation": "key", "windowId": foreign_xid,
                "allowedPids": [pid], "shortcut": "Ctrl+S",
            })
            require("current process" in reason, f"wrong shortcut denial: {reason}")
            print("PASS live X11 owner denial for foreign frame, click, and shortcut")

            accepted(child, {"operation": "activate", "windowId": owned_xid, "allowedPids": [pid]})
            owned.change_attributes(event_mask=X.ButtonPressMask | X.ButtonReleaseMask)
            d.sync()
            accepted(child, {
                "operation": "click", "windowId": owned_xid,
                "allowedPids": [pid], "xRatio": 0.5, "yRatio": 0.5,
            })
            d.sync()
            delivered_events = []
            while d.pending_events():
                delivered_events.append(d.next_event().type)
            require(X.ButtonPress in delivered_events and X.ButtonRelease in delivered_events,
                    f"approved click was not delivered to the synthetic owned window: {delivered_events}")
            print("PASS approved window activation and delivered XTEST pointer click")

            # An XID can remain valid even when the owning PID property changes.
            # The helper must RECHECK ownership on each new request.
            owned.change_property(d.intern_atom("_NET_WM_PID"), Xatom.CARDINAL, 32, [foreign_pid])
            d.sync()
            for op in ("frame", "activate"):
                req = {"operation": op, "windowId": owned_xid, "allowedPids": [pid]}
                reason = denied(child, req)
                require("current process" in reason, f"stale PID accepted: {op}: {reason}")
            print("PASS live process-identity drift refused for stale window IDs")

            # Restart/teardown model: after the isolated child display exits,
            # the helper cannot accidentally connect to a host DISPLAY.
            servers.processes[-1].terminate()
            servers.processes[-1].wait(timeout=5)
            reason = denied(child, {"operation": "ping"})
            require("connect" in reason.lower(), f"isolated display loss not reported: {reason}")
            print("PASS dead isolated display cannot fall back to host desktop")
        finally:
            try:
                d.close()
            except Exception:
                # The test deliberately terminates Xephyr before closing the
                # owning Xlib client; the connection is already severed.
                if servers.processes[-1].poll() is None:
                    raise
        # This second Xephyr instance exercises actual native GUI processes
        # and window-manager lifecycle; the first exercises synthetic hostile
        # X11 identity and authentication cases.
        exercise_real_gui_lifecycle(servers, parent, private)
    finally:
        servers.close()
        TEST_XAUTHORITY = None
        if original_auth is None:
            os.environ.pop("XAUTHORITY", None)
        else:
            os.environ["XAUTHORITY"] = original_auth
        private.cleanup()
    print("PASS cleanup: all off-screen Xvfb/Xephyr processes reaped")


if __name__ == "__main__":
    run()
