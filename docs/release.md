# RepoTunnel Release and Auto-Update Guide

RepoTunnel publishes desktop releases through GitHub Releases. Linux produces Debian (`.deb`), RPM (`.rpm`) and AppImage packages, Windows produces NSIS and MSI installers, and macOS produces DMG packages plus signed updater application archives.

The in-app updater is deliberately separate from RepoTunnel's MCP, Direct HTTPS, OAuth, AI Workspace and project data. Installing a newer application package must not replace the RepoTunnel application-data directory.

## Versioning

Use semantic versioning for future releases:

- Use a patch version for bug fixes and small safe improvements.
- Use a minor version for meaningful feature releases.
- Reserve `1.0.0` for the stable product milestone.

The current public release is **v0.4.1**. Version changes require a separate, explicitly authorized release.

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

1. Prefer an authorized **`workflow_dispatch` on the default branch**. The workflow also supports tag-triggered runs; main-branch dispatch makes the stable Windows cache reusable.
2. Run quality/security checks and platform builds in parallel. **Windows validation** (Rust Clippy and tests) is a separate job from **Windows packaging**; the validation job avoids unnecessary Node setup.
3. Build Linux DEB/RPM/AppImage, Windows NSIS/MSI, and Apple Silicon/Intel macOS DMGs and signed updater artifacts.
4. Wait for quality, Linux, Windows validation, Windows packaging, and both macOS builds to pass before the final release job can proceed.
5. Collect the verified artifacts, derive release notes from the matching `CHANGELOG.md` section, generate `latest.json`, and generate `RepoTunnel-SHA256SUMS.txt`.
6. The final job establishes the canonical `v<version>` tag for the verified commit and publishes the release.

**Protected tags:** Publication and any protected-tag changes need explicit repository-owner authorization. Do not alter the published v0.4.1 tag.

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

Linux development can validate the shared updater logic and release metadata, but installation/runtime behavior must be checked on the applicable native OS. CI packaging and tests do not replace installed-app acceptance checks. Exercise **Check for updates -> Update & Restart -> persisted-state health check** only on operating systems where RepoTunnel permits in-app installation. **macOS currently supports update discovery but blocks in-app installation** pending verified recovery; test DMG installation separately.

### macOS install safety gate

Signed update discovery and macOS release artifacts are enabled, but the current RepoTunnel safety policy intentionally blocks in-app installation on macOS because a failed application replacement must not be allowed to remove the working installed `.app`.

Do not expose **Update & Restart** on macOS until RepoTunnel's current updater stack has a verified restore-on-failure path (either through an upstream fix or an independently validated RepoTunnel recovery implementation). Before removing the gate, run native macOS failure-injection testing as well as the normal patch-update acceptance test.

## Auto-update behavior

RepoTunnel checks for updates on startup and at a bounded interval while the desktop app is running. Settings also provides a manual **Check for updates** action.

When a newer version is available the user can review release notes, select **Later** (deferring reminders for 24 hours), or change automatic-check preferences. **Update & Restart** is offered only on platforms where RepoTunnel permits in-app installation; it is intentionally not available on macOS.

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

1. Confirm no credentials, signing keys, or other secrets are present in public source or release assets; release-only credentials remain in the CI secret store.
2. Verify the updater signing configuration is available to the release workflow.
3. For a separately **approved future release**, synchronize version files and update `CHANGELOG.md`.
4. Run validation and the applicable live acceptance checks. Confirm the intended Git publication and tag-protection operations have been authorized.
5. Prefer `workflow_dispatch` from the default branch; keep quality, Linux, Windows validation, Windows packaging, and macOS builds parallel.
6. Confirm all required checks pass before the final job establishes the canonical tag and publishes assets.
7. Verify updater manifests, signatures, checksums, and asset lists. Exclude package-inspection leftovers.
8. Test clean packages on each native platform available for acceptance.
9. Exercise in-app installation only where enabled; preserve the current macOS install safety gate.
10. Confirm RepoTunnel restarts with its projects, security settings, Direct HTTPS, and continuity data intact.
