#!/usr/bin/env python3
"""Build one RepoTunnel managed-capability candidate from an allowlisted upstream.

This script is for CI producer jobs only. It never signs metadata and never writes
outside its explicit output directory. Build jobs intentionally run without write
permissions or publication credentials because npm package lifecycle scripts may
execute while producing a platform-native REA bundle.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import subprocess
import tarfile
import tempfile
import urllib.parse
import urllib.request
import zipfile

MAX_DOWNLOAD_BYTES = 256 * 1024 * 1024
ALLOWED_HOSTS = {
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
    "nodejs.org",
    "registry.npmjs.org",
}

AST_GREP_ASSETS = {
    ("linux", "x86_64"): "app-x86_64-unknown-linux-gnu.zip",
    ("linux", "aarch64"): "app-aarch64-unknown-linux-gnu.zip",
    ("macos", "x86_64"): "app-x86_64-apple-darwin.zip",
    ("macos", "aarch64"): "app-aarch64-apple-darwin.zip",
    ("windows", "x86_64"): "app-x86_64-pc-windows-msvc.zip",
    ("windows", "aarch64"): "app-aarch64-pc-windows-msvc.zip",
}

NODE_ASSETS = {
    ("linux", "x86_64"): ("node-v{version}-linux-x64.tar.gz", "bin/node"),
    ("linux", "aarch64"): ("node-v{version}-linux-arm64.tar.gz", "bin/node"),
    ("macos", "x86_64"): ("node-v{version}-darwin-x64.tar.gz", "bin/node"),
    ("macos", "aarch64"): ("node-v{version}-darwin-arm64.tar.gz", "bin/node"),
    ("windows", "x86_64"): ("win-x64/node.exe", "node.exe"),
    ("windows", "aarch64"): ("win-arm64/node.exe", "node.exe"),
}


def _safe_relative(name: str) -> PurePosixPath:
    path = PurePosixPath(name)
    if path.is_absolute() or not path.parts or any(part in {"", ".", ".."} for part in path.parts):
        raise ValueError(f"unsafe archive path: {name!r}")
    return path


def _assert_https_allowlisted(url: str) -> None:
    parsed = urllib.parse.urlparse(url)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.hostname not in ALLOWED_HOSTS
        or parsed.username
        or parsed.password
        or parsed.port
        or parsed.fragment
    ):
        raise ValueError(f"untrusted managed-capability upstream URL: {url}")


class _AllowlistedRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        _assert_https_allowlisted(newurl)
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def download(url: str, destination: Path) -> dict[str, object]:
    _assert_https_allowlisted(url)
    destination.parent.mkdir(parents=True, exist_ok=True)
    request = urllib.request.Request(url, headers={"User-Agent": "RepoTunnel managed capability CI"})
    opener = urllib.request.build_opener(_AllowlistedRedirectHandler())
    digest = hashlib.sha256()
    total = 0
    with opener.open(request, timeout=120) as response, destination.open("xb") as output:
        while True:
            chunk = response.read(64 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if total > MAX_DOWNLOAD_BYTES:
                raise ValueError("upstream artifact exceeds managed-capability safety limit")
            digest.update(chunk)
            output.write(chunk)
    if total <= 0:
        raise ValueError("upstream artifact is empty")
    return {"url": url, "size": total, "sha256": digest.hexdigest()}


def _write_zip(content_root: Path, output_zip: Path) -> None:
    if output_zip.exists():
        raise FileExistsError(output_zip)
    files: list[Path] = []
    for root, directories, names in os.walk(content_root, followlinks=False):
        root_path = Path(root)
        for directory in directories:
            path = root_path / directory
            if path.is_symlink():
                raise ValueError(f"candidate content contains symlink directory: {path}")
        for name in names:
            path = root_path / name
            if path.is_symlink() or not path.is_file():
                raise ValueError(f"candidate content contains unsafe entry: {path}")
            files.append(path)
    if not files:
        raise ValueError("candidate content is empty")
    files.sort(key=lambda item: item.relative_to(content_root).as_posix().encode())
    with zipfile.ZipFile(output_zip, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for source in files:
            relative = source.relative_to(content_root).as_posix()
            info = zipfile.ZipInfo.from_file(source, arcname=relative)
            info.compress_type = zipfile.ZIP_DEFLATED
            # Normalize away special file bits. Runtime adapters set executable permissions after extraction.
            info.external_attr = (0o100600 << 16)
            with source.open("rb") as input_file, archive.open(info, "w") as output_file:
                shutil.copyfileobj(input_file, output_file, 64 * 1024)


def _extract_unique_zip_basename(archive_path: Path, basename: str, destination: Path) -> None:
    matches: list[zipfile.ZipInfo] = []
    with zipfile.ZipFile(archive_path, "r") as archive:
        for item in archive.infolist():
            path = _safe_relative(item.filename.rstrip("/")) if item.filename.rstrip("/") else None
            mode = (item.external_attr >> 16) & 0xFFFF
            file_type = stat.S_IFMT(mode)
            if file_type not in {0, stat.S_IFREG, stat.S_IFDIR}:
                raise ValueError(f"upstream ZIP contains link or special entry: {item.filename}")
            if not item.is_dir() and path and path.name == basename:
                matches.append(item)
        if len(matches) != 1:
            raise ValueError(f"expected exactly one {basename!r} in upstream ZIP, found {len(matches)}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        with archive.open(matches[0], "r") as source, destination.open("xb") as output:
            shutil.copyfileobj(source, output, 64 * 1024)


def _extract_unique_tar_suffix(archive_path: Path, suffix: PurePosixPath, destination: Path) -> None:
    matches: list[tarfile.TarInfo] = []
    with tarfile.open(archive_path, "r:gz") as archive:
        for item in archive:
            path = _safe_relative(item.name.rstrip("/")) if item.name.rstrip("/") else None
            # We extract exactly one regular Node executable, not the surrounding runtime tree.
            # Official Node archives contain npm/npx symlinks; unselected entries therefore do
            # not need to be extractable, but a link/special entry can never satisfy the target.
            if not item.isfile():
                continue
            if path and len(path.parts) >= len(suffix.parts) and path.parts[-len(suffix.parts):] == suffix.parts:
                matches.append(item)
        if len(matches) != 1:
            raise ValueError(f"expected exactly one *{suffix.as_posix()} in upstream tar, found {len(matches)}")
        source = archive.extractfile(matches[0])
        if source is None:
            raise ValueError("could not read selected upstream tar entry")
        destination.parent.mkdir(parents=True, exist_ok=True)
        with source, destination.open("xb") as output:
            shutil.copyfileobj(source, output, 64 * 1024)


def _extract_npm_package(archive_path: Path, destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=False)
    total = 0
    count = 0
    with tarfile.open(archive_path, "r:gz") as archive:
        for item in archive:
            path = _safe_relative(item.name.rstrip("/")) if item.name.rstrip("/") else None
            if path is None:
                continue
            if item.issym() or item.islnk() or not (item.isfile() or item.isdir()):
                raise ValueError(f"npm tarball contains link or special entry: {item.name}")
            count += 1
            if count > 50_000:
                raise ValueError("npm tarball contains too many entries")
            target = destination.joinpath(*path.parts)
            if item.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            total += item.size
            if total > 512 * 1024 * 1024:
                raise ValueError("npm package expands beyond managed-capability safety limit")
            source = archive.extractfile(item)
            if source is None:
                raise ValueError(f"could not read npm tar entry: {item.name}")
            target.parent.mkdir(parents=True, exist_ok=True)
            with source, target.open("xb") as output:
                shutil.copyfileobj(source, output, 64 * 1024)


def _remove_npm_symlink_bin(package_root: Path) -> None:
    bin_dir = package_root / "node_modules" / ".bin"
    if bin_dir.exists() or bin_dir.is_symlink():
        if bin_dir.is_symlink() or bin_dir.is_file():
            bin_dir.unlink()
        else:
            shutil.rmtree(bin_dir)


def producer_node_supported(raw: str) -> bool:
    value = raw.strip().removeprefix("v")
    try:
        major_text, minor_text, patch_text = value.split(".", 2)
        major, minor, patch = int(major_text), int(minor_text), int(patch_text)
    except (ValueError, AttributeError):
        return False
    if patch < 0:
        return False
    return (major == 22 and minor >= 19) or (major == 24 and minor >= 11) or major >= 26


def _tool(name: str) -> str:
    resolved = shutil.which(name)
    if not resolved:
        raise RuntimeError(f"required producer tool is unavailable: {name}")
    return resolved


def ensure_rea_producer_runtime() -> tuple[str, str]:
    node_tool = _tool("node")
    npm_tool = _tool("npm")
    node = subprocess.run([node_tool, "--version"], check=True, capture_output=True, text=True).stdout.strip()
    if not producer_node_supported(node):
        raise RuntimeError(
            f"REA candidate production requires Node ^22.19.0, ^24.11.0, or >=26.0.0; found {node!r}"
        )
    npm = subprocess.run([npm_tool, "--version"], check=True, capture_output=True, text=True).stdout.strip()
    if not npm or not npm.split(".", 1)[0].isdigit():
        raise RuntimeError(f"REA candidate production requires a usable npm CLI; found {npm!r}")
    return node_tool, npm_tool


def build_ast_grep(version: str, os_name: str, arch: str, work: Path) -> tuple[Path, dict[str, object]]:
    asset = AST_GREP_ASSETS.get((os_name, arch))
    if not asset:
        raise ValueError(f"unsupported ast-grep platform: {os_name}/{arch}")
    upstream = work / "upstream.zip"
    provenance = download(f"https://github.com/ast-grep/ast-grep/releases/download/{version}/{asset}", upstream)
    content = work / "content"
    content.mkdir()
    binary_name = "ast-grep.exe" if os_name == "windows" else "ast-grep"
    binary = content / binary_name
    _extract_unique_zip_basename(upstream, binary_name, binary)
    if os_name != "windows":
        binary.chmod(0o700)
    return content, provenance


def build_node(version: str, os_name: str, arch: str, work: Path) -> tuple[Path, dict[str, object]]:
    mapping = NODE_ASSETS.get((os_name, arch))
    if not mapping:
        raise ValueError(f"unsupported managed Node platform: {os_name}/{arch}")
    asset_template, entrypoint = mapping
    asset = asset_template.format(version=version)
    url = f"https://nodejs.org/dist/v{version}/{asset}"
    upstream = work / ("node.exe" if os_name == "windows" else "node.tar.gz")
    provenance = download(url, upstream)
    content = work / "content"
    content.mkdir()
    destination = content.joinpath(*PurePosixPath(entrypoint).parts)
    if os_name == "windows":
        destination.write_bytes(upstream.read_bytes())
    else:
        _extract_unique_tar_suffix(upstream, PurePosixPath("bin/node"), destination)
        destination.chmod(0o700)
    return content, provenance


def build_rea(version: str, work: Path) -> tuple[Path, dict[str, object]]:
    _node_tool, npm_tool = ensure_rea_producer_runtime()
    upstream = work / "rea.tgz"
    provenance = download(
        f"https://registry.npmjs.org/rea-agents/-/rea-agents-{version}.tgz",
        upstream,
    )
    content = work / "content"
    _extract_npm_package(upstream, content)
    package_root = content / "package"
    manifest_path = package_root / "package.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("name") != "rea-agents" or manifest.get("version") != version:
        raise ValueError("REA npm package identity does not match requested version")
    subprocess.run(
        [
            npm_tool,
            "install",
            "--omit=dev",
            "--package-lock=false",
            "--audit=false",
            "--fund=false",
            "--ignore-scripts",
        ],
        cwd=package_root,
        check=True,
        env={**os.environ, "npm_config_update_notifier": "false"},
    )
    _remove_npm_symlink_bin(package_root)
    return content, provenance


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--capability", choices=["ast-grep", "rea", "rea-node"], required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--os", choices=["linux", "windows", "macos"], required=True)
    parser.add_argument("--arch", choices=["x86_64", "aarch64"], required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    asset_name = f"{args.capability}-{args.version}-{args.os}-{args.arch}.zip"
    artifact = args.output_dir / asset_name
    content_destination = args.output_dir / "content"
    provenance_path = args.output_dir / "upstream.json"
    if artifact.exists() or content_destination.exists() or provenance_path.exists():
        raise FileExistsError("candidate output directory already contains managed-capability outputs")

    with tempfile.TemporaryDirectory(prefix="repotunnel-managed-candidate-") as temp:
        work = Path(temp)
        if args.capability == "ast-grep":
            content, provenance = build_ast_grep(args.version, args.os, args.arch, work)
        elif args.capability == "rea-node":
            content, provenance = build_node(args.version, args.os, args.arch, work)
        else:
            content, provenance = build_rea(args.version, work)

        _write_zip(content, artifact)
        shutil.copytree(content, content_destination, symlinks=False)
        provenance_path.write_text(
            json.dumps(
                {
                    "schema": 1,
                    "capability": args.capability,
                    "version": args.version,
                    "os": args.os,
                    "arch": args.arch,
                    "upstream": provenance,
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )

    print(artifact)


if __name__ == "__main__":
    main()
