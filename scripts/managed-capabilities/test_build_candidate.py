import io
from pathlib import Path, PurePosixPath
import stat
import tarfile
import tempfile
import unittest
import zipfile

import build_candidate as builder


class ManagedCapabilityBuilderTests(unittest.TestCase):
    def test_supported_platform_maps_are_complete(self):
        expected = {
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
        }
        self.assertEqual(set(builder.AST_GREP_ASSETS), expected)
        self.assertEqual(set(builder.NODE_ASSETS), expected)

    def test_rea_producer_node_ranges_match_runtime_contract(self):
        for good in ["v22.19.0", "22.22.1", "v24.11.0", "26.0.0", "30.1.2"]:
            self.assertTrue(builder.producer_node_supported(good), good)
        for bad in ["v20.20.2", "v22.18.9", "v23.9.0", "v24.10.9", "v25.0.0", "nope"]:
            self.assertFalse(builder.producer_node_supported(bad), bad)

    def test_upstream_urls_are_https_and_allowlisted(self):
        for good in [
            "https://github.com/ast-grep/ast-grep/releases/download/0.45.3/app.zip",
            "https://nodejs.org/dist/v22.19.0/node-v22.19.0-linux-x64.tar.gz",
            "https://registry.npmjs.org/rea-agents/-/rea-agents-6.3.0.tgz",
        ]:
            builder._assert_https_allowlisted(good)
        for bad in [
            "http://nodejs.org/dist/v22.19.0/node.tar.gz",
            "https://user@nodejs.org/dist/v22.19.0/node.tar.gz",
            "https://example.com/rea.tgz",
            "file:///tmp/rea.tgz",
        ]:
            with self.assertRaises(ValueError, msg=bad):
                builder._assert_https_allowlisted(bad)

    def test_node_tar_extractor_ignores_unselected_symlink_but_requires_regular_node(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive_path = root / "node.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive:
                node = tarfile.TarInfo("node-v22.19.0-linux-x64/bin/node")
                node_bytes = b"node-binary"
                node.size = len(node_bytes)
                node.mode = 0o755
                archive.addfile(node, io.BytesIO(node_bytes))

                npm = tarfile.TarInfo("node-v22.19.0-linux-x64/bin/npm")
                npm.type = tarfile.SYMTYPE
                npm.linkname = "../lib/node_modules/npm/bin/npm-cli.js"
                archive.addfile(npm)

            output = root / "content/bin/node"
            builder._extract_unique_tar_suffix(archive_path, PurePosixPath("bin/node"), output)
            self.assertEqual(output.read_bytes(), b"node-binary")

    def test_zip_builder_rejects_symlink_content(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            content = root / "content"
            content.mkdir()
            target = content / "real"
            target.write_text("ok", encoding="utf-8")
            link = content / "linked"
            try:
                link.symlink_to(target)
            except OSError:
                self.skipTest("symlinks unavailable on this platform")
            with self.assertRaises(ValueError):
                builder._write_zip(content, root / "candidate.zip")

    def test_ast_grep_zip_selects_exactly_one_binary_basename(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archive_path = root / "upstream.zip"
            with zipfile.ZipFile(archive_path, "w") as archive:
                info = zipfile.ZipInfo("bundle/ast-grep")
                info.external_attr = (stat.S_IFREG | 0o755) << 16
                archive.writestr(info, b"binary")
                archive.writestr("bundle/README.md", "readme")
            output = root / "content/ast-grep"
            builder._extract_unique_zip_basename(archive_path, "ast-grep", output)
            self.assertEqual(output.read_bytes(), b"binary")


if __name__ == "__main__":
    unittest.main()
