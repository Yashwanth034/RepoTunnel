# Build Verification Report

Website checkpoint: **1.2.0**
Updated: 2026-10-09 (Asia/Kolkata)

## Scope

This checkpoint preserves the website's expanded documentation, official platform links and six real guide screenshots, alongside the reviewed navigation, copy controls and layout corrections. The website contains 40 guides, 16 product pages and 9 solution pages. The main RepoTunnel application remains read-only.

Website versions in `WEBSITE_VERSION` are independent of desktop release versions. The earlier 1.1.0 checkpoint remains in Git history.

## Fresh checkpoint verification

The following command completed with exit code 0 in managed process `process-1a11d4c4b5b-30`:

```bash
npm run check
./node_modules/.bin/astro check
node scripts/verify_theme.mjs
SITE_PREVIEW_ORIGIN=http://127.0.0.1:3010 node scripts/smoke_live.mjs
```

- Source verification passed across 24 Astro source files, including content integrity, guide links and screenshot hashes/dimensions.
- The production build generated 77 HTML pages; 76 normal pages are indexed by the search inventory and Pagefind.
- Static validation passed for internal routes, anchors, page metadata and release link configuration.
- Astro reported 0 errors, 0 warnings and 0 hints across 32 checked files.
- Theme startup passed all five storage scenarios.
- All 76 normal routes passed the live HTTP smoke test.

## Recorded browser verification

The preceding guide/layout verification, completed before this metadata-only checkpoint update, reported:

- 77 pages checked at 320, 375, 640, 768, 1024, 1440 and 1920 pixels.
- 566 browser checks and 105 checked code blocks, with no failures.
- 42 guide page variants and 48 image loads across light/dark themes, with no failures.

Those browser checks were not rerun for this checkpoint's version/README changes. Screenshot provenance is recorded in `docs/platform-screenshots.json`; guide coverage and external destination results are under `docs/`.

## Checkpoint boundaries

No public deployment or Git push is part of this checkpoint. Third-party sign-in, download transfer and authenticated provider setup are not proved by the static build. Use the recorded link report and official instructions for those external services.

The pre-existing untracked `package-lock.json` remains untouched and excluded under RepoTunnel's protected-file policy. Generated dependencies and build output remain ignored.
