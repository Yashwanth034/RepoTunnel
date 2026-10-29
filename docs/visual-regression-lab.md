# Visual Regression Lab (managed-browser screenshots)

## Purpose

Compare two captures from RepoTunnel's existing managed browser **without a new browser runtime, Playwright installation, screenshot uploads, or host filesystem access**. This feature helps AI detect unintended visual changes before reporting a UI task complete.

## Workflow

1. Use RepoTunnel's existing browser automation to open and stabilize a tab.
2. Call `browser_take_screenshot({workspace_id,tab_id,full_page:false})` to capture the baseline. Save its returned `id`.
3. Implement the UI change using the approved normal workspace tools. Do **not** treat screenshots as permission to modify unrelated files.
4. Capture the same tab again using `browser_take_screenshot`, with the same `full_page` mode.
5. Call `compare_browser_screenshots({workspace_id,tab_id,baseline_id,current_id,full_page:false,channel_tolerance:16,max_changed_percent:0.25})`.

The result includes a numerical `changedPercent`, changed-pixel count, dimensions, a tight union bounding rectangle around changed pixels, and a red-highlight diff PNG as MCP image content where the image budget permits. Unchanged pixels are dark grayscale to keep changes legible.

The comparison status is one of `unchanged`, `withinTolerance`, `regressionCandidate`, or `dimensionMismatch`. A dimension mismatch is **not** silently rescaled: verify viewport size, responsive breakpoints, browser zoom and page height. The tolerance is an exact per-channel 8-bit difference; it is not a semantic or perceptual similarity score. A low changed percentage **does not prove the UI is correct**. A high changed percentage can be intentional.

## Scope & privacy

- Screenshots are captured only by the existing browser tool, which applies project approval, per-AI tab ownership and Google/Gmail access restrictions.
- Each new screenshot receives private provenance metadata binding it to its approved workspace, actual browser runtime/AI owner, tab ID, and viewport/full-page capture mode. It additionally records a boolean (not the URL) for whether the capture was on a Google identity page; if Gmail access is subsequently revoked, comparison refuses that earlier capture even after the browser navigates elsewhere.
- Comparison refuses unknown screenshot IDs, screenshots without valid provenance (including screenshots from an older RepoTunnel build), mismatched browser scopes/tabs/modes, missing or expired images, linked PNG files, oversized files, malformed PNGs and excessively large dimensions.
- No arbitrary project path, host file path, public URL or unchecked image upload can be supplied to this tool.
- Output includes pixel metrics and an image highlight that naturally represents content the caller has already been authorized to view; no remote telemetry is sent. Screenshot PNGs and provenance are stored privately under RepoTunnel's existing AppData browser screenshot retention area.
- Inputs are bounded to 8 MiB compressed per PNG, maximum dimension 8192, and 6 million pixels. If the output diff image exceeds the MCP image budget, only the numeric report is returned.

## Determinism and QA

For useful comparisons, hold constant: window/viewport size, device pixel ratio and zoom, browser scale, fonts, animations, timestamps, locale, content, scrolling, and loading state. Where possible disable animation in the test fixture and wait for fonts/images to settle. For deliberate responsive changes, compare corresponding baseline/current captures at each viewport separately.

Check **both** numerical differences and the highlighted difference image, then inspect DOM/accessibility and actual functionality. This feature does not silently accept or reject a code change, alter Task Integrity, or substitute for Completion Proof acceptance criteria.

The image-processing unit tests cover identical images, exact changed bounds, tolerance, dimension mismatch, unsafe PNGs, cross-session provenance, symlink refusal and oversized dimensions. MCP registration and policy tests ensure a read-only tool. **Live-browser acceptance remains pending until the rebuilt RepoTunnel app is loaded:** capture and compare a real managed tab end-to-end and inspect the diff rendering.