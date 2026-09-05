# RepoTunnel Release and Auto-Update Guide

RepoTunnel publishes desktop releases through GitHub Releases. Linux produces Debian (`.deb`), RPM (`.rpm`) and AppImage packages, Windows produces NSIS and MSI installers, and macOS produces DMG packages plus signed updater application archives.

The in-app updater is deliberately separate from RepoTunnel's MCP, Direct HTTPS, OAuth, AI Workspace and project data. Installing a newer application package must not replace the RepoTunnel application-data directory.

## Versioning

Use semantic patch releases for small fixes:

- `0.3.0 -> 0.3.1` for bug fixes and small safe improvements.
- `0.3.x -> 0.4.0` for a meaningful feature release.
- Reserve `1.0.0` for the stable product milestone.

Keep the version identical in `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock` and `src-tauri/tauri.conf.json`. The release workflow rejects mismatches.

## Validate before release

Run:

```bash
./scripts/check-release.sh
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
npm run check
npm run test:frontend
npm run build
cargo test --manifest-path src-tauri/Cargo.toml --lib
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

Also run the applicable live checks from `docs/acceptance.md`. Never publish a release only because compilation succeeded.

## Update signing key

RepoTunnel's updater uses Tauri's mandatory minisign verification. The public key is compiled into `src-tauri/tauri.conf.json`; the private key must never be committed.

The GitHub Actions repository secret required by the release workflow is:

```text
TAURI_SIGNING_PRIVATE_KEY
```

RepoTunnel's updater private key is password protected, so also configure:

```text
TAURI_SIGNING_PRIVATE_KEY_PASSWORD
```

Both secrets are required by the release workflow; it fails before building release packages when either is missing.

The release workflow must fail if the signing key is missing. Never fall back to an unsigned updater artifact.

Treat the updater private key as a long-term release identity. Losing it prevents existing installations from trusting future updates. If key rotation is ever required, ship a trusted transition release before retiring the old key.

## GitHub release pipeline

`.github/workflows/platform-build.yml` performs the release pipeline:

1. Run frontend, Rust, formatting, Clippy, audit and test gates.
2. Verify the Git tag matches the application version.
3. Build platform installers with `src-tauri/tauri.release.conf.json`, which enables updater artifacts only for release builds so ordinary local packaging does not require the signing key.
4. Collect the normal installers plus their `.sig` files and macOS `.app.tar.gz` updater archives.
5. Extract the matching `CHANGELOG.md` section as release notes.
6. Generate `latest.json` with `scripts/generate-updater-manifest.mjs`.
7. Generate `RepoTunnel-SHA256SUMS.txt` for downloadable-file integrity checks.
8. Publish all files in one GitHub Release.

The installed application checks only the official endpoint:

```text
https://github.com/Yashwanth034/RepoTunnel/releases/latest/download/latest.json
```

The updater verifies the artifact signature with the public key embedded in RepoTunnel before installation.

## Platform coverage

Auto Update is one cross-platform RepoTunnel feature, not a Linux-only implementation. The release pipeline builds and verifies the updater on native GitHub-hosted operating systems:

- Linux x64: Debian, RPM and AppImage, each with updater signatures.
- Windows x64: NSIS and MSI, each with updater signatures, built and tested on a Windows runner.
- macOS Apple Silicon: DMG plus signed `.app.tar.gz` updater archive on an arm64 macOS runner.
- macOS Intel: DMG plus signed `.app.tar.gz` updater archive on an Intel macOS runner.

Linux development can validate the shared updater logic and release metadata, but Windows installer execution and macOS bundle/update behavior must be validated on their native runners before a release is called production-ready. The final patch-release acceptance test should exercise **Check for updates -> Update & Restart -> persisted-state health check** on each supported operating-system family.

### macOS install safety gate

Signed update discovery and macOS release artifacts are enabled, but in-app installation is intentionally blocked on macOS while Tauri updater issue #3505 remains unresolved: the current updater can lose the installed `.app` if replacement fails after moving the old app into a temporary backup. RepoTunnel must not expose **Update & Restart** on macOS until either an upstream release fixes restore-on-failure or RepoTunnel has an independently verified recovery implementation. After that change, run a native failure-injection test as well as the normal patch-update acceptance test before removing the gate.

## Auto-update behavior

RepoTunnel checks for updates on startup and at a bounded interval while the desktop app is running. Settings also provides a manual **Check for updates** action.

When a newer version is available the user can:

- review the release notes;
- choose **Update & Restart**;
- choose **Later**, which defers automatic reminders for 24 hours;
- disable or re-enable automatic checks.

RepoTunnel refuses to begin installation while work that would be interrupted is active, including Home generation, Model Trial, managed processes, running terminal commands, active Team Mode tasks, Browser Automation or AI Workspace sessions.

Before installation RepoTunnel records the intended version transition. After restart it confirms that the expected version started and that core persisted state is still readable. A failed download, signature verification or install leaves the current application installed and records the failure instead of pretending the update succeeded.

## Data-preservation contract

Application updates do not replace or reset RepoTunnel's application-data directory. This preserves, subject to explicit future schema migrations:

- approved projects/workspaces and permissions;
- Project Memory / continuity state;
- MCP and OAuth state;
- Direct HTTPS and public-tunnel configuration;
- AI Workspace and desktop integration settings;
- History, checkpoints and operational records;
- Model Hub and local-model configuration;
- user preferences.

Any future data-schema migration must be backward-aware, tested independently, and included in the post-update health checks. Never combine an irreversible data migration with an updater change without a recovery plan.

## Release checklist

1. Confirm the repository contains no credentials, updater private keys, tunnel credentials, updater private keys, or other secrets; release-only secrets must come from the CI secret store.
2. Confirm the updater signing secret exists in GitHub Actions.
3. Update all version files together and add release notes to `CHANGELOG.md`.
4. Run the complete validation gate.
5. Tag the exact verified commit with `v<version>`.
6. Let GitHub Actions build and sign every supported platform artifact.
7. Confirm `latest.json` contains every required platform target and every referenced asset has a matching signature.
8. Install at least one clean package for each platform that is available for testing.
9. Verify the in-app **Check for updates -> Update & Restart** flow with a real patch release before calling Auto Update production-ready.
10. Verify RepoTunnel reopens with existing project, security, Direct HTTPS and continuity data intact.
