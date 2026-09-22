# RepoTunnel AI Continuity Guide

Read this file and `STABLE_CHECKPOINT.md` before making substantial changes to RepoTunnel.

## Current development policy

RepoTunnel will continue receiving new features and improvements after the stable checkpoint at:

```
8c571da45f5ef4e32e716ed227dddf8553a61599
```

That commit must remain the known-good recovery baseline until the user explicitly locks a newer stable checkpoint.

## Rules for any AI working on this repository

1. Use the existing RepoTunnel project and branch. Do not recreate the project or start a duplicate workspace.
2. Preserve existing working functionality unless the user explicitly asks to replace it.
3. Do not silently reset, revert, or discard unrelated changes.
4. Before risky changes, inspect Git status and understand what is already modified.
5. Keep temporary test/debug artifacts out of commits.
6. Run relevant automated tests after implementation.
7. For desktop/UI/media work, verify the real behavior in the isolated AI Workspace when practical instead of relying only on unit tests.
8. Do not call a feature finished only because it compiles; verify the user-visible behavior.
9. Do not automatically install generated packages on the user's machine. Build and verify them first, then give the user a safe install command when requested.
10. When the user says a new version is fully working and wants it locked, create a new stable checkpoint by updating both this file and `STABLE_CHECKPOINT.md` with the new branch, full commit SHA, validation evidence, and recovery instructions.

## How to resume after a future problem

When the user says something like:

- "go back to the working one"
- "return to the last stable version"
- "this new feature broke RepoTunnel"
- "use the previous correct working version"

first read `STABLE_CHECKPOINT.md`, inspect current Git status/log, and identify the delta from the stable commit. Preserve any newer work unless the user explicitly authorizes discarding it.

## Important locked behavior at the current baseline

- Standalone Video Project workflow stays available.
- Video playback fix using private localhost HTTP byte-range serving stays intact.
- Video Project media helpers remain visible.
- Long-video seeking/playback must not be regressed.
- RepoTunnel desktop UI must not behave like a browser page with whole-app zoom.
- Existing Direct HTTPS, OAuth/DCR, project/workspace data, account state, MCP gateway security, and previously working project functionality must not be casually replaced or reset.

## Next phase

New feature development may continue from the current branch. Treat the stable commit above as the emergency recovery anchor until a newer stable checkpoint is deliberately created and validated.
