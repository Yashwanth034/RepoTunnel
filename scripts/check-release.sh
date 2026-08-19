#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

need node
need npm
need rustc
need cargo
need git

if ! command -v bwrap >/dev/null 2>&1; then
  echo "Warning: bubblewrap (bwrap) is missing; RepoTunnel command verification will be disabled at runtime." >&2
fi

if ! command -v tunnel-client >/dev/null 2>&1 && \
   [[ ! -x "$HOME/.local/bin/tunnel-client" ]] && \
   [[ ! -x /usr/local/bin/tunnel-client ]] && \
   [[ ! -x /usr/bin/tunnel-client ]]; then
  echo "Note: OpenAI tunnel-client is missing; only the optional Secure Tunnel path cannot be live-tested. The recommended managed public MCP path is embedded in RepoTunnel." >&2
fi

echo "Node:  $(node --version)"
echo "npm:   $(npm --version)"
echo "Rust:  $(rustc --version)"
echo "Cargo: $(cargo --version)"
echo "Git:   $(git --version)"

if [[ ! -d node_modules ]]; then
  echo "node_modules is missing. Run: npm install" >&2
  exit 1
fi

echo "Running frontend type/build checks..."
npm run build

echo "Running Rust tests..."
cargo test --manifest-path src-tauri/Cargo.toml

echo "Release checks passed."
