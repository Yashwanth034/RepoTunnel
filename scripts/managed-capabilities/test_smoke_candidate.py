import unittest

import smoke_candidate as smoke


class SmokeCandidateTests(unittest.TestCase):
    def test_entrypoints_are_adapter_owned(self):
        self.assertEqual(smoke.entrypoint("ast-grep", "linux"), "ast-grep")
        self.assertEqual(smoke.entrypoint("ast-grep", "windows"), "ast-grep.exe")
        self.assertEqual(smoke.entrypoint("rea-node", "macos"), "bin/node")
        self.assertEqual(smoke.entrypoint("rea-node", "windows"), "node.exe")
        with self.assertRaises(ValueError):
            smoke.entrypoint("shell", "linux")


if __name__ == "__main__":
    unittest.main()
