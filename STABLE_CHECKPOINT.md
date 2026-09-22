# RepoTunnel Stable Checkpoint

## Locked working baseline

- Branch: `fix-glib-alert`
- Stable commit: `8c571da45f5ef4e32e716ed227dddf8553a61599`
- Short SHA: `8c571da`
- Commit message: `feat: add video production workspace and desktop integrations`
- Remote at checkpoint: `origin/fix-glib-alert` points to the same commit.
- Date locked: 2026-09-22

This commit is the known-good recovery point before the next round of RepoTunnel feature development.

## What was verified at this checkpoint

- RepoTunnel desktop app launches and works.
- The new standalone Video Project workflow is present.
- Real video playback was manually tested in the isolated desktop and confirmed working.
- Video playback uses the localhost HTTP byte-range transport instead of relying on Tauri `asset://` for long-form media playback.
- Seeking, pause/resume, stop/restart, and long-video playback fixes are included.
- Browser-like whole-app zoom behavior was removed so RepoTunnel behaves like a native desktop application.
- Final release validation passed before packaging:
  - frontend tests: 17/17
  - Rust tests: 210 passed, 1 intentionally ignored
  - release checks: passed
  - npm audit: 0 vulnerabilities
- Final verified Debian package produced from this working state:
  - package: `RepoTunnel_0.3.1_amd64.deb`
  - DEB SHA-256: `ac3874edaa014641d9fd58e5ee2639a1325920b26f1187048dbb16c9c88faff9`
  - packaged binary SHA-256: `952014e87eb7a94a1794bb6a88a63cb818c0b12e48010ce6cd5738b0cb3c38d8`

## Recovery rule

If later development breaks RepoTunnel and the user asks to return to the last known-good version, use commit:

```
8c571da45f5ef4e32e716ed227dddf8553a61599
```

Do not reset, revert, checkout, force-push, or overwrite current work automatically. First show the user the current Git state and explain what would be preserved or lost. Only move back to this baseline when the user explicitly approves it.

## Temporary files

Files such as `.neural_tail.txt`, `.edge_*.txt`, preview screenshots, transfer screenshots, and files under temporary validation folders are test/debug artifacts unless the user explicitly says otherwise. Do not treat them as source code or stable project state.
