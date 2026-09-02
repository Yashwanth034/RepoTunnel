#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Prevent the actual local project path/user directory from being embedded in release binaries.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${ROOT}=/usr/src/repotunnel-build"

"$ROOT/scripts/check-release.sh"

echo "Building Linux Debian, RPM, and AppImage bundles..."
npm run tauri -- build --bundles deb,rpm,appimage

echo "Bundles are under src-tauri/target/release/bundle/."
