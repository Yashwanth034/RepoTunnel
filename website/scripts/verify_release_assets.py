#!/usr/bin/env python3
"""Verify website download links against a real GitHub release (opt-in network check).

Default: request the public GitHub Releases API. Use --fixture to verify the
same invariants with a captured API JSON payload in an offline regression test.
Never downloads executable installers and never logs access tokens.
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit, unquote
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / "src" / "data" / "site-content.json"
DOWNLOAD_KEYS = (
    "linuxDeb", "linuxRpm", "linuxAppImage", "windowsExe", "windowsMsi", "macArm", "macIntel"
)

def validate(release: dict, config: dict) -> None:
    version = config["version"]
    tag = f"v{version}"
    expected_release_url = f"https://github.com/Yashwanth034/RepoTunnel/releases/tag/{tag}"
    expected_asset_base = f"https://github.com/Yashwanth034/RepoTunnel/releases/download/{tag}/"
    if config["releaseUrl"] != expected_release_url:
        raise ValueError("releaseUrl must use the official version tag")
    if release.get("tag_name") != tag or release.get("draft") or release.get("prerelease"):
        raise ValueError("GitHub release tag or published status does not match the website")
    assets = release.get("assets")
    if not isinstance(assets, list) or not assets:
        raise ValueError("official release has no asset list")
    names = {a.get("name") for a in assets if isinstance(a, dict)}
    downloads = config["downloads"]
    if set(downloads) != set(DOWNLOAD_KEYS):
        raise ValueError("expected exactly seven OS/architecture download destinations")
    for platform, url in downloads.items():
        parts = urlsplit(url)
        filename = unquote(parts.path.rsplit("/", 1)[-1])
        if not url.startswith(expected_asset_base) or parts.query or parts.fragment:
            raise ValueError(f"{platform}: unsafe or unexpected asset URL")
        if version not in filename or filename not in names:
            raise ValueError(f"{platform}: {filename} is not an asset published in {tag}")
        print(f"PASS {platform}: {filename}")
    checksum = config["checksums"]
    expected_checksum = expected_asset_base + "RepoTunnel-SHA256SUMS.txt"
    if checksum != expected_checksum or "RepoTunnel-SHA256SUMS.txt" not in names:
        raise ValueError("official release is missing the configured checksum document")
    print(f"RELEASE_ASSETS_PASS: {len(DOWNLOAD_KEYS)} installers + checksum found in {tag} ({len(names)} total published assets).")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path, help="Offline JSON fixture for deterministic test")
    args = parser.parse_args()
    config = json.loads(CONFIG.read_text())["site"]["release"]
    if args.fixture is not None:
        release = json.loads(args.fixture.read_text())
    else:
        url = ("https://api.github.com/repos/Yashwanth034/RepoTunnel/"
               f"releases/tags/v{config['version']}")
        req = Request(url, headers={
            "Accept": "application/vnd.github+json",
            "User-Agent": "RepoTunnel-Website-Release-Asset-Validator",
            "X-GitHub-Api-Version": "2022-11-28",
        })
        try:
            with urlopen(req, timeout=15) as response:
                release = json.load(response)
        except (URLError, HTTPError, TimeoutError, OSError) as exc:
            print(f"RELEASE_ASSETS_UNAVAILABLE: {type(exc).__name__}; "
                  "GitHub could not be queried. Local asset-name checks still run in npm run check.", file=sys.stderr)
            return 2
    try:
        validate(release, config)
    except (ValueError, KeyError, TypeError) as exc:
        print(f"RELEASE_ASSETS_FAIL: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
