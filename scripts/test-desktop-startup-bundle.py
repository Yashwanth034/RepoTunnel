#!/usr/bin/env python3
"""Check that rarely used desktop screens stay outside the initial Vite graph.

Run after `npm run build`. This is a deterministic output-boundary test, not a
claim about production frame times or installed desktop startup metrics.
"""
import gzip
from html.parser import HTMLParser
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DIST = ROOT / "dist"
ASSETS = DIST / "assets"


class IndexAssets(HTMLParser):
    def __init__(self):
        super().__init__()
        self.initial_scripts = []
        self.preloads = []

    def handle_starttag(self, tag, attrs):
        values = dict(attrs)
        if tag == "script" and values.get("type") == "module":
            self.initial_scripts.append(values.get("src", ""))
        elif tag == "link" and values.get("rel") == "modulepreload":
            self.preloads.append(values.get("href", ""))


def main():
    html = (DIST / "index.html").read_text(encoding="utf-8")
    parser = IndexAssets()
    parser.feed(html)
    assert len(parser.initial_scripts) == 1, parser.initial_scripts
    initial = parser.initial_scripts + parser.preloads
    assert initial, "No initial module graph"
    assert all(path.startswith("/assets/") for path in initial), initial
    assert not any("editor-core" in path for path in initial), (
        "The CodeMirror core regressed into the Home preload graph", initial
    )
    deferred_views = (
        "WorkspaceEditor", "VideoPanel", "ProductionPanel",
        "AgentsPanel", "TeamPanel", "RemoteMachinesPanel", "PhonePanel",
        "ExecutionPanel", "GitPanel", "WorkflowPanel", "ModelHub",
        "ChangeHistoryPanel", "CheckpointManager",
    )
    for view in deferred_views:
        lazy_chunks = list(ASSETS.glob(view + "-*.js"))
        assert len(lazy_chunks) == 1, (view, lazy_chunks)
        assert not any(view in path for path in initial), (
            f"{view} was eagerly preloaded", initial
        )
    initial_files = [DIST / path.lstrip("/") for path in initial]
    assert all(path.is_file() for path in initial_files), initial_files
    raw_size = sum(path.stat().st_size for path in initial_files)
    gzip_size = sum(len(gzip.compress(path.read_bytes(), compresslevel=9)) for path in initial_files)
    # Catch accidental eager loading, not small normal bundle size fluctuations.
    assert gzip_size < 145_000, f"Unexpectedly large Home module graph: {gzip_size} gzip bytes"
    print(f"PASS: {len(initial_files)} initially loaded JavaScript assets, "
          f"{raw_size:,} raw / {gzip_size:,} gzip bytes")
    print(f"PASS: CodeMirror and all {len(deferred_views)} non-Home views are deferred until used")


if __name__ == "__main__":
    main()
