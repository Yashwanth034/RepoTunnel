#!/usr/bin/env python3
"""Read-only Linux release/package preflight. Never builds, installs, or mutates packages.

This intentionally distinguishes an inspectable old .deb from a CURRENT artifact
built from the latest modified frontend/backend sources. It does not certify
runtime behavior, cryptographic signatures or a complete packaging release.
"""
import argparse
import json
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path


def cargo_package_version(path: Path) -> str:
    return tomllib.loads(path.read_text(encoding="utf-8"))["package"]["version"]


def locked_package_version(path: Path) -> str:
    packages = tomllib.loads(path.read_text(encoding="utf-8"))["package"]
    matches = [record["version"] for record in packages if record.get("name") == "repotunnel"]
    if len(matches) != 1:
        raise ValueError("Cargo.lock must contain exactly one RepoTunnel entry")
    return matches[0]


def latest_input(root: Path):
    paths = [
        root / "package.json", root / "package-lock.json",
        root / "src-tauri" / "Cargo.toml", root / "src-tauri" / "Cargo.lock",
        root / "src-tauri" / "tauri.conf.json", root / "dist" / "index.html",
    ]
    for directory, patterns in [
        (root / "src", ("*.tsx", "*.ts", "*.css", "*.html")),
        (root / "src-tauri" / "src", ("*.rs",)),
    ]:
        if directory.is_dir():
            for pattern in patterns:
                paths.extend(directory.rglob(pattern))
    existing = [p for p in paths if p.is_file()]
    return max(existing, key=lambda p: p.stat().st_mtime) if existing else None


def read_deb_field(package: Path, field: str):
    result = subprocess.run(["dpkg-deb", "--field", str(package), field],
                            capture_output=True, text=True, check=True, timeout=15)
    return result.stdout.strip()


def inspect(root: Path, package: Path | None = None):
    versions = {
        "npm": json.loads((root / "package.json").read_text())["version"],
        "npmLock": json.loads((root / "package-lock.json").read_text())["version"],
        "tauri": json.loads((root / "src-tauri" / "tauri.conf.json").read_text())["version"],
        "cargo": cargo_package_version(root / "src-tauri" / "Cargo.toml"),
        "cargoLock": locked_package_version(root / "src-tauri" / "Cargo.lock"),
    }
    version_ok = len(set(versions.values())) == 1
    version = versions["tauri"]
    default_package = root / "src-tauri" / "target" / "release" / "bundle" / "deb" / f"RepoTunnel_{version}_amd64.deb"
    package = package or default_package
    report = {
        "project": "RepoTunnel",
        "readOnly": True,
        "versions": versions,
        "versionsAgree": version_ok,
        "packagePath": str(package),
        "packageExists": package.is_file(),
        "metadataValid": False,
        "currentBuild": False,
        "sourceProvenanceVerified": False,
        "freshnessSemantics": "Timestamp comparison only; cannot prove the package was produced from this exact source or detect a retouched archive",
        "problems": [],
    }
    if not version_ok:
        report["problems"].append("Frontend, Cargo and Tauri release versions disagree")
    if not package.is_file():
        report["problems"].append("No existing Linux Debian package at expected path")
        return report
    if not shutil.which("dpkg-deb"):
        report["problems"].append("dpkg-deb unavailable for read-only validation")
        return report
    try:
        meta = {key: read_deb_field(package, key) for key in ("Package", "Version", "Architecture", "Depends")}
        contents = subprocess.run(["dpkg-deb", "--contents", str(package)],
                                  capture_output=True, text=True, check=True, timeout=20).stdout
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError) as exc:
        report["problems"].append("Cannot read package metadata: " + str(exc)[:220])
        return report
    expected = ("usr/bin/repotunnel", "usr/share/applications/RepoTunnel.desktop",
                "usr/share/icons/hicolor/")
    present = {name: name in contents for name in expected}
    report["packageMetadata"] = meta
    report["expectedContent"] = present
    report["metadataValid"] = (
        version_ok and meta["Version"] == version and meta["Package"] == "repo-tunnel"
        and meta["Architecture"] == "amd64" and all(present.values())
    )
    if not report["metadataValid"]:
        report["problems"].append("Debian control metadata or application payload mismatch")
    latest = latest_input(root)
    if latest is not None:
        package_time = package.stat().st_mtime
        report["latestInput"] = str(latest.relative_to(root))
        report["latestInputMtime"] = latest.stat().st_mtime
        report["packageMtime"] = package_time
        report["currentBuild"] = report["metadataValid"] and package_time >= latest.stat().st_mtime
        if package_time < latest.stat().st_mtime:
            report["problems"].append("Debian archive predates latest source/frontend asset; stale, not a current release")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, help="Existing .deb to inspect, never execute")
    parser.add_argument("--require-current", action="store_true",
                        help="Fail when package is older than current source/assets (mtime only; not source attestation)")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    try:
        report = inspect(root, args.package)
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as exc:
        print(json.dumps({"project": "RepoTunnel", "readOnly": True, "error": str(exc)}))
        return 2
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        for k in ("versionsAgree", "packageExists", "metadataValid", "currentBuild"):
            print(k + ":", report[k])
        for problem in report["problems"]:
            print("BLOCKER:", problem)
    if not report["metadataValid"]:
        return 2
    return 0 if report["currentBuild"] or not args.require_current else 3


if __name__ == "__main__":
    sys.exit(main())
