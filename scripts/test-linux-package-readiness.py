#!/usr/bin/env python3
"""Safe unit tests for read-only package status, no package execution/installation."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("readonly_pkg", ROOT / "scripts" / "check-linux-package-readiness.py")
assert SPEC and SPEC.loader
MOD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MOD)


class PackagePreflightTests(unittest.TestCase):
    def test_real_package_metadata_valid_but_not_current(self):
        report = MOD.inspect(ROOT)
        self.assertTrue(report["readOnly"])
        self.assertTrue(report["versionsAgree"])
        self.assertTrue(report["metadataValid"])
        self.assertFalse(report["currentBuild"])
        self.assertTrue(any("stale" in problem for problem in report["problems"]))

    def test_missing_package_is_not_reported_valid(self):
        with tempfile.TemporaryDirectory(prefix="repotunnel-deb-missing-") as directory:
            report = MOD.inspect(ROOT, Path(directory) / "absent.deb")
            self.assertFalse(report["packageExists"])
            self.assertFalse(report["metadataValid"])
            self.assertFalse(report["currentBuild"])

    def test_cargo_version_parser_checks_structured_tables(self):
        with tempfile.TemporaryDirectory(prefix="repotunnel-cargo-version-") as directory:
            manifest = Path(directory) / "Cargo.toml"
            manifest.write_text('[package]\nname="repotunnel"\nversion="4.5.6"\n[dependencies]\nfoo="1"\n')
            locked = Path(directory) / "Cargo.lock"
            locked.write_text('[[package]]\nname="different"\nversion="3.0.0"\n[[package]]\nname="repotunnel"\nversion="4.5.6"\n')
            self.assertEqual(MOD.cargo_package_version(manifest), "4.5.6")
            self.assertEqual(MOD.locked_package_version(locked), "4.5.6")

    def test_missing_or_ambiguous_lock_record_rejected(self):
        with tempfile.TemporaryDirectory(prefix="repotunnel-cargo-reject-") as directory:
            lockfile = Path(directory) / "Cargo.lock"
            lockfile.write_text('[[package]]\nname="different"\nversion="1"\n')
            with self.assertRaises(ValueError):
                MOD.locked_package_version(lockfile)
            lockfile.write_text('[[package]]\nname="repotunnel"\nversion="1"\n[[package]]\nname="repotunnel"\nversion="2"\n')
            with self.assertRaises(ValueError):
                MOD.locked_package_version(lockfile)

    def test_latest_input_detects_new_frontend_source(self):
        with tempfile.TemporaryDirectory(prefix="repotunnel-freshness-") as directory:
            root = Path(directory)
            (root / "src").mkdir()
            older = root / "package.json"
            newer = root / "src" / "latest.tsx"
            older.write_text("{}")
            newer.write_text("export const test = true;")
            os.utime(older, (100, 100))
            os.utime(newer, (200, 200))
            self.assertEqual(MOD.latest_input(root), newer)


if __name__ == "__main__":
    unittest.main()
