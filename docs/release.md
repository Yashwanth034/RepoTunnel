# Linux Release Guide

RepoTunnel's first release target is Linux. The Tauri configuration produces Debian (`.deb`), RPM (`.rpm`), and AppImage bundles.

## Release status

The source tree is prepared as **v0.1.0 release-candidate quality**. Do not call it a stable 1.0 release until the compiled application has passed the live acceptance test in `docs/acceptance.md` on a real Linux desktop with ChatGPT, `tunnel-client`, Bubblewrap, Git, Rust, and the Tauri runtime dependencies installed.

## Build host

Use an older supported Linux baseline when producing broadly distributed binaries. Tauri's Linux documentation recommends Ubuntu 22.04 or Debian 12 as suitable baseline examples because building on newer glibc can make the resulting binary incompatible with older distributions.

For Debian/Ubuntu development, install Tauri's current Linux prerequisites:

```bash
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev \
  build-essential \
  curl \
  wget \
  file \
  libxdo-dev \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev
```

Also install:

- Node.js LTS and npm
- Rust stable (RepoTunnel's current MCP dependency requires Rust 1.88 or newer)
- Git
- Bubblewrap (`bwrap`) for sandboxed verification commands
- OpenAI `tunnel-client` for the ChatGPT connectivity acceptance test

## Install dependencies

```bash
npm install
```

Do not commit `node_modules`.

## Validate

```bash
./scripts/check-release.sh
```

This runs the frontend production build and Rust test suite. It also reports missing optional runtime dependencies used by RepoTunnel features.

## Build Linux bundles

```bash
./scripts/build-linux.sh
```

Expected artifacts are created under:

```text
src-tauri/target/release/bundle/deb/
src-tauri/target/release/bundle/rpm/
src-tauri/target/release/bundle/appimage/
```

## Signing

Linux package signing is not required by Tauri to distribute an application, but signed artifacts improve provenance and user trust. Add signing only after choosing the project's long-term release identity and key-management process. Never place signing private keys in this repository.

## Release checklist

1. Run the full acceptance test in `docs/acceptance.md`.
2. Confirm the repository contains no secrets or Runtime API keys.
3. Confirm `git status` is clean before producing release artifacts.
4. Run `./scripts/check-release.sh`.
5. Build all three Linux bundles.
6. Install and run at least the Debian package and AppImage on clean test machines/VMs.
7. Verify diagnostics, workspace permissions, edit review, command sandboxing, Git approval, graceful shutdown, and ChatGPT tunnel connectivity.
8. Record known limitations in `CHANGELOG.md` before publishing.
