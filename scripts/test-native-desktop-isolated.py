#!/usr/bin/env python3
"""Isolated source-built RepoTunnel desktop smoke; run only under private Xvfb/DBus.

Launches no production installation. Nested Bubblewrap denies network, production
filesystem writes, IPC reuse and host GUI observation. Runtime is marker-owned,
synthetic and disposable. A source-built debug binary is allowed for *baseline*
only; do not claim it exercises source modifications newer than its timestamp.
"""
import json
import os
from pathlib import Path
from datetime import datetime, timezone
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
TASK = ROOT / ".repotunnel-tmp" / "desktop-perf-isolated-20261026"
RUNTIME = TASK / "runtime"
MODE = sys.argv[1] if len(sys.argv) > 1 else "preflight"
if MODE not in ("preflight", "launch", "launch-release", "inspect-release", "audit-release", "pixel-release", "launch-dev-preview", "audit-dev-preview", "pixel-dev-preview", "workflow-dev-preview", "resize-dev-preview", "commands-dev-preview", "editor-dev-preview", "home-dev-preview", "editor-restore-dev-preview", "explorer-safety-dev-preview", "ai-workspace-permissions-dev-preview", "process-output-dev-preview", "soak-dev-preview", "large-project-dev-preview"):
    raise SystemExit("usage: test-native-desktop-isolated.py [preflight|launch|launch-release|inspect-release|audit-release|pixel-release|launch-dev-preview|audit-dev-preview|pixel-dev-preview|workflow-dev-preview|resize-dev-preview|commands-dev-preview|editor-dev-preview|home-dev-preview|editor-restore-dev-preview|explorer-safety-dev-preview|ai-workspace-permissions-dev-preview|process-output-dev-preview|soak-dev-preview|large-project-dev-preview]")
# Every dev-preview mode (including navigation, pixel benchmarks, and resizing)
# must start with a fresh private HOME/XDG. Reusing a previous synthetic profile
# accumulates test-only projects and makes visual timings incomparable.
# Preserve existing screenshots/JSON under TASK; isolate only the live profile.
if MODE.endswith("-dev-preview"):
    RUNTIME = TASK / ("isolated-workflow-" + str(time.time_ns()))
# Soak evidence is unique per run; do not overwrite prior native screenshots.
EVIDENCE = (TASK / (("soak-" if MODE == "soak-dev-preview" else "large-project-") + str(time.time_ns()))) if MODE in ("soak-dev-preview", "large-project-dev-preview") else TASK
if EVIDENCE != TASK:
    EVIDENCE.mkdir(parents=True, exist_ok=False)
if not os.environ.get("DISPLAY") or not os.environ.get("DBUS_SESSION_BUS_ADDRESS"):
    raise SystemExit("Refusing: private Xvfb DISPLAY and D-Bus required")
if not os.environ.get("XAUTHORITY"):
    raise SystemExit("Refusing: no private Xvfb Xauthority")
if "wayland" in os.environ.get("XDG_SESSION_TYPE", "").lower():
    raise SystemExit("Refusing: host Wayland session environment")

paths = {}
for name in ("home", "config", "data", "cache", "run", "tmp"):
    path = RUNTIME / name
    path.mkdir(parents=True, exist_ok=True)
    path.chmod(0o700)
    paths[name] = str(path)
RUNTIME.chmod(0o700)
if MODE == "editor-restore-dev-preview":
    # Seed ONLY a fresh, marker-owned test XDG/Projects profile. Never access
    # an existing user profile or reuse any actual editor session.
    import sqlite3
    synthetic_home = (RUNTIME / "home" / "Projects" / "Editor Session QA").resolve()
    assert synthetic_home.is_relative_to(RUNTIME.resolve())
    synthetic_home.mkdir(parents=True, exist_ok=True)
    synthetic_workspace = "workspace-synthetic-editor-restore"
    fixture_tabs = []
    for index in range(16):
        path = f"synthetic-tab-{index:02d}.ts"
        source = f"export const restored{index} = {index};\\n"
        if index == 14:
            source = "export const locallyEdited = 'base';\\n"
        if index == 15:
            source = "export const externallyEdited = 'disk-new';\\n"
        (synthetic_home / path).write_text(source, encoding="utf-8")
        saved = {"workspaceId": synthetic_workspace, "path": path, "dirty": False}
        if index == 14:
            saved.update({"dirty": True, "draftContent": "export const locallyEdited = 'draft';\\n",
                          "savedContent": source})
        if index == 15:
            saved.update({"dirty": True, "draftContent": "export const externallyEdited = 'my-draft';\\n",
                          "savedContent": "export const externallyEdited = 'original';\\n"})
        fixture_tabs.append(saved)
    project_registry = RUNTIME / "data" / "app.repotunnel.desktop" / "workspaces.json"
    project_registry.parent.mkdir(parents=True, exist_ok=True)
    project_registry.write_text(json.dumps([{
        "id": synthetic_workspace, "name": "Editor Session QA",
        "path": str(synthetic_home), "addedAt": 1,
        "accessMode": "readWrite", "changePolicy": "review",
        "commandPolicy": "review",
    }]), encoding="utf-8")
    storage = project_registry.parent / "localstorage" / "http_localhost_1420.localstorage"
    storage.parent.mkdir(parents=True, exist_ok=True)
    session_data = json.dumps({
        "tabs": fixture_tabs,
        "activeKey": synthetic_workspace + ":synthetic-tab-15.ts",
        "secondaryKey": synthetic_workspace + ":synthetic-tab-14.ts",
    })
    with sqlite3.connect(storage) as db:
        db.execute("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB NOT NULL ON CONFLICT FAIL)")
        db.execute("INSERT INTO ItemTable (key, value) VALUES (?, ?)",
                   ("repotunnel.editorSession.v1", session_data.encode("utf-16le")))
    print(json.dumps({"privateEditorRestoreFixture": {
        "tabs": len(fixture_tabs), "dirtyDrafts": 2,
        "workspaceRoot": str(synthetic_home),
    }}), flush=True)
if MODE == "large-project-dev-preview":
    # Native stress fixture owned entirely by this invocation's fresh private
    # XDG/HOME. No real projects or files are opened, changed or indexed.
    import sqlite3
    synthetic_root = (RUNTIME / "home" / "Projects" / "Large Explorer QA").resolve()
    assert synthetic_root.is_relative_to(RUNTIME.resolve())
    synthetic_root.mkdir(parents=True, exist_ok=True)
    count = 2400 if len(sys.argv) < 3 else int(sys.argv[2])
    if count not in (2400, 20000):
        raise SystemExit("Large-project fixture supports only 2400 or 20000 synthetic files")
    for index in range(count):
        (synthetic_root / f"file-{index:04d}.ts").write_text(
            f"export const syntheticIndex = {index};\\n", encoding="utf-8")
    workspace_id = "workspace-synthetic-large-explorer"
    registry_path = RUNTIME / "data" / "app.repotunnel.desktop" / "workspaces.json"
    registry_path.parent.mkdir(parents=True, exist_ok=True)
    registry_path.write_text(json.dumps([{
        "id": workspace_id, "name": "Large Explorer QA",
        "path": str(synthetic_root), "addedAt": 1,
        "accessMode": "readWrite", "changePolicy": "review",
        "commandPolicy": "review",
    }]), encoding="utf-8")
    storage = registry_path.parent / "localstorage" / "http_localhost_1420.localstorage"
    storage.parent.mkdir(parents=True, exist_ok=True)
    session = json.dumps({
        "tabs": [{"workspaceId": workspace_id, "path": "file-0000.ts", "dirty": False}],
        "activeKey": workspace_id + ":file-0000.ts", "secondaryKey": None,
    })
    with sqlite3.connect(storage) as db:
        db.execute("CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB NOT NULL ON CONFLICT FAIL)")
        db.execute("INSERT INTO ItemTable (key, value) VALUES (?, ?)",
                   ("repotunnel.editorSession.v1", session.encode("utf-16le")))
    print(json.dumps({"syntheticLargeDirectory": {
        "fileCount": count, "root": str(synthetic_root),
        "privateXdg": True, "restoredTab": "file-0000.ts",
    }}), flush=True)
fontconfig = RUNTIME / "fonts.conf"
fontconfig.write_text(
    '<?xml version="1.0"?>\n'
    '<fontconfig>\n'
    '  <dir>/usr/share/fonts</dir>\n'
    f'  <cachedir>{paths["cache"]}/fontconfig</cachedir>\n'
    '</fontconfig>\n', encoding="utf-8"
)
child_env = {
    "HOME": paths["home"], "XDG_CONFIG_HOME": paths["config"],
    "XDG_DATA_HOME": paths["data"], "XDG_CACHE_HOME": paths["cache"],
    "XDG_RUNTIME_DIR": paths["run"], "TMPDIR": paths["tmp"],
    "DISPLAY": os.environ["DISPLAY"], "XAUTHORITY": os.environ["XAUTHORITY"],
    "DBUS_SESSION_BUS_ADDRESS": os.environ["DBUS_SESSION_BUS_ADDRESS"],
    "WEBKIT_GST_DMABUF_SINK_DISABLED": "1",
    "WEBKIT_DISABLE_COMPOSITING_MODE": "1",
    "WEBKIT_DISABLE_DMABUF_RENDERER": "1",
    "LIBGL_ALWAYS_SOFTWARE": "1", "GDK_BACKEND": "x11",
    "FONTCONFIG_FILE": str(fontconfig), "PATH": "/usr/bin:/bin",
}
bwrap = [
    "/usr/bin/bwrap", "--die-with-parent", "--new-session",
    "--unshare-net", "--unshare-pid", "--unshare-ipc", "--unshare-uts",
    "--ro-bind", "/", "/", "--bind", str(RUNTIME), str(RUNTIME),
    "--proc", "/proc", "--dev", "/dev", "--clearenv",
]
for key, value in child_env.items():
    bwrap.extend(["--setenv", key, value])
bwrap.append("--")
proof = (
    "import json,os,socket; "
    "print(json.dumps(dict(interfaces=socket.if_nameindex(),"
    "display=os.environ.get('DISPLAY'),home=os.environ['HOME'],"
    "xdgData=os.environ['XDG_DATA_HOME'],"
    "xdgConfig=os.environ['XDG_CONFIG_HOME'],"
    "runtime=os.environ['XDG_RUNTIME_DIR'],"
    "testWritable=os.access(os.environ['XDG_DATA_HOME'],os.W_OK),"
    "pid=os.getpid())))"
)
check = subprocess.run(bwrap + ["/usr/bin/python3", "-c", proof],
                       text=True, capture_output=True, timeout=15)
if check.returncode:
    print(json.dumps({"preflight": "failed", "exit": check.returncode,
                      "stderr": check.stderr[-1000:]}))
    raise SystemExit(4)
details = json.loads(check.stdout)
assert details["interfaces"] == [[1, "lo"]], details
assert details["testWritable"], details
assert str(RUNTIME) in details["home"], details
assert 1 <= details["pid"] < 10, details  # Bubblewrap has a PID-1 supervisor
print(json.dumps({"preflight": "passed", "isolation": details}), flush=True)
if MODE == "preflight":
    raise SystemExit(0)

binary = ROOT / "src-tauri" / "target" / ("release" if MODE in ("launch-release", "inspect-release", "audit-release", "pixel-release") else "debug") / "repotunnel"
if not binary.is_file():
    raise SystemExit("Source-built native binary unavailable")
backend_identity = {
    "path": str(binary.relative_to(ROOT)),
    "sizeBytes": binary.stat().st_size,
    "modifiedUtc": datetime.fromtimestamp(binary.stat().st_mtime, timezone.utc).isoformat(timespec="seconds"),
}
print(json.dumps({"nativeBinaryIdentity": backend_identity}), flush=True)
backend_label = f"Rust binary {backend_identity['path']} updated {backend_identity['modifiedUtc']}"
preview_mode = MODE.endswith("-dev-preview")
log_path = EVIDENCE / ("native-dev-preview.log" if preview_mode else "native-launch.log")
started = time.monotonic()
if preview_mode:
    # The existing, earlier compiled Tauri *debug* binary still expects its
    # development URL. Serve the freshly built `dist` on loopback *inside the
    # same private network namespace*. No public network or host port exists.
    preview_launch = """
set -eu
/usr/bin/python3 -m http.server 1420 --bind 127.0.0.1 --directory /workspace/dist > "$XDG_RUNTIME_DIR/static-preview.log" 2>&1 &
for i in $(seq 1 70); do
  if /usr/bin/python3 -c 'import urllib.request; urllib.request.urlopen("http://127.0.0.1:1420", timeout=.3)' >/dev/null 2>&1; then
    exec /workspace/src-tauri/target/debug/repotunnel
  fi
  sleep .1
done
echo "isolated static preview did not start" >&2
exit 31
"""
    launch = bwrap + ["/bin/sh", "-c", preview_launch]
else:
    launch = bwrap + [str(binary)]
with log_path.open("w") as log:
    process = subprocess.Popen(launch, stdout=log, stderr=subprocess.STDOUT)
    try:
        from Xlib import display, X
        d = display.Display(os.environ["DISPLAY"])
        found = []
        for _ in range(60):
            if process.poll() is not None:
                break
            windows = d.screen().root.query_tree().children
            names = []
            for window in windows:
                try:
                    name = window.get_wm_name()
                    if name:
                        names.append(str(name))
                except Exception:
                    pass
            if any("RepoTunnel" in name for name in names):
                found = names
                break
            time.sleep(0.2)
        window_visible_ms = round((time.monotonic() - started) * 1000, 1)
        print(json.dumps({"desktopLaunch": "window-visible" if found else "no-window",
                          "windowNames": found, "alive": process.poll() is None,
                          "elapsedMs": window_visible_ms,
                          "exitCode": process.poll()}), flush=True)
        if found:
            home_ready_ms = None
            if MODE == "home-dev-preview":
                # Measure from the private native process launch until the
                # actual Home heading AND actionable New Project control appear.
                # Window visibility alone does not prove the UI is interactive.
                import pyatspi
                desktop = pyatspi.Registry.getDesktop(0)
                def nested_nodes(node, depth=0):
                    if node is None or depth > 16:
                        return
                    yield node
                    for i in range(min(int(getattr(node, "childCount", 0)), 120)):
                        try:
                            yield from nested_nodes(node.getChildAtIndex(i), depth+1)
                        except Exception:
                            pass
                deadline = time.monotonic() + 9
                while time.monotonic() < deadline:
                    app = next((a for a in desktop
                                if "repotunnel" in str(getattr(a, "name", "") or "").lower()), None)
                    if app:
                        headings = set()
                        new_project = False
                        for node in nested_nodes(app):
                            role = node.getRoleName()
                            name = str(getattr(node, "name", "") or "")
                            if role == "heading":
                                headings.add(name)
                            elif role == "push button" and name.startswith("New project"):
                                new_project = True
                        if "Home" in headings and new_project:
                            home_ready_ms = round((time.monotonic() - started) * 1000, 2)
                            break
                    time.sleep(.06)
            time.sleep(4)
            if MODE == "large-project-dev-preview":
                import pyatspi
                import re
                from PIL import Image
                from Xlib import X
                started_large = time.monotonic()
                registry = pyatspi.Registry.getDesktop(0)
                def nodes_for_large(node, depth=0):
                    if node is None or depth > 24:
                        return
                    yield node
                    for index in range(min(int(getattr(node, "childCount", 0)), 450)):
                        try:
                            yield from nodes_for_large(node.getChildAtIndex(index), depth + 1)
                        except Exception:
                            pass
                def large_buttons():
                    app = next((app for app in registry if "repotunnel" in
                        str(getattr(app, "name", "") or "").lower()), None)
                    if app is None:
                        return []
                    found = []
                    for node in nodes_for_large(app):
                        try:
                            if node.getRoleName() == "push button":
                                found.append((str(node.name or ""), node))
                        except Exception:
                            pass
                    return found
                def until_large(predicate, seconds=12):
                    stop = time.monotonic() + seconds
                    while time.monotonic() < stop:
                        value = predicate()
                        if value:
                            return value
                        time.sleep(.10)
                    return None
                def initial_large_buttons():
                    buttons = large_buttons()
                    return buttons if any("Show 200 more" in name for name, _ in buttons) else None
                before = until_large(initial_large_buttons, 15)
                initial_names = [name for name, _ in before] if before else []
                def unique_large_files(names):
                    # Each file also has an Actions button; count distinct
                    # filenames rather than double-counting accessible controls.
                    return {match for name in names
                            for match in re.findall(r"file-\d{4,5}\.ts", name)}
                initial_file_names = unique_large_files(initial_names)
                show_more = next((node for name, node in (before or [])
                                  if "Show 200 more" in name), None)
                initial_ms = round((time.monotonic() - started_large) * 1000, 2)
                activated = False
                if show_more:
                    try:
                        show_more.queryComponent().grabFocus()
                        activated = bool(show_more.queryAction().doAction(0))
                    except Exception as exc:
                        print(json.dumps({"largeShowMoreError": str(exc)[:200]}), flush=True)
                def advanced_large_buttons():
                    buttons = large_buttons()
                    return buttons if len(unique_large_files([name for name, _ in buttons])) > len(initial_file_names) else None
                after = until_large(advanced_large_buttons, 10) if activated else None
                after_names = [name for name, _ in after] if after else []
                after_file_names = unique_large_files(after_names)
                next_more = any("Show 200 more" in name for name in after_names)
                elapsed = round((time.monotonic() - started_large) * 1000, 2)
                screen = d.screen()
                pixels = screen.root.get_image(0, 0, screen.width_in_pixels,
                                                screen.height_in_pixels, X.ZPixmap, 0xffffffff)
                original = EVIDENCE / "native-large-project-explorer.png"
                if pixels:
                    Image.frombytes("RGB", (screen.width_in_pixels,
                        screen.height_in_pixels), pixels.data, "raw", "BGRX").save(original)
                checks = [
                    {"step":"test-owned-large-file-fixture", "pass":
                     len(list(synthetic_root.glob("file-*.ts"))) == count},
                    {"step":"initial-show-more-accessible", "pass": show_more is not None},
                    {"step":"initial-page-has-real-files", "pass": len(initial_file_names) > 0},
                    {"step":"initial-dom-smaller-than-directory", "pass": len(initial_file_names) < count},
                    {"step":"initial-dom-bounded", "pass": 0 < len(initial_file_names) <= 200},
                    {"step":"actual-show-more-click", "pass": activated},
                    {"step":"next-page-accessible", "pass": len(after_file_names) > len(initial_file_names)},
                    {"step":"later-pages-remain-unmounted", "pass": len(after_file_names) < count},
                    {"step":"additional-page-control", "pass": next_more},
                    {"step":"native-pixel-screenshot", "pass": original.is_file() and original.stat().st_size > 20000 if original.is_file() else False},
                ]
                # Verify the newly-created file still opens in the Editor
                # even when a very large directory only has its first 200
                # entries in the Explorer. This is a test-owned private file.
                new_file_button = next(
                    (node for label, node in large_buttons() if label in ("+ File", "File")), None)
                created_name = "native-large-created-" + str(time.time_ns())[-8:] + ".ts"
                created_path = synthetic_root / created_name
                create_clicked = False
                typed_ok = False
                created_on_disk = False
                opened_in_editor = False
                input_found = False
                if new_file_button:
                    try:
                        create_clicked = bool(new_file_button.queryAction().doAction(0))
                        def filename_field():
                            app = next((app for app in registry if "repotunnel" in
                                str(getattr(app, "name", "") or "").lower()), None)
                            if app is None:
                                return None
                            return next((node for node in nodes_for_large(app)
                                if node.getRoleName() == "entry" and
                                str(getattr(node, "name", "") or "") == "file-name.ext"), None)
                        field = until_large(filename_field, 5) if create_clicked else None
                        input_found = field is not None
                        if field:
                            field.queryComponent().grabFocus()
                            typed = subprocess.run(
                                ["/usr/bin/xdotool", "type", "--clearmodifiers",
                                 "--delay", "12", created_name],
                                env=dict(os.environ, DISPLAY=os.environ["DISPLAY"],
                                         XAUTHORITY=os.environ["XAUTHORITY"]),
                                capture_output=True, timeout=12)
                            typed_ok = typed.returncode == 0
                            if typed_ok:
                                subprocess.run(["/usr/bin/xdotool", "key", "Return"],
                                    env=dict(os.environ, DISPLAY=os.environ["DISPLAY"],
                                             XAUTHORITY=os.environ["XAUTHORITY"]),
                                    capture_output=True, timeout=5)
                            created_on_disk = bool(until_large(
                                lambda: created_path if created_path.is_file() else None, 8))
                            def created_editor_tab():
                                app = next((app for app in registry if "repotunnel" in
                                    str(getattr(app, "name", "") or "").lower()), None)
                                if app is None:
                                    return False
                                return any(created_name in str(getattr(node, "name", "") or "") and
                                    node.getRoleName() in ("page tab", "push button", "heading")
                                    for node in nodes_for_large(app))
                            opened_in_editor = bool(until_large(created_editor_tab, 8)) if created_on_disk else False
                    except Exception as error:
                        print(json.dumps({"largeCreateError": str(error)[:220]}), flush=True)
                created_snapshot = d.screen().root.get_image(
                    0, 0, d.screen().width_in_pixels, d.screen().height_in_pixels,
                    X.ZPixmap, 0xffffffff)
                created_screenshot = EVIDENCE / "native-large-created-editor.png"
                if created_snapshot:
                    from PIL import Image
                    Image.frombytes("RGB", (d.screen().width_in_pixels,
                        d.screen().height_in_pixels), created_snapshot.data,
                        "raw", "BGRX").save(created_screenshot)
                checks.extend([
                    {"step":"native-large-folder-create-control", "pass":new_file_button is not None and create_clicked},
                    {"step":"native-private-create-input", "pass":input_found and typed_ok},
                    {"step":"native-file-actually-created", "pass":created_on_disk},
                    {"step":"native-created-file-editor-tab-opened", "pass":opened_in_editor},
                    {"step":"native-created-editor-screenshot", "pass":created_screenshot.is_file() and created_screenshot.stat().st_size>20000},
                ])
                summary = {
                    "scenario": f"real isolated Tauri + {count} private synthetic files and progressive Explorer DOM",
                    "fixtureFileCount": count,
                    "rustBinary": backend_identity, "checks": checks,
                    "passed": sum(item["pass"] for item in checks), "total": len(checks),
                    "initialVisibleFiles": len(initial_file_names),
                    "afterShowMoreVisibleFiles": len(after_file_names),
                    "initialResponseMs": initial_ms, "totalInteractionMs": elapsed,
                    "screenshot": str(original.relative_to(ROOT)),
                    "initialButtonSamples": initial_names[:12],
                    "afterButtonSamples": after_names[-12:],
                }
                (EVIDENCE / "native-large-project-results.json").write_text(json.dumps(summary, indent=2))
                print(json.dumps({"largeProjectAudit": summary}), flush=True)
                if summary["passed"] != summary["total"]:
                    raise RuntimeError("Native large-directory Explorer regression failed")
            if MODE == "editor-restore-dev-preview":
                import pyatspi
                import re
                registry = pyatspi.Registry.getDesktop(0)
                def all_accessible(node, depth=0):
                    if node is None or depth > 17:
                        return
                    yield node
                    for child_index in range(min(int(getattr(node, "childCount", 0)), 150)):
                        try:
                            yield from all_accessible(node.getChildAtIndex(child_index), depth+1)
                        except Exception:
                            continue
                accessible_app = next((app for app in registry
                    if "repotunnel" in str(getattr(app, "name", "") or "").lower()), None)
                observed = []
                if accessible_app:
                    for node in all_accessible(accessible_app):
                        try:
                            name = str(node.name or "")
                            role = node.getRoleName()
                            selected = node.getState().contains(pyatspi.STATE_SELECTED)
                            if "synthetic-tab-" in name or "changed externally" in name or (
                                role == "push button" and name == "Save"
                            ):
                                text_value = None
                                if role == "entry" and name.startswith("Edit synthetic-tab-"):
                                    try:
                                        interface = node.queryText()
                                        text_value = interface.getText(0, min(interface.characterCount, 400))
                                    except Exception:
                                        pass
                                observed.append({"name": name[:150], "role": role,
                                                 "selected": bool(selected), "text": text_value})
                        except Exception:
                            continue
                import re
                tab_entries = [entry for entry in observed if entry["role"] == "page tab"
                               and re.search(r"synthetic-tab-\d{2}\.ts", entry["name"])]
                unique_tabs = sorted({re.search(r"synthetic-tab-\d{2}\.ts", entry["name"]).group(0)
                                      for entry in tab_entries})
                selected = [entry["name"] for entry in tab_entries if entry["selected"]]
                conflicted = any("synthetic-tab-15.ts changed externally" in item["name"]
                                 for item in observed)
                entry_text = {item["name"]: item["text"] for item in observed
                              if item["role"] == "entry" and item["text"] is not None}
                drafts = {
                    "active-local-draft-restored": "my-draft" in
                        (entry_text.get("Edit synthetic-tab-15.ts") or ""),
                    "split-local-draft-restored": "'draft'" in
                        (entry_text.get("Edit synthetic-tab-14.ts") or ""),
                }
                saves = sum(item["role"] == "push button" and item["name"] == "Save"
                            for item in observed)
                disk_files = {
                    "local-draft-base-intact": (synthetic_home / "synthetic-tab-14.ts").read_text()
                        == "export const locallyEdited = 'base';\\n",
                    "external-disk-edit-intact": (synthetic_home / "synthetic-tab-15.ts").read_text()
                        == "export const externallyEdited = 'disk-new';\\n",
                }
                result = {
                    "isolation": "new private HOME + XDG + Bubblewrap loopback only",
                    "expectedTabs": 16, "restoredTabs": len(unique_tabs),
                    "selectedTabs": selected, "splitSaveButtons": saves,
                    "conflictWarningAccessible": conflicted, "drafts": drafts,
                    "diskFiles": disk_files,
                    "pass": len(unique_tabs) == 16 and saves >= 2
                        and any("synthetic-tab-15.ts" in name for name in selected)
                        and all(drafts.values()) and all(disk_files.values()),
                    "observed": observed[:70],
                }
                (TASK / "native-preview-editor-session-restore.json").write_text(
                    json.dumps(result, indent=2))
                print(json.dumps({"editorSessionRestoreAudit": result}), flush=True)
                if not result["pass"]:
                    raise RuntimeError("Synthetic editor session restoration assertions failed")
            if MODE == "home-dev-preview":
                log = (RUNTIME / "run" / "static-preview.log").read_text()
                forbidden = (
                    "WorkspaceEditor", "VideoPanel", "ProductionPanel",
                    "AgentsPanel", "TeamPanel", "RemoteMachinesPanel", "PhonePanel",
                    "ExecutionPanel", "GitPanel", "WorkflowPanel", "ModelHub",
                    "ChangeHistoryPanel", "CheckpointManager", "editor-core",
                )
                requests = [line.split("GET ")[1].split(" HTTP")[0]
                            for line in log.splitlines() if "GET /assets/" in line]
                unneeded = [name for name in forbidden if
                            any("/assets/" + name + "-" in path for path in requests)]
                result = {"cleanPrivateProfile": True,
                          "assetsRequested": requests, "unneededLazyChunks": unneeded,
                          "homeReadyElapsedMs": home_ready_ms,
                          "windowVisibleElapsedMs": window_visible_ms,
                          "pass": len(unneeded) == 0 and home_ready_ms is not None}
                (TASK / "native-preview-clean-home-assets.json").write_text(
                    json.dumps(result, indent=2))
                # Keep comparative startup evidence across new private profiles;
                # never overwrite earlier launch samples or user app data.
                with (TASK / "native-preview-home-startup-samples.jsonl").open("a") as file:
                    file.write(json.dumps(result, sort_keys=True) + "\n")
                print(json.dumps({"homeAssetAudit": result}), flush=True)
                if not result["pass"]:
                    raise RuntimeError("Private Home not ready or fetched lazy chunks: " + repr(result))
            if MODE == "resize-dev-preview":
                import pyatspi
                from PIL import Image
                screenshots = []
                resize_results = []
                app_window = next((w for w in d.screen().root.query_tree().children
                                   if w.get_wm_name() == "RepoTunnel"), None)
                if app_window is None:
                    raise RuntimeError("Private test RepoTunnel window is missing")
                for width, height in ((1180,720),(1400,820),(1660,940),(1200,740)):
                    app_window.configure(x=12,y=12,width=width,height=height)
                    d.sync()
                    time.sleep(0.30)
                    bounds = app_window.get_geometry()
                    screen = d.screen()
                    shot = screen.root.get_image(0,0,screen.width_in_pixels,
                                                 screen.height_in_pixels,X.ZPixmap,0xffffffff)
                    if not shot:
                        resize_results.append({"size":[width,height],"status":"no-frame"})
                        continue
                    pixels = Image.frombytes("RGB", (screen.width_in_pixels,screen.height_in_pixels),
                                             shot.data,"raw","BGRX")
                    x = max(0,int(bounds.x))
                    y = max(0,int(bounds.y))
                    cropped=pixels.crop((x,y,min(x+int(bounds.width),pixels.width),
                                              min(y+int(bounds.height),pixels.height)))
                    file_name=f"native-resize-{width}x{height}.png"
                    cropped.save(TASK/file_name)
                    sample=cropped.resize((64,40))
                    distinct=len(set(sample.getdata()))
                    resize_results.append({"size":[width,height],
                            "actual":[int(bounds.width),int(bounds.height)],
                            "status":"pass" if (bounds.width==width and bounds.height==height
                                                and distinct>=20) else "mismatch",
                            "distinctColors":distinct, "screenshot":file_name})
                summary={"scenario":f"private current frontend + {backend_label} window resize",
                         "results":resize_results,
                         "passed":sum(i["status"]=="pass" for i in resize_results),
                         "tested":len(resize_results)}
                (TASK/"native-preview-resize.json").write_text(json.dumps(summary,indent=2))
                print(json.dumps({"resizeAudit":summary}),flush=True)
            if MODE in ("workflow-dev-preview", "commands-dev-preview", "editor-dev-preview", "explorer-safety-dev-preview", "ai-workspace-permissions-dev-preview", "process-output-dev-preview"):
                import pyatspi
                registry = pyatspi.Registry.getDesktop(0)
                def nodes(node, depth=0):
                    # The Commands view embeds deeply nested Launcher/AI
                    # Workspace sections; 17 levels missed visible controls.
                    if node is None or depth > 28:
                        return
                    yield node
                    for i in range(min(int(getattr(node, "childCount", 0)), 120)):
                        try:
                            yield from nodes(node.getChildAtIndex(i), depth+1)
                        except Exception:
                            pass
                def app_root():
                    for app in registry:
                        if "repotunnel" in str(getattr(app, "name", "") or "").lower():
                            return app
                    return None
                def find_node(role=None, name=None, exact=True):
                    root = app_root()
                    if not root:
                        return None
                    for node in nodes(root):
                        node_role = node.getRoleName()
                        node_name = str(getattr(node, "name", "") or "")
                        if (role is None or node_role == role) and (name is None or
                                (node_name == name if exact else name in node_name)):
                            return node
                    return None
                def until(check, timeout=3):
                    limit = time.monotonic() + timeout
                    while time.monotonic() < limit:
                        value = check()
                        if value:
                            return value
                        time.sleep(0.05)
                    return None
                def focused_node():
                    root = app_root()
                    if not root:
                        return None
                    candidate = None
                    for node in nodes(root):
                        try:
                            if node.getState().contains(pyatspi.STATE_FOCUSED):
                                candidate = {"role":node.getRoleName(), "name":str(node.name or "")}
                        except Exception:
                            continue
                    return candidate
                outcomes = []
                button = until(lambda: find_node("push button", "New project", exact=False), timeout=10)
                outcomes.append({"step":"new-project-control", "pass": button is not None,
                                 "appAvailableOnFailure":app_root() is not None if button is None else None})
                if button:
                    button.queryComponent().grabFocus()
                    button.queryAction().doAction(0)
                    dialog = until(lambda: find_node("dialog", "Create from scratch"))
                    outcomes.append({"step":"open-create-dialog", "pass": dialog is not None,
                                     "accessibleName":str(dialog.name) if dialog else None})
                    entry_focus = focused_node()
                    outcomes.append({"step":"modal-input-focus", "pass": bool(entry_focus and entry_focus["role"]=="entry"),
                                     "focused":entry_focus})
                    if dialog:
                        cancel = find_node("push button", "Cancel")
                        outcomes.append({"step":"cancel-control", "pass":cancel is not None})
                        if cancel:
                            cancel.queryAction().doAction(0)
                            closed = until(lambda: find_node("dialog", "Create from scratch") is None)
                            outcomes.append({"step":"cancel-closes-dialog", "pass":bool(closed)})
                            after_cancel = until(lambda: (candidate if candidate and "New project" in candidate["name"] else None) if (candidate := focused_node()) else None, timeout=2)
                            outcomes.append({"step":"cancel-restores-trigger-focus",
                                             "pass":bool(after_cancel),
                                             "focused":after_cancel or focused_node()})
                    button = find_node("push button", "New project", exact=False)
                    if button:
                        button.queryComponent().grabFocus()
                        button.queryAction().doAction(0)
                        dialog = until(lambda: find_node("dialog", "Create from scratch"))
                        field = find_node("entry", "Project name")
                        if not field:
                            field = next((node for node in nodes(app_root())
                                          if node.getRoleName()=="entry" and
                                          str(getattr(node,"name","") or "") != "Search projects"),None)
                        outcomes.append({"step":"project-name-input", "pass": field is not None})
                        if field and dialog:
                            synthetic_name = "Synthetic Performance QA " + str(time.time_ns())[-12:]
                            try:
                                # Avoid dropped XTEST keystrokes while the WebView
                                # handles a fresh project dialog under CPU load.
                                # Verify the *exact* synthetic name before creating.
                                actual = ""
                                for attempt in range(3):
                                    field.queryComponent().grabFocus()
                                    subprocess.run(
                                        ["/usr/bin/xdotool", "key", "--clearmodifiers", "ctrl+a"],
                                        env=dict(os.environ, DISPLAY=os.environ["DISPLAY"],
                                                 XAUTHORITY=os.environ["XAUTHORITY"]),
                                        capture_output=True, timeout=4,
                                    )
                                    typed = subprocess.run(
                                        ["/usr/bin/xdotool", "type", "--clearmodifiers",
                                         "--delay", "50", synthetic_name],
                                        env=dict(os.environ, DISPLAY=os.environ["DISPLAY"],
                                                 XAUTHORITY=os.environ["XAUTHORITY"]),
                                        capture_output=True, text=True, timeout=12,
                                    )
                                    if typed.returncode:
                                        raise RuntimeError(f"virtual-keyboard typing failed: {typed.stderr[:180]}")
                                    time.sleep(0.12)
                                    actual = field.queryText().getText(0, -1)
                                    if actual == synthetic_name:
                                        break
                                if actual != synthetic_name:
                                    raise RuntimeError("test name entry incomplete after bounded retries")
                                create = until(lambda: find_node("push button", "Create project"))
                                outcomes.append({"step":"create-control", "pass":create is not None})
                                if create:
                                    create.queryAction().doAction(0)
                                    closed=until(lambda: find_node("dialog", "Create from scratch") is None,timeout=6)
                                    # Project cards may be virtualized or below the
                                    # accessibility tree's visible range. Validate
                                    # creation against the PRIVATE test registry and
                                    # test-only Projects root, never user app data.
                                    def registered_project():
                                        try:
                                            registry_path = RUNTIME / "data" / "app.repotunnel.desktop" / "workspaces.json"
                                            entries = json.loads(registry_path.read_text())
                                            return next((entry for entry in entries
                                                if entry.get("name") == synthetic_name), None)
                                        except (OSError, ValueError, TypeError):
                                            return None
                                    registered = until(registered_project, timeout=6)
                                    synthetic_root = (RUNTIME / "home" / "Projects").resolve()
                                    created_path = Path(registered["path"]).resolve() if registered else None
                                    safe_created = bool(created_path
                                        and created_path.is_relative_to(synthetic_root)
                                        and created_path.is_dir())
                                    outcomes.append({"step":"create-synthetic-project",
                                        "pass": bool(closed and safe_created),
                                        "dialogClosed":bool(closed),
                                        "registeredInPrivateTestState":bool(registered),
                                        "directoryUnderPrivateTestHome":safe_created})
                            except Exception as error:
                                outcomes.append({"step":"create-synthetic-project","pass":False,
                                                 "error":repr(error)[:200]})
                if MODE in ("editor-dev-preview", "explorer-safety-dev-preview"):
                    try:
                        new_file = until(lambda: find_node("push button", "+ File") or
                                         find_node("push button", "File"), timeout=5)
                        outcomes.append({"step": "editor-new-file-control", "pass": bool(new_file)})
                        if new_file:
                            new_file.queryAction().doAction(0)
                            entry = until(lambda: find_node("entry", "file-name.ext"))
                            outcomes.append({"step": "editor-new-file-input", "pass": bool(entry)})
                            if entry:
                                file_name = "lazy-editor-smoke-" + str(time.time_ns())[-9:] + ".ts"
                                entry.queryComponent().grabFocus()
                                typed = subprocess.run(
                                    ["/usr/bin/xdotool", "type", "--clearmodifiers", "--delay", "15", file_name],
                                    env=dict(os.environ, DISPLAY=os.environ["DISPLAY"],
                                             XAUTHORITY=os.environ["XAUTHORITY"]),
                                    capture_output=True, text=True, timeout=12)
                                outcomes.append({"step": "editor-file-name-typed", "pass": typed.returncode == 0})
                                subprocess.run(["/usr/bin/xdotool", "key", "Return"],
                                    env=dict(os.environ, DISPLAY=os.environ["DISPLAY"],
                                             XAUTHORITY=os.environ["XAUTHORITY"]),
                                    capture_output=True, timeout=4)
                                fixture = (Path(registered["path"]) / file_name) if registered else None
                                created = until(lambda: fixture if fixture and fixture.is_file() else None, timeout=6)
                                outcomes.append({"step": "editor-private-fixture-created", "pass": bool(created)})
                                opened = until(lambda: find_node("push button", "Save") or
                                                find_node("push button", "Open externally"),
                                               timeout=5)
                                outcomes.append({"step": "editor-view-opened", "pass": bool(opened)})
                                def editor_chunk_loaded():
                                    log = (RUNTIME / "run" / "static-preview.log")
                                    try:
                                        return "WorkspaceEditor-" in log.read_text()
                                    except OSError:
                                        return False
                                chunk = until(editor_chunk_loaded, timeout=5)
                                outcomes.append({"step": "editor-lazy-chunk-fetched", "pass": bool(chunk)})
                                if MODE == "explorer-safety-dev-preview":
                                    # A native destructive-dialog test that NEVER confirms
                                    # deletion and never touches non-synthetic files.
                                    actions = until(lambda: find_node("push button", "Actions for " + file_name))
                                    outcomes.append({"step": "synthetic-file-actions-button", "pass": bool(actions)})
                                    if actions:
                                        actions.queryAction().doAction(0)
                                        deletion = until(lambda: find_node("push button", "Delete", exact=False))
                                        outcomes.append({"step": "delete-menu-action-visible", "pass": bool(deletion)})
                                        if deletion:
                                            deletion.queryAction().doAction(0)
                                            dialog = until(lambda: find_node("dialog", "Delete file?"))
                                            outcomes.append({"step": "safe-delete-dialog-visible", "pass": bool(dialog)})
                                            if dialog:
                                                screen = d.screen()
                                                pixels = screen.root.get_image(
                                                    0, 0, screen.width_in_pixels, screen.height_in_pixels,
                                                    X.ZPixmap, 0xffffffff)
                                                if pixels:
                                                    from PIL import Image
                                                    frame = Image.frombytes("RGB",
                                                        (screen.width_in_pixels, screen.height_in_pixels),
                                                        pixels.data, "raw", "BGRX")
                                                    evidence = TASK / "native-explorer-delete-confirm-isolated.png"
                                                    frame.save(evidence)
                                                    print(json.dumps({"deleteDialogScreenshot":str(evidence.relative_to(ROOT))}),
                                                          flush=True)
                                                cancel = until(lambda: find_node("push button", "Cancel"))
                                                if cancel:
                                                    cancel.queryAction().doAction(0)
                                                closed = until(lambda: find_node("dialog", "Delete file?") is None)
                                                intact = bool(fixture and fixture.is_file())
                                                outcomes.append({"step": "cancel-preserves-private-file",
                                                    "pass":bool(cancel and closed and intact),
                                                    "dialogClosed":bool(closed), "syntheticFileUnchanged":intact})
                    except Exception as error:
                        outcomes.append({"step": "editor-workflow", "pass":False,
                                         "error":repr(error)[:240]})
                if MODE == "ai-workspace-permissions-dev-preview":
                    # Only read the actual isolated UI. Never enable Desktop,
                    # launch an AI app or touch any user session. WebKitGTK does
                    # not expose this nested panel's text through the AT-SPI
                    # traversal here; text semantics are verified separately in
                    # frontend integration tests, NOT falsely claimed in native.
                    try:
                        commands = find_node("push button", "Commands")
                        if commands:
                            commands.queryAction().doAction(0)
                        panel = until(lambda: find_node(name="Live terminal command"))
                        outcomes.append({"step": "open-real-commands-view",
                            "pass": bool(commands and panel)})
                        screen = d.screen()
                        before = screen.root.get_image(
                            580, 400, 600, 420, X.ZPixmap, 0xffffffff)
                        shortcut = until(lambda: find_node("push button", "AI Workspace"))
                        outcomes.append({"step": "ai-workspace-quick-jump-accessible",
                            "pass": shortcut is not None})
                        if shortcut:
                            shortcut.queryAction().doAction(0)
                        time.sleep(0.85)  # Wait for smooth native WebKit scroll.
                        after = screen.root.get_image(
                            580, 400, 600, 420, X.ZPixmap, 0xffffffff)
                        outcomes.append({"step": "ai-workspace-jump-scrolls-real-native-view",
                            "pass": bool(shortcut and before and after and before.data != after.data)})
                        pixels = screen.root.get_image(
                            0, 0, screen.width_in_pixels, screen.height_in_pixels,
                            X.ZPixmap, 0xffffffff)
                        screenshot = TASK / "native-ai-workspace-permissions-isolated.png"
                        if pixels:
                            from PIL import Image
                            frame = Image.frombytes("RGB",
                                (screen.width_in_pixels, screen.height_in_pixels),
                                pixels.data, "raw", "BGRX")
                            frame.save(screenshot)
                            print(json.dumps({"aiWorkspacePermissionsScreenshot":
                                str(screenshot.relative_to(ROOT)),
                                "nativeTextSemantics": "not exposed; verify frontend separately"}),
                                flush=True)
                        outcomes.append({"step": "real-native-ai-workspace-visual-capture",
                            "pass": screenshot.is_file() and screenshot.stat().st_size > 20000,
                            "verification": "X11 screenshot; permission message reviewed visually, not via AT-SPI"})
                        outcomes.append({"step": "no-ai-session-or-desktop-permission-action",
                            "pass": True, "verification":
                                "This test performs only project creation, navigation, scroll and screenshot"})
                    except Exception as error:
                        outcomes.append({"step": "ai-workspace-native-visual",
                            "pass": False, "error": repr(error)[:240]})
                if MODE == "commands-dev-preview":
                    # The isolated profile contains synthetic test projects only.
                    # This exact command has no filesystem/network side effects.
                    try:
                        commands = find_node("push button", "Commands")
                        if commands:
                            commands.queryAction().doAction(0)
                        panel = until(lambda: find_node(name="Live terminal command"))
                        outcomes.append({"step":"open-commands-and-terminal","pass":commands is not None and panel is not None})
                        if panel:
                            sentinel = "isolated-qa-commands-" + str(time.time_ns())[-9:]
                            panel.queryComponent().grabFocus()
                            typed = subprocess.run(
                                ["/usr/bin/xdotool", "type", "--clearmodifiers", "--delay", "15",
                                 "printf " + sentinel],
                                env=dict(os.environ, DISPLAY=os.environ["DISPLAY"],
                                         XAUTHORITY=os.environ["XAUTHORITY"]),
                                capture_output=True,text=True,timeout=12,
                            )
                            outcomes.append({"step":"type-side-effect-free-command","pass":typed.returncode==0,
                                             "error":typed.stderr[:100] if typed.returncode else None})
                            runner = until(lambda: find_node("push button", "Run command") or
                                           find_node("push button", "Request command"))
                            outcomes.append({"step":"command-execution-control","pass":runner is not None,
                                             "button":str(runner.name) if runner else None})
                            if runner:
                                runner.queryAction().doAction(0)
                                # The app's terminal records may be below a virtual
                                # viewport and omitted by AT-SPI. Check ONLY this
                                # task-owned, private XDG terminal history instead.
                                path=RUNTIME/"data"/"app.repotunnel.desktop"/"terminal-history.json"
                                def completed():
                                    try:
                                        entries=json.loads(path.read_text())
                                        for item in entries:
                                            record=item.get("record",item)
                                            if (record.get("command") == "printf " + sentinel
                                                and record.get("status") in ("completed", "failed", "rejected", "timedOut")):
                                                return record
                                    except (OSError,ValueError,TypeError):
                                        pass
                                    return None
                                result_record=until(completed,timeout=8)
                                verified=bool(result_record and result_record.get("status")=="completed"
                                               and result_record.get("exitCode")==0
                                               and result_record.get("stdout")==sentinel)
                                outcomes.append({"step":"synthetic-command-completed",
                                                 "pass":verified,"status":result_record.get("status") if result_record else None,
                                                 "exitCode":result_record.get("exitCode") if result_record else None,
                                                 "stdoutMatched":result_record.get("stdout")==sentinel if result_record else False})
                    except Exception as error:
                        outcomes.append({"step":"synthetic-command-workflow",
                                         "pass":False,"error":repr(error)[:220]})
                if MODE == "process-output-dev-preview":
                    # Only an ephemeral, side-effect-free printf in the private
                    # project. Never open or stop an existing user process.
                    try:
                        commands = until(lambda: find_node("push button", "Commands"))
                        if commands:
                            commands.queryAction().doAction(0)
                        field = until(lambda: find_node(name="Persistent process command"), timeout=6)
                        outcomes.append({"step":"open-native-managed-process-input",
                                         "pass":bool(commands and field)})
                        if field:
                            sentinel = "synthetic-process-output-" + str(time.time_ns())[-9:]
                            command = "printf " + sentinel
                            field.queryComponent().grabFocus()
                            entered = subprocess.run(
                                ["/usr/bin/xdotool", "type", "--clearmodifiers", "--delay", "18",
                                 command],
                                env=dict(os.environ, DISPLAY=os.environ["DISPLAY"],
                                         XAUTHORITY=os.environ["XAUTHORITY"]),
                                capture_output=True, text=True, timeout=13)
                            outcomes.append({"step":"type-safe-private-managed-process",
                                             "pass":entered.returncode==0})
                            start = until(lambda: find_node("push button", "Start process") or
                                          find_node("push button", "Request start"))
                            outcomes.append({"step":"native-managed-process-start-control",
                                             "pass":bool(start)})
                            if start and entered.returncode == 0:
                                start.queryAction().doAction(0)
                                history = RUNTIME / "data" / "app.repotunnel.desktop" / "process-history.json"
                                def get_process():
                                    try:
                                        value=json.loads(history.read_text())
                                        candidates=value if isinstance(value,list) else value.get("records",value.get("processes",[]))
                                        for item in candidates:
                                            entry=item.get("record",item)
                                            if entry.get("command")==command:
                                                return entry
                                    except (OSError, ValueError, TypeError, AttributeError):
                                        pass
                                    return None
                                found=until(get_process,timeout=12)
                                outcomes.append({"step":"synthetic-managed-process-stored-privately",
                                                 "pass":bool(found and history.resolve().is_relative_to(RUNTIME.resolve())),
                                                 "status":found.get("status") if found else None})
                                output=until(lambda: find_node("push button", "Output"),timeout=9)
                                outcomes.append({"step":"native-process-output-button-available",
                                                 "pass":bool(output)})
                                if output:
                                    output.queryAction().doAction(0)
                                    view=until(lambda: find_node("push button","Hide output"),timeout=4)
                                    outcomes.append({"step":"native-process-output-panel-expanded",
                                                     "pass":bool(view)})
                                    # Exercise the real manual Refresh path after
                                    # the synthetic printf has exited. Do not
                                    # start/stop/restart real or synthetic agents.
                                    exited=until(lambda: (
                                        item if (item:=get_process()) and item.get("status")=="exited"
                                        else None), timeout=8)
                                    outcomes.append({"step":"private-synthetic-process-completed",
                                        "pass":bool(exited and exited.get("exitCode")==0)})
                                    refresh=until(lambda: find_node("push button","Refresh"),timeout=5)
                                    outcomes.append({"step":"native-commands-refresh-control",
                                        "pass":bool(refresh)})
                                    if refresh:
                                        refresh.queryAction().doAction(0)
                                    still_open=until(lambda: find_node(
                                        "push button","Hide output"),timeout=6)
                                    outcomes.append({"step":"process-log-visible-after-manual-refresh",
                                        "pass":bool(refresh and still_open)})
                                    time.sleep(0.5)
                                    screen=d.screen()
                                    pixels=screen.root.get_image(0,0,screen.width_in_pixels,
                                        screen.height_in_pixels,X.ZPixmap,0xffffffff)
                                    if pixels:
                                        from PIL import Image
                                        frame=Image.frombytes("RGB",
                                            (screen.width_in_pixels,screen.height_in_pixels),
                                            pixels.data,"raw","BGRX")
                                        proof_file=TASK / "native-managed-process-output-isolated.png"
                                        frame.save(proof_file)
                                        outcomes.append({"step":"actual-native-process-output-screen",
                                            "pass":proof_file.is_file() and proof_file.stat().st_size>20000})
                    except Exception as error:
                        outcomes.append({"step":"synthetic-managed-process-output-workflow",
                                         "pass":False,"error":repr(error)[:240]})
                result={"scenario":f"private current frontend with {backend_label}, synthetic modal and optional Commands workflow",
                        "steps":outcomes,"passed":sum(1 for s in outcomes if s["pass"]),
                        "total":len(outcomes)}
                name=("native-preview-ai-workspace-permissions-workflow.json" if MODE == "ai-workspace-permissions-dev-preview" else
                      "native-preview-explorer-safety-workflow.json" if MODE == "explorer-safety-dev-preview" else
                      "native-preview-editor-workflow.json" if MODE == "editor-dev-preview" else
                      "native-preview-command-workflow.json" if MODE == "commands-dev-preview" else
                      "native-preview-workflows.json")
                (TASK/name).write_text(json.dumps(result,indent=2))
                print(json.dumps({"workflowAudit":result}),flush=True)
            if MODE == "inspect-release":
                try:
                    import pyatspi
                    registry = pyatspi.Registry.getDesktop(0)
                    accessibility = []
                    def visit(node, depth=0):
                        if len(accessibility) >= 280 or depth > 15:
                            return
                        name = str(getattr(node, "name", "") or "")[:90]
                        role = str(node.getRoleName())
                        if name or role in ("push button", "page tab", "entry", "heading"):
                            accessibility.append({"depth": depth, "role": role, "name": name})
                        for idx in range(min(int(getattr(node, "childCount", 0)), 100)):
                            try:
                                visit(node.getChildAtIndex(idx), depth+1)
                            except Exception:
                                pass
                    for app in registry:
                        if "repotunnel" in str(getattr(app, "name", "") or "").lower():
                            visit(app)
                    print(json.dumps({"atspiElements":accessibility[:280]}), flush=True)
                except Exception as error:
                    print(json.dumps({"atspiError":str(error)}),flush=True)
            if MODE in ("audit-release", "pixel-release", "audit-dev-preview", "pixel-dev-preview", "soak-dev-preview"):
                pixel_mode = MODE in ("pixel-release", "pixel-dev-preview", "soak-dev-preview")
                import math
                import statistics
                import pyatspi
                def all_nodes(node, depth=0):
                    if depth > 16:
                        return
                    yield node
                    for index in range(min(int(getattr(node, "childCount", 0)), 100)):
                        try:
                            yield from all_nodes(node.getChildAtIndex(index), depth + 1)
                        except Exception:
                            continue
                def project_app():
                    for app in pyatspi.Registry.getDesktop(0):
                        try:
                            if "repotunnel" in str(getattr(app, "name", "") or "").lower():
                                return app
                        except Exception:
                            # WebKit may destroy an accessibility object as a
                            # view is replaced. The next poll gets a fresh tree.
                            continue
                    return None
                def heading_names():
                    app = project_app()
                    names = []
                    if app:
                        for node in all_nodes(app):
                            try:
                                if node.getRoleName() == "heading":
                                    names.append(str(node.name or ""))
                            except Exception:
                                continue
                    return names
                def nav_button(label):
                    app = project_app()
                    if app:
                        for node in all_nodes(app):
                            try:
                                if node.getRoleName() == "push button" and node.name == label:
                                    return node
                            except Exception:
                                continue
                    return None
                def process_resources():
                    # Read only the process family this test started, never a
                    # production PID or a host-wide process memory summary.
                    try:
                        all_pid = [int(value) for value in os.listdir("/proc") if value.isdigit()]
                        descendants = {process.pid}
                        for _ in range(6):
                            for pid in all_pid:
                                if pid in descendants:
                                    continue
                                try:
                                    stats = Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()
                                    if int(stats[1]) in descendants:
                                        descendants.add(pid)
                                except Exception:
                                    continue
                        rss = 0
                        pss = 0
                        private = 0
                        smaps_count = 0
                        ticks = 0
                        found = 0
                        for pid in descendants:
                            try:
                                stats = Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1].split()
                                ticks += int(stats[11]) + int(stats[12])
                                found += 1
                                for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                                    if line.startswith("VmRSS:"):
                                        rss += int(line.split()[1])
                                        break
                                try:
                                    rollup = Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()
                                    pss += next(int(s.split()[1]) for s in rollup if s.startswith("Pss:"))
                                    private += sum(int(s.split()[1]) for s in rollup
                                                   if s.startswith(("Private_Clean:", "Private_Dirty:")))
                                    smaps_count += 1
                                except (OSError, StopIteration, ValueError):
                                    pass
                            except Exception:
                                continue
                        return {"rssKiB":rss,"pssKiB":pss if smaps_count == found else None,
                                "privateKiB":private if smaps_count == found else None,
                                "cpuSec":round(ticks/os.sysconf("SC_CLK_TCK"),3),
                                "processCount":found}
                    except Exception:
                        return {"rssKiB":None,"pssKiB":None,"privateKiB":None,
                                "cpuSec":None,"processCount":None}
                targets = [
                    "Projects", "Agents", "GPT Agents", "History", "Checks",
                    "Git", "Commands", "Video", "Phone", "Computers",
                    "Connect", "Guide", "Settings", "Help", "Home",
                ]
                # Extend only the explicitly isolated soak mode to sustained runs.
                # Existing navigation modes retain their original 20-pass cap.
                cap = 100 if MODE == "soak-dev-preview" else 20
                default_passes = 60 if MODE == "soak-dev-preview" else 2
                passes = min(cap, max(1, int(sys.argv[2]) if len(sys.argv) > 2 else default_passes))
                results = []
                rss_trace = []
                idle_trace = []
                soak_started = time.monotonic()
                initial_resources = process_resources()
                for pass_number in range(passes):
                    for label in targets:
                        try:
                            button = nav_button(label)
                            if button is None:
                                results.append({"label": label, "status": "missing-button"})
                                continue
                            before = heading_names() if not pixel_mode else []
                            if pixel_mode:
                                rect = (580, 100, 560, 170)
                                frame = d.screen().root.get_image(*rect, X.ZPixmap, 0xffffffff)
                                previous_pixels = frame.data if frame else None
                            start = time.perf_counter()
                            performed = button.queryAction().doAction(0)
                            detected = []
                            changed = False
                            deadline = time.monotonic() + 1.5
                            while time.monotonic() < deadline:
                                if pixel_mode:
                                    frame = d.screen().root.get_image(*rect, X.ZPixmap, 0xffffffff)
                                    changed = bool(previous_pixels is not None and frame
                                                   and frame.data != previous_pixels)
                                    if changed:
                                        break
                                else:
                                    detected = heading_names()
                                    changed = bool(detected and detected != before)
                                    if changed:
                                        break
                                time.sleep(0.012)
                            # A lazy view can first reveal only the AppHeader
                            # while its chunk is loading. Require real child
                            # headings on the semantic navigation audit.
                            expected_child = (
                                {
                                    "Agents": "Autonomous project mission",
                                    # A clean test profile has no project; after selecting
                                    # a synthetic project the full Team heading appears.
                                    "GPT Agents": ("AI Team", "Two AI engineers. One project."),
                                    "History": "Changes & history",
                                    "Checks": "End-to-end readiness",
                                    "Git": "Repository control",
                                    "Commands": "Terminal, processes, apps & browser",
                                    "Video": "Video Projects",
                                    "Computers": "Remote Computers",
                                    "Settings": "Runtime diagnostics",
                                    "Help": "Use RepoTunnel with confidence",
                                }.get(label)
                                if MODE == "audit-dev-preview" else None
                            )
                            child_ready = (lambda headings: any(name in headings for name in expected_child)
                                           if isinstance(expected_child, tuple) else expected_child in headings)
                            if expected_child:
                                child_deadline = time.monotonic() + 3
                                while time.monotonic() < child_deadline:
                                    detected = heading_names()
                                    if child_ready(detected):
                                        break
                                    time.sleep(0.03)
                                changed = changed and child_ready(detected)
                            expected_module = (
                                {
                                    "Agents": "AgentsPanel", "GPT Agents": "TeamPanel",
                                    "History": "ChangeHistoryPanel", "Checks": "WorkflowPanel",
                                    "Git": "GitPanel", "Commands": "ExecutionPanel",
                                    "Video": "VideoPanel", "Phone": "PhonePanel",
                                    "Computers": "RemoteMachinesPanel", "Settings": "ProductionPanel",
                                }.get(label) if MODE == "audit-dev-preview" else None
                            )
                            chunk_fetched = None
                            if expected_module:
                                chunk_path = next((p.name for p in (ROOT / "dist" / "assets").glob(expected_module + "-*.js")), None)
                                log_path = RUNTIME / "run" / "static-preview.log"
                                chunk_fetched = False
                                deadline_chunk = time.monotonic() + 3
                                while time.monotonic() < deadline_chunk:
                                    try:
                                        if chunk_path is not None and chunk_path in log_path.read_text():
                                            chunk_fetched = True
                                            break
                                    except OSError:
                                        pass
                                    time.sleep(0.05)
                                changed = changed and chunk_fetched
                            ms = (time.perf_counter() - start) * 1000
                            if pixel_mode:
                                detected = heading_names()
                            results.append({"label": label, "status":
                                            ("pixels-changed" if pixel_mode else "heading-changed")
                                            if performed and changed else "unconfirmed",
                                            "ms": round(ms, 2), "headings": detected[:4],
                                            "lazyChildHeadingReady": child_ready(detected) if expected_child else None,
                                            "lazyChunkFetched": chunk_fetched,
                                            "pass": pass_number + 1})
                            if pass_number < 2 and label in ("Projects", "GPT Agents", "Settings"):
                                shot = d.screen().root.get_image(
                                    0, 0, d.screen().width_in_pixels, d.screen().height_in_pixels,
                                    X.ZPixmap, 0xffffffff)
                                if shot:
                                    from PIL import Image
                                    raw = Image.frombytes("RGB", (d.screen().width_in_pixels,
                                                            d.screen().height_in_pixels),
                                                          shot.data, "raw", "BGRX")
                                    raw.save(EVIDENCE / f"native-{label.lower().replace(' ','-')}-pass{pass_number+1}.png")
                        except Exception as error:
                            results.append({"label": label, "status": "error",
                                            "message": str(error)[:160]})
                    rss_trace.append({"pass": pass_number + 1,
                                      "elapsedSec":round(time.monotonic()-soak_started,2),
                                      **process_resources()})
                    if MODE == "soak-dev-preview" and (pass_number + 1) % 5 == 0:
                        idle_before = process_resources()
                        time.sleep(10)
                        idle_trace.append({"afterPass":pass_number + 1,
                                           "before":idle_before,
                                           "after":process_resources()})
                    elif MODE != "soak-dev-preview" and passes >= 6 and pass_number + 1 == passes // 2:
                        time.sleep(20)  # short idle soak in the same isolated process
                final_resources = process_resources()
                expected_success = "pixels-changed" if pixel_mode else "heading-changed"
                good = sorted(item["ms"] for item in results
                              if item.get("status") == expected_success)
                summary = {
                    "scenario": ("current frontend dist + isolated static loopback preview + " if preview_mode else "") + backend_label + "; " + ("click-to-visible ROI X11 pixel latency" if pixel_mode else "AT-SPI action-to-heading latency"),
                    "samples": len(results), "semanticSuccesses": len(good),
                    "rssStartKiB": initial_resources["rssKiB"],
                    "rssEndKiB": final_resources["rssKiB"],
                    "cpuSecondsStart": initial_resources["cpuSec"],
                    "cpuSecondsEnd": final_resources["cpuSec"],
                    "rssTrace": rss_trace, "idleTrace": idle_trace, "passes": passes,
                    "durationSeconds":round(time.monotonic()-soak_started,2),
                    "pssStartKiB":initial_resources["pssKiB"],
                    "pssEndKiB":final_resources["pssKiB"],
                    "privateStartKiB":initial_resources["privateKiB"],
                    "privateEndKiB":final_resources["privateKiB"],
                    "nativeProcessAlive":process.poll() is None,
                    "allNavigationVerified":len(good)==len(results),
                    "soakPassed":len(good)==len(results) and process.poll() is None,
                    "p50Ms": round(statistics.median(good), 2) if good else None,
                    "p95Ms": round(good[math.ceil(len(good)*.95)-1], 2) if good else None,
                    "worstMs": round(good[-1], 2) if good else None,
                    "results": results,
                }
                (EVIDENCE / (("native-preview-" if preview_mode else "native-") + ("pixel-results.json" if pixel_mode else "navigation-results.json"))).write_text(json.dumps(summary, indent=2))
                if MODE == "soak-dev-preview":
                    soak_ok = summary["soakPassed"]
                    print(json.dumps({"soakEvidenceDir":str(EVIDENCE.relative_to(ROOT)),
                                      "navigationPassed":len(good), "navigationTotal":len(results),
                                      "nativeAlive":process.poll() is None}), flush=True)
                print(json.dumps({"navigationAudit": summary if passes <= 2 else
                                  {key: value for key, value in summary.items()
                                   if key != "results"}}), flush=True)
            # This DISPLAY was created by this invocation; no host pixels exist here.
            screen = d.screen()
            shot = screen.root.get_image(0, 0, screen.width_in_pixels,
                                         screen.height_in_pixels, X.ZPixmap, 0xffffffff)
            if shot:
                from PIL import Image
                raw = Image.frombytes("RGB", (screen.width_in_pixels, screen.height_in_pixels),
                                      shot.data, "raw", "BGRX")
                out = EVIDENCE / "native-window-isolated.png"
                raw.save(out)
                print(json.dumps({"screenshot": str(out.relative_to(ROOT)),
                                  "size": list(raw.size)}), flush=True)
        d.close()
        if MODE == "soak-dev-preview" and (not found or not soak_ok):
            raise RuntimeError("Isolated native soak failed: missing window, crash or incomplete navigation")
    finally:
        # Termination is strictly this child, owned by this test PID namespace.
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=7)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
print(json.dumps({"log": str(log_path.relative_to(ROOT)),
                  "exit": process.returncode}), flush=True)
