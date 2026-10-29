import importlib.util
import io
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


def load_module(name: str, relative: str):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class HelperServeProtocolTests(unittest.TestCase):
    def exercise(self, module):
        source = io.StringIO(
            "\n".join(
                [
                    json.dumps({"id": 7, "request": {"value": "first"}}),
                    json.dumps({"id": 8, "request": {"value": "explode"}}),
                    json.dumps({"id": 9, "request": {"value": "second"}}),
                ]
            )
            + "\n"
        )
        sink = io.StringIO()
        calls = []

        def dispatch(request):
            calls.append(request["value"])
            if request["value"] == "explode":
                raise RuntimeError("expected failure")
            return {"echo": request["value"]}

        module.serve_stream(source, sink, dispatch)
        replies = [json.loads(line) for line in sink.getvalue().splitlines()]

        self.assertEqual(calls, ["first", "explode", "second"])
        self.assertEqual([reply["id"] for reply in replies], [7, 8, 9])
        self.assertEqual(replies[0], {"id": 7, "ok": True, "result": {"echo": "first"}})
        self.assertEqual(replies[1], {"id": 8, "ok": False, "error": "expected failure"})
        self.assertEqual(replies[2], {"id": 9, "ok": True, "result": {"echo": "second"}})

    def test_desktop_helper_serve_stream_correlates_multiple_requests(self):
        self.exercise(load_module("desktop_helper_test", "resources/desktop_control.py"))

    def test_desktop_helper_ping_is_process_local_and_needs_no_gui(self):
        module = load_module("desktop_helper_ping_test", "resources/desktop_control.py")
        self.assertEqual(module.main({"operation": "ping"}), {"ready": True})

    def test_ai_workspace_helper_serve_stream_correlates_multiple_requests(self):
        self.exercise(load_module("ai_workspace_helper_test", "resources/ai_workspace/ai_workspace.py"))


if __name__ == "__main__":
    unittest.main()
