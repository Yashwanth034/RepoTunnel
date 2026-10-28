#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import urllib.request

UA = {"User-Agent": "RepoTunnel managed capability discovery"}
AST_RELEASE = "https://api.github.com/repos/ast-grep/ast-grep/releases/latest"
REA_LATEST = "https://registry.npmjs.org/rea-agents/latest"
NODE_INDEX = "https://nodejs.org/dist/index.json"


def fetch_json(url: str):
    request = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(request, timeout=45) as response:
        return json.load(response)


def parse_version(raw: str) -> tuple[int, int, int]:
    value = raw.strip().removeprefix("v")
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", value)
    if not match:
        raise ValueError(f"unsupported semantic version: {raw!r}")
    return tuple(int(part) for part in match.groups())


def satisfies_simple_node_range(version: str, expression: str) -> bool:
    current = parse_version(version)
    clauses = [clause.strip() for clause in expression.split("||") if clause.strip()]
    if not clauses:
        return False
    for clause in clauses:
        if clause.startswith("^"):
            floor = parse_version(clause[1:].strip())
            major = floor[0]
            if major == 0:
                continue
            if current[0] == major and current >= floor:
                return True
        elif clause.startswith(">="):
            floor = parse_version(clause[2:].strip())
            if current >= floor:
                return True
        elif re.fullmatch(r"v?\d+\.\d+\.\d+", clause):
            if current == parse_version(clause):
                return True
        else:
            raise ValueError(f"unsupported Node engine clause: {clause!r}")
    return False


def discover() -> dict[str, str]:
    ast = fetch_json(AST_RELEASE)
    ast_version = str(ast.get("tag_name") or "").removeprefix("v")
    parse_version(ast_version)

    rea = fetch_json(REA_LATEST)
    rea_version = str(rea.get("version") or "")
    parse_version(rea_version)
    engines = rea.get("engines") or {}
    node_range = engines.get("node")
    if not isinstance(node_range, str) or not node_range.strip():
        raise ValueError("latest rea-agents package does not declare a Node engine range")

    node_versions = []
    for item in fetch_json(NODE_INDEX):
        raw = item.get("version")
        if not isinstance(raw, str):
            continue
        try:
            parsed = parse_version(raw)
        except ValueError:
            continue
        # Odd Node majors are short-lived development lines: managed REA uses stable even releases.
        if parsed[0] % 2 != 0:
            continue
        if satisfies_simple_node_range(raw, node_range):
            node_versions.append((parsed, raw.removeprefix("v")))
    if not node_versions:
        raise ValueError(f"no stable Node release satisfies REA engine range {node_range!r}")
    node_versions.sort(reverse=True)

    return {
        "ast_grep_version": ast_version,
        "rea_version": rea_version,
        "rea_node_range": node_range,
        "rea_node_version": node_versions[0][1],
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output")
    parser.add_argument("--github-output")
    args = parser.parse_args()
    result = discover()
    text = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(text, encoding="utf-8")
    if args.github_output:
        with Path(args.github_output).open("a", encoding="utf-8") as handle:
            for key, value in result.items():
                handle.write(f"{key}={value}\n")
    print(text, end="")


if __name__ == "__main__":
    main()
