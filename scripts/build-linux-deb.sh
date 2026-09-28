#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Match the full Linux release path while producing only the installable Debian package.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${ROOT}=/usr/src/repotunnel-build"
export CARGO_HTTP_TIMEOUT="${CARGO_HTTP_TIMEOUT:-120}"
export CARGO_HTTP_LOW_SPEED_LIMIT="${CARGO_HTTP_LOW_SPEED_LIMIT:-1}"
export CARGO_NET_RETRY="${CARGO_NET_RETRY:-5}"

echo "Preparing locked Cargo dependencies..."
cargo fetch --locked --manifest-path src-tauri/Cargo.toml

"$ROOT/scripts/check-release.sh"

echo "Building Linux Debian package only..."
npm run tauri -- build --bundles deb

echo "Debian package is under src-tauri/target/release/bundle/deb/."
