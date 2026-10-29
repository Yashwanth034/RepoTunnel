"""AI Workspace app ownership checks; does not open a real X display."""
import importlib.util
import pathlib
import unittest
from unittest import mock
from types import SimpleNamespace

MODULE_PATH = pathlib.Path(__file__).with_name("ai_workspace.py")
spec = importlib.util.spec_from_file_location("repotunnel_aiw_helper_security", MODULE_PATH)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class Application:
    def __init__(self, pid):
        self.pid = pid

    def get_process_id(self):
        return self.pid


class SemanticElement:
    def __init__(self, pid):
        self.application = Application(pid)
        self.actions_attempted = 0

    def getApplication(self):
        return self.application

    def queryAction(self):
        self.actions_attempted += 1
        return SimpleNamespace(
            nActions=1,
            getName=lambda _: "click",
            doAction=lambda _: True,
        )


class SessionOwnershipTests(unittest.TestCase):
    def setUp(self):
        self.window = SimpleNamespace(id=0x11)
        self.bounds = {"x": 0, "y": 0, "width": 600, "height": 400}
        self.center = {"x": 20, "y": 20, "width": 40, "height": 30}
        self.display = SimpleNamespace(close=lambda: None)
        self.patchers = [
            mock.patch.object(helper, "open_display", return_value=self.display),
            mock.patch.object(helper, "client_windows", return_value=[(self.window, "App")]),
            mock.patch.object(helper, "window_info", side_effect=lambda *args: {"pid": 111}),
            mock.patch.object(helper, "window_bounds", return_value=self.bounds),
            mock.patch.object(helper, "semantic_bounds", return_value=self.center),
            mock.patch.object(helper, "resolve_semantic_element"),
            mock.patch.object(helper, "semantic_actions", return_value=["click"]),
        ]
        for patcher in self.patchers:
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_owned_node_can_access_its_matching_owned_window(self):
        node = SemanticElement(111)
        self.assertEqual(
            helper.semantic_allowed_window(node, ["0x11"], [111]),
            ("0x11", self.bounds),
        )

    def test_overlapping_other_app_cannot_invoke_accessibility_action(self):
        node = SemanticElement(222)
        helper.resolve_semantic_element.return_value = node
        with self.assertRaisesRegex(RuntimeError, "another application"):
            helper.semantic_click_element("a0.w0.0#good", ["0x11"], [111])
        self.assertEqual(node.actions_attempted, 0)

    def test_foreign_accessibility_node_cannot_type_even_when_windows_overlap(self):
        node = SemanticElement(222)
        node.queryEditableText = mock.Mock(
            side_effect=AssertionError("foreign editable text must never be reached")
        )
        helper.resolve_semantic_element.return_value = node
        with self.assertRaisesRegex(RuntimeError, "another application"):
            helper.semantic_type_element("a0.w0.1#foreign", "private data", False, ["0x11"], [111])
        node.queryEditableText.assert_not_called()

    def test_app_session_window_lookup_rechecks_live_x11_pid(self):
        self.assertEqual(
            helper.target_window(self.display, "0x11", [111])[0].id,
            0x11,
        )
        with mock.patch.object(helper, "window_info", return_value={"pid": 222}):
            with self.assertRaisesRegex(RuntimeError, "current process"):
                helper.target_window(self.display, "0x11", [111])
        with self.assertRaisesRegex(RuntimeError, "verified window"):
            helper.target_window(self.display, None, [111])
        with self.assertRaisesRegex(RuntimeError, "verified window"):
            helper.target_window(self.display, "0x11", [])

    def test_foreign_window_frame_and_shortcut_fail_before_capture_or_typing(self):
        with mock.patch.object(helper, "window_info", return_value={"pid": 222}):
            with self.assertRaisesRegex(RuntimeError, "current process"):
                helper.frame({"windowId": "0x11", "allowedPids": [111]})
            with self.assertRaisesRegex(RuntimeError, "current process"):
                helper.shortcut({
                    "windowId": "0x11",
                    "allowedPids": [111],
                    "shortcut": "Ctrl+S",
                })

    def test_scoped_sequence_ignores_step_supplied_process_override(self):
        request = {
            "windowId": "0x11",
            "allowedWindowIds": ["0x11"],
            "allowedPids": [111],
            "steps": [
                {
                    "operation": "activate",
                    "windowId": "0x11",
                    "allowedPids": [222],
                }
            ],
        }
        with mock.patch.object(helper, "main", side_effect=lambda step: dict(step)):
            result = helper.sequence(request)
        self.assertEqual(result["results"][0]["result"]["allowedPids"], [111])

    def test_app_session_wait_filters_other_windows_and_active_titles(self):
        second_window = SimpleNamespace(id=0x22)
        self.display.sync = lambda: None
        with (
            mock.patch.object(
                helper, "client_windows",
                return_value=[(self.window, "Mine"), (second_window, "Other AI")],
            ),
            mock.patch.object(
                helper, "window_info",
                side_effect=lambda d, win, title: {
                    "pid": 111 if win.id == 0x11 else 222,
                },
            ),
            mock.patch.object(helper, "active_window", return_value=second_window),
        ):
            result = helper.wait_step({
                "allowedPids": [111],
                "windowCountAtMost": 1,
                "timeoutMs": 100,
            })
        self.assertEqual(result["windowCount"], 1)
        self.assertEqual(result["activeTitle"], "")

    def test_unknown_application_pid_fails_closed(self):
        node = SemanticElement(None)
        with self.assertRaisesRegex(RuntimeError, "another application"):
            helper.semantic_allowed_window(node, ["0x11"], [111])

    def test_missing_app_session_scope_fails_closed(self):
        node = SemanticElement(111)
        for windows, pids in [([], [111]), (["0x11"], []), ([], [])]:
            with self.subTest(windows=windows, pids=pids):
                with self.assertRaisesRegex(RuntimeError, "verified app-session"):
                    helper.semantic_allowed_window(node, windows, pids)

    def test_matching_app_pid_with_other_x11_window_pid_is_rejected(self):
        node = SemanticElement(111)
        with mock.patch.object(helper, "window_info", return_value={"pid": 222}):
            with self.assertRaisesRegex(RuntimeError, "outside this AI"):
                helper.semantic_allowed_window(node, ["0x11"], [111])

    def test_application_pid_parser_rejects_invalid_or_missing_values(self):
        for value in [None, "", 0, -1, "not-a-pid", True]:
            with self.subTest(value=value):
                self.assertIsNone(helper.application_process_id(Application(value)))
        self.assertEqual(helper.application_process_id(Application("111")), 111)


class SemanticDiscoveryTests(unittest.TestCase):
    def test_inspection_tags_owned_nodes_and_hides_embedded_foreign_processes(self):
        class Node:
            def __init__(self, owner, name, children=()):
                self.owner = owner
                self.name = name
                self.description = ""
                self.children = list(children)
                self.childCount = len(children)

            def getApplication(self):
                return self.owner

            def getChildAtIndex(self, index):
                return self.children[index]

        class App(Application):
            def __init__(self, pid, name):
                super().__init__(pid)
                self.name = name
                self.children = []
                self.childCount = 0

            def getChildAtIndex(self, index):
                return self.children[index]

        mine = App(111, "Owned Editor")
        foreign = App(222, "Other Agent")
        embedded = Node(foreign, "injected-other-process")
        mine.children = [Node(mine, "owned window", [embedded])]
        mine.childCount = 1
        foreign.children = [Node(foreign, "foreign window")]
        foreign.childCount = 1
        desktop = SimpleNamespace(
            childCount=2,
            getChildAtIndex=lambda index: [mine, foreign][index],
        )
        fake_pyatspi = SimpleNamespace(
            Registry=SimpleNamespace(getDesktop=lambda index: desktop),
        )
        with (
            mock.patch.object(helper, "pyatspi", fake_pyatspi),
            mock.patch.dict(helper.os.environ, {"DBUS_SESSION_BUS_ADDRESS": "private-session"}),
            mock.patch.object(helper, "semantic_sensitive", return_value=False),
            mock.patch.object(helper, "semantic_role_name", return_value="button"),
            mock.patch.object(helper, "semantic_actions", return_value=["click"]),
            mock.patch.object(helper, "semantic_bounds", return_value={
                "x": 10, "y": 10, "width": 50, "height": 20
            }),
            mock.patch.object(helper, "semantic_states", return_value=[]),
            mock.patch.object(helper, "semantic_text", return_value=""),
        ):
            inspection = helper.inspect_semantics(20, allowed_pids=[111])
            none_scoped = helper.inspect_semantics(20, allowed_pids=None)
            explicitly_empty = helper.inspect_semantics(20, allowed_pids=[])
        self.assertEqual(explicitly_empty["elements"], [])
        self.assertIn(
            "foreign window",
            [node["name"] for node in none_scoped["elements"]],
            "intentionally unscoped internal inspection must retain existing behavior",
        )
        names = [node["name"] for node in inspection["elements"]]
        self.assertIn("owned window", names)
        self.assertNotIn("injected-other-process", names)
        self.assertNotIn("foreign window", names)
        self.assertTrue(all(node["pid"] == 111 for node in inspection["elements"]))


if __name__ == "__main__":
    unittest.main()
