#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Prevent the actual local project path/user directory from being embedded in release binaries.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=${ROOT}=/usr/src/repotunnel-build"

# Keep Cargo downloads resilient on slow links and fetch the locked graph once up front.
# Subsequent check/clippy/test/build steps then reuse the same persistent Cargo cache.
export CARGO_HTTP_TIMEOUT="${CARGO_HTTP_TIMEOUT:-120}"
export CARGO_HTTP_LOW_SPEED_LIMIT="${CARGO_HTTP_LOW_SPEED_LIMIT:-1}"
export CARGO_NET_RETRY="${CARGO_NET_RETRY:-5}"
cargo fetch --locked --manifest-path src-tauri/Cargo.toml

"$ROOT/scripts/check-release.sh"

echo "Building Linux Debian, RPM, and AppImage bundles..."
npm run tauri -- build --bundles deb,rpm,appimage

echo "Bundles are under src-tauri/target/release/bundle/."
