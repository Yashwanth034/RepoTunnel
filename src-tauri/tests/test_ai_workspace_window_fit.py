"""Regression checks for the isolated native AI Workspace window-fit helper."""

import importlib.util
from pathlib import Path
from types import SimpleNamespace
from unittest import TestCase, main
from unittest.mock import patch

HELPER = Path(__file__).resolve().parents[1] / "resources/ai_workspace/ai_workspace.py"
spec = importlib.util.spec_from_file_location("repotunnel_ai_workspace_fit", HELPER)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Window:
    def __init__(self, xid, pid, bounds, accepts_resize=True):
        self.id = xid
        self.pid = pid
        self.bounds = dict(bounds)
        self.accepts_resize = accepts_resize
        self.resizes = 0

    def configure(self, **kwargs):
        self.resizes += 1
        if self.accepts_resize:
            # Metacity accounts for the decorated titlebar.
            self.bounds.update(kwargs)
            self.bounds["y"] = 37


class Display:
    def __init__(self):
        self.closed = False
        self.maximize_requests = 0
        self._root = SimpleNamespace(send_event=self.send_event)

    def intern_atom(self, _name):
        return 1

    def send_event(self, *_args, **_kwargs):
        self.maximize_requests += 1

    def screen(self):
        return SimpleNamespace(root=self._root)

    def sync(self):
        pass

    def close(self):
        self.closed = True


class WindowFittingTests(TestCase):
    def fit(self, windows, pids):
        display = Display()
        def info(_d, win, title):
            return {
                "pid": win.pid,
                "windowId": f"0x{win.id:x}",
                "bounds": dict(win.bounds),
            }
        with (
            patch.object(module, "open_display", return_value=display),
            patch.object(module, "root_size", return_value=(1440, 900)),
            patch.object(module, "client_windows", return_value=[(win, "Owned App") for win in windows]),
            patch.object(module, "window_info", side_effect=info),
            patch.object(module, "window_bounds", side_effect=lambda win: dict(win.bounds)),
            patch.object(module.protocol.event, "ClientMessage", return_value=object()),
            patch.object(module.time, "sleep", return_value=None),
        ):
            result = module.fit_owned_windows({"allowedPids": pids})
        self.assertTrue(display.closed)
        return result

    def test_resizes_only_verified_oversized_app_window(self):
        owned = Window(1, 100, {"x": 10, "y": 20, "width": 1640, "height": 1000})
        other = Window(2, 200, {"x": 50, "y": 40, "width": 1640, "height": 1000})
        fitted = Window(3, 100, {"x": 0, "y": 37, "width": 1440, "height": 863})
        result = self.fit([owned, other, fitted], [100])
        self.assertEqual([item["windowId"] for item in result["fitted"]], ["0x1"])
        self.assertEqual(owned.bounds["width"], 1424)
        self.assertLessEqual(owned.bounds["y"] + owned.bounds["height"], 900)
        self.assertEqual(other.resizes, 0)
        self.assertEqual(fitted.resizes, 0)

    def test_refuses_empty_ownership_before_inspecting_display(self):
        with self.assertRaisesRegex(RuntimeError, "nonempty owned PID"):
            module.fit_owned_windows({"allowedPids": []})

    def test_refuses_if_the_owned_window_still_cannot_fit(self):
        stuck = Window(4, 100, {"x": 0, "y": 37, "width": 1650, "height": 1000}, accepts_resize=False)
        with self.assertRaisesRegex(RuntimeError, "still outside the isolated screen"):
            self.fit([stuck], [100])


if __name__ == "__main__":
    main()
