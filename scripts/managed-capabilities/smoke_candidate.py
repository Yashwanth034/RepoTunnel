#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def entrypoint(capability: str, os_name: str) -> str:
    if capability == "ast-grep":
        return "ast-grep.exe" if os_name == "windows" else "ast-grep"
    if capability == "rea-node":
        return "node.exe" if os_name == "windows" else "bin/node"
    raise ValueError(f"unsupported smoke capability: {capability}")


def smoke_node(root: Path, os_name: str, expected_version: str) -> None:
    binary = root.joinpath(*entrypoint("rea-node", os_name).split("/"))
    result = subprocess.run([str(binary), "--version"], check=True, capture_output=True, text=True, timeout=20)
    actual = result.stdout.strip().removeprefix("v")
    if actual != expected_version:
        raise RuntimeError(f"managed Node reported {actual!r}, expected {expected_version!r}")


def smoke_ast_grep(root: Path, os_name: str, expected_version: str) -> None:
    binary = root.joinpath(*entrypoint("ast-grep", os_name).split("/"))
    version = subprocess.run([str(binary), "--version"], check=True, capture_output=True, text=True, timeout=20)
    if expected_version not in (version.stdout + version.stderr):
        raise RuntimeError("ast-grep candidate did not report the requested version")
    with tempfile.TemporaryDirectory(prefix="repotunnel-ast-smoke-") as temp:
        fixture = Path(temp) / "fixture.ts"
        fixture.write_text("foo(value);\n", encoding="utf-8")
        result = subprocess.run(
            [str(binary), "run", "--pattern", "foo($A)", "--lang", "ts", "--json=stream", "--", str(fixture)],
            check=True,
            capture_output=True,
            text=True,
            timeout=30,
        )
        lines = [line for line in result.stdout.splitlines() if line.strip()]
        if len(lines) != 1:
            raise RuntimeError(f"ast-grep structural smoke returned {len(lines)} matches instead of one")
        match = json.loads(lines[0])
        if match.get("text") != "foo(value)":
            raise RuntimeError("ast-grep structural smoke returned the wrong match")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--capability", choices=["ast-grep", "rea-node"], required=True)
    parser.add_argument("--os", choices=["linux", "windows", "macos"], required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--content-root", required=True)
    args = parser.parse_args()
    root = Path(args.content_root).resolve()
    if args.capability == "rea-node":
        smoke_node(root, args.os, args.version)
    else:
        smoke_ast_grep(root, args.os, args.version)
    print(f"{args.capability} {args.version} smoke passed on {args.os}")


if __name__ == "__main__":
    main()
