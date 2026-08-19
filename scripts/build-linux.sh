#!/usr/bin/env bash
set -euo pipefail

# Prevent local build-machine paths/usernames from being embedded in release binaries.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${HOME}=/usr/src/repotunnel-build"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

"$ROOT/scripts/check-release.sh"

echo "Building Linux Debian, RPM, and AppImage bundles..."
npm run tauri -- build --bundles deb,rpm,appimage

echo "Bundles are under src-tauri/target/release/bundle/."
