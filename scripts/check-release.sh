#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

for command in node npm cargo rustc; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "Missing required command: $command" >&2
    exit 1
  }
done

[[ -d node_modules ]] || {
  echo "node_modules is missing. Run npm ci first." >&2
  exit 1
}

node <<'NODE'
const fs = require('fs');

const packageVersion = require('./package.json').version;
const packageLockVersion = require('./package-lock.json').version;
const tauriVersion = require('./src-tauri/tauri.conf.json').version;
const cargoToml = fs.readFileSync('./src-tauri/Cargo.toml', 'utf8');
const cargoLock = fs.readFileSync('./src-tauri/Cargo.lock', 'utf8');
const cargoVersion = cargoToml.match(/^version = "([^"]+)"/m)?.[1];
const cargoLockVersion = cargoLock.match(/\[\[package\]\]\nname = "repotunnel"\nversion = "([^"]+)"/)?.[1];
const versions = { packageVersion, packageLockVersion, tauriVersion, cargoVersion, cargoLockVersion };

if (Object.values(versions).some((value) => !value) || new Set(Object.values(versions)).size !== 1) {
  console.error('Release version mismatch:', versions);
  process.exit(1);
}

console.log(`RepoTunnel v${packageVersion}`);
NODE

if find src-tauri/src -type f -name '*.before-*' -print -quit | grep -q .; then
  echo "Stale .before-* source files are present." >&2
  exit 1
fi

for path in .aiw-android-smoke ai-workspace-android-test; do
  [[ ! -e "$path" ]] || {
    echo "Local test artifact is present: $path" >&2
    exit 1
  }
done

npm run check
npm run test:frontend
npm run build
npm audit --audit-level=moderate

cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo check --locked --manifest-path src-tauri/Cargo.toml
cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path src-tauri/Cargo.toml --lib

if command -v cargo-audit >/dev/null 2>&1; then
  cargo audit --no-yanked --file src-tauri/Cargo.lock
fi

echo "Release checks passed."
