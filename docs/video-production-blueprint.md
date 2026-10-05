# RepoTunnel Video Production Blueprint v1

Status: LOCKED FOR IMPLEMENTATION
Date: 2026-09-21

## Goal

Extend the existing Video section from analysis/understanding into a minimal but complete AI-operated tutorial/video production workflow.

The AI may create a real software demonstration when a tutorial needs proof, generate explanatory animation/diagram scenes when the subject needs them, generate narration and subtitles, then assemble those assets into one finished video. Everything created for a production belongs to one Video Project stored inside the selected approved RepoTunnel workspace.

No webcam or human presenter is required.

## Locked product shape

The Video section has two capabilities:

1. Understand
   - Existing Video Intelligence stays intact.
   - URL/local media analysis, captions, frames and audio fallback remain reusable.

2. Produce
   - Video Projects are listed similarly to normal RepoTunnel Projects.
   - Each Video Project owns its script, storyboard, recordings, animations, narration, subtitles, timeline, thumbnail, renders and QA artifacts.
   - Selecting a Video Project shows its current/final preview and production state.

The base production path must not depend on DaVinci, Canva, Blender, OBS, Kdenlive, Manim, or any paid service. Optional external applications may be used later only when they materially improve a scene and are safely available.

## Minimal complete workflow

User request
-> create/select Video Project
-> research/plan
-> script
-> scene/storyboard plan
-> choose visual method per scene
   - real screen/app/browser recording when actual software behavior must be demonstrated
   - generated 2D diagram/motion scene when explanation is clearer visually
   - static screenshot/image/text card only when sufficient
-> generate narration
-> generate subtitles from narration timing
-> assemble/edit timeline
-> render draft
-> AI reviews draft
-> corrections
-> final render
-> preview in RepoTunnel

## Reference visual language already reviewed

The supplied references were sampled rather than watched end-to-end. Do not require rewatching in a future chat.

Reference IDs:
- hUZNPCSZDaQ
- qUHyCjOo8Z8
- U2hZFMVNSE0
- YmLp8qe87A0
- IauULFe1j-A
- ZaPbP9DwBOE
- kn6dxL53NkM
- 7yNvsFrwpp0
- dt_OMxufoGE
- LPZh9BOjkQs
- q1QQN08ZK6I
- aX7QAfld7hs

Observed techniques that matter to the product:
- progressive technical diagrams built in sync with narration
- devices/nodes/labels/arrows appearing one-by-one rather than static slides
- conceptual/programmatic animation for AI/LLM/mathematical ideas
- fast developer explainers with frequent visual changes
- news/story assembly using screenshots, articles, callouts and zooms
- real software/browser demonstrations
- UI/product motion
- polished 3D/UI scenes for special cases
- vertical Shorts/Reels pacing with large readable captions and fast cuts
- narration-synchronized typography, highlights and transitions

The production engine therefore chooses the simplest visual method that explains each scene clearly. It must not open Blender/DaVinci/other heavy tools merely because they exist.

## Video Project storage

New Video Projects use a standalone project root under the user's Projects directory:

~/Projects/<project-slug>/
  .repotunnel-video-project
  video-project.json
  script/
  storyboard/
  recordings/
    raw/
    selected/
  animations/
    generated/
    source/
  assets/
    images/
    video/
    audio/
  narration/
  subtitles/
  timeline/
  thumbnails/
  renders/
    drafts/
    final/
  qa/
  licenses/

RepoTunnel keeps the originating approved workspace association as the permission/context boundary. Legacy workspace-relative Video Projects remain readable through their recorded relative path for backward compatibility.

All generated or imported production assets must stay inside the resolved Video Project root unless the user explicitly exports/copies them elsewhere. RepoTunnel validates the standalone marker/manifest identity and rejects symlink/path escape.

## Project manifest

video-project.json is the durable source of truth. It records:
- project ID/name/slug/workspace
- status and timestamps
- output aspect ratio, resolution and FPS
- script/storyboard/timeline relative paths
- recording/animation/narration/subtitle assets
- current preview
- latest draft
- final export
- production checkpoints
- external asset/license records
- errors/attention state

A new chat should recover production work from these manifests rather than asking the user to repeat the plan.

## Permissions and safety

- Use existing RepoTunnel approved-workspace boundaries.
- Production does not grant blanket filesystem access.
- Real screen recording requires existing Desktop Control permission and an explicit recording action/request from the connected AI.
- Recording captures only the requested display/window/region when possible.
- Do not intentionally record secrets, passwords, tokens, private notifications or unrelated user content. Tutorial credentials should be synthetic/demo credentials where practical.
- Existing command/browser/Git/application security paths remain authoritative.
- One AI must not steal a browser/application/recording resource actively used by another RepoTunnel task.
- Never publish/upload a video without explicit user authorization.
- Never fabricate a successful software demonstration; if the real step fails, the production records the real failure or the scene is clearly labeled as an illustration.
- Do not overwrite earlier renders; drafts/finals are versioned.

## Rendering strategy v1

Use existing FFmpeg as the mandatory base renderer/encoder.

Initial generated animation capability should be built in RepoTunnel rather than requiring a third-party website:
- SVG/HTML-like scene descriptions generated by AI
- text, boxes, icons/shapes, arrows/lines, highlights
- fade/slide/scale/draw/progressively-reveal timing
- 16:9 and 9:16 canvases
- deterministic rendering to frames/video
- synchronized to narration/storyboard timing

The current story-animation layer can route supported shots through the native motion renderer and installed Godot 4, Blender Grease Pencil/2.5D/3D, and Rhubarb lip-sync capabilities. OpenToonz and Synfig remain optional external adapters when installed. Other tools such as Manim, DaVinci Resolve, Kdenlive, or Audacity are optional workflow helpers rather than mandatory production dependencies.

## External visual/animation source strategy

RepoTunnel must not hard-code the product to one animation website. Use a provider registry so the AI can search/import the simplest legally usable asset when native generated scenes are not enough.

Provider classes to support:
- native RepoTunnel generated SVG/diagram/motion scenes (preferred first)
- user-provided local assets
- Lottie/.lottie animation providers
- open SVG/illustration/icon providers
- stock image/video providers
- CC0/public-domain 3D/HDRI/texture providers
- sound-effect/music providers

Initial useful provider candidates include LottieFiles, Openverse/Wikimedia Commons, Pexels, Pixabay, Mixkit, Poly Haven, and similar sources whose current license/API terms permit the requested use. unDraw can be used manually in a video when its license allows the end use, but RepoTunnel must not build automated scraping/search integration against it without separate permission because its current license restricts integrations/scraping.

Provider rules:
- check the current asset license/terms before download/use; do not assume a site's terms remain unchanged
- store source URL, author/creator when available, license identifier/text reference, attribution requirement, and retrieval date in the Video Project licenses/ metadata
- reject or flag an asset when the intended video use is not clearly permitted
- never scrape a provider that forbids automated access; prefer documented APIs/download mechanisms
- do not redistribute source assets as a standalone asset library
- imported assets are copied into the Video Project before use so final rendering is reproducible
- the AI chooses native/generated visuals first when that is faster, clearer, or avoids licensing/network dependency
- provider support is expandable; adding a new site should be one adapter, not a renderer rewrite

## Narration, languages, voices, subtitles

Narration is multilingual by design rather than English-only.

The production model stores:
- BCP-47 language tag (examples: en-US, en-IN, hi-IN, te-IN)
- optional voice/provider ID
- speaking rate, pitch/style when the selected provider supports them
- source narration text
- generated audio asset
- word/sentence timing when available

The AI may produce the whole video in one language or intentionally mix languages when requested. Subtitle generation follows the narration language/timing and can additionally create translated subtitle tracks.

Narration provider architecture:
- local/offline provider first when a suitable voice exists
- optional network/provider adapters only when the user allows the provider and its terms/cost are acceptable
- provider selection is automatic based on requested language/voice and availability
- no paid service is a mandatory dependency of the base product
- original narration audio and subtitle tracks stay inside the Video Project
- microphone/camera recording is not required for AI tutorial production

The project should be able to add more voices/languages later without changing storyboard/timeline/render formats.

## Continuity rule

Before ending a development session:
- update this blueprint only if a locked design decision changes
- update docs/video-production-state.json with implemented stages, tests, blockers and next action
- update RepoTunnel Project Memory
- never require the next chat to rediscover the reference videos

## Initial implementation order

1. Durable Video Project model and workspace folders.
2. Video Project list/create/open UI.
3. Script/storyboard/timeline persistence.
4. Recording job API with permission/resource checks and workspace-local output.
5. Basic generated 2D scene renderer.
6. Narration provider abstraction and subtitle files.
7. FFmpeg assembly/edit/render pipeline.
8. Preview/player in Video Project.
9. AI/MCP production tools.
10. End-to-end tutorial production test.

## Implementation completion record

Completed and validated on 2026-09-21.

All ten locked implementation stages are complete. The finished base workflow includes durable Video Projects, project-owned script/storyboard/timeline persistence, safe AI Workspace recording, generated 2D scenes, multilingual narration/subtitles, FFmpeg timeline assembly, secure preview, AI/MCP production tools, the expandable asset/license registry, and an integrated end-to-end tutorial production test.

Final validation evidence:
- TypeScript check: PASS
- frontend tests: 6 passed, 0 failed
- production frontend build: PASS
- npm audit: 0 vulnerabilities
- Rust unit/integration suite: 198 passed, 0 failed, 1 ignored
- the ignored managed Supertonic network/download test was run separately and passed for English and Hindi
- Clippy with warnings denied: PASS
- real X11 AI Workspace recording smoke test: PASS
- generated-scene rendering smoke test: PASS
- real FFmpeg video+narration assembly smoke test: PASS
- integrated Video Project end-to-end tutorial pipeline: PASS
- Direct HTTPS regression after dependency security update: 6 passed, 0 failed
- release gate/cargo audit: PASS with no release-blocking vulnerabilities

The release gate discovered RUSTSEC-2026-0285 against rustls 0.23.44. RepoTunnel now locks rustls 0.23.45. No Direct HTTPS source implementation was changed for that dependency-only security update.

Historical v1 Debian validation artifact:
- path: src-tauri/target/release/bundle/deb/RepoTunnel_0.3.1_amd64.deb
- size: 15,988,858 bytes
- SHA-256: de64e3138f631363c8d59cccb22b45dcaee0af3597614f9bca411e75393949c7
- the binary extracted from this historical package resolved all dynamic libraries and survived an isolated Xvfb startup smoke test with HOME/XDG state redirected to a temporary profile

Current release artifact, revalidated for v0.4.1 on 2026-10-05:
- path: src-tauri/target/release/bundle/deb/RepoTunnel_0.4.1_amd64.deb
- size: 19,828,744 bytes
- DEB SHA-256: 328353f1cbfb1bd85c0cf0d02eb476ff148a560e3564563296eed2097c7e8fbd
- packaged binary SHA-256: 4eaf65cf4bd735ee39d2bc24e99a42a117a2592ff119ca9e163ad0295adde381
- package metadata: repo-tunnel 0.4.1, amd64
- package control archive contains only control and md5sums; there are no maintainer install/remove scripts
- final release gate: 43/43 frontend tests passed; 448 Rust tests passed, 0 failed, 8 intentionally ignored
- source freshness check: PASS after the final Phone live-view/control fix

The locked Video Production scope is complete. Installation, staging, commit, push, publishing, or future scope expansion remain separate explicit user actions.

## Visual-quality pipeline v2

Added after the v1 production workflow proved technically reliable but visually too slideshow-like.

### Renderer policy

For tutorials, explainers, promos and reels, the default generated-scene path is now HTML/CSS + GSAP with deterministic headless-Chrome frame capture. The timeline is explicitly sought per output frame; RepoTunnel does not screen-record the browser. CSS flex/grid owns text/card alignment. The original native SVG/2D renderer remains available as fallback only.

Do not install or download AI, GPU, diffusion or video-generation models unless the user explicitly requests them. Before selecting a production method, inspect CPU, RAM, GPU and disk without installing anything and record why the selected method fits the machine. Lightweight CPU speech tooling such as TTS/Whisper remains allowed behind the existing permission/download policy. Manim may be used for math/graph work only when already installed; it must never be installed automatically. Other already-available free tools such as Remotion, Blender bpy, Godot movie mode, MoviePy, FFmpeg, Kdenlive and Audacity may be selected when they materially improve the result. Paid or watermarked tools/assets are not part of the default workflow.

### Reusable HTML scene library

The parameterized template catalog is:

- intro
- title_bullets
- card_grid
- flow_steps
- git_graph
- code_typing
- terminal
- comparison
- stats_chart
- quote
- lower_third
- outro

Planning must pick one of these first and use a custom scene only when none fits.

### Design system

The 1920x1080 reference layout uses a 120px safe area. Titles are at least 72px, body text 40px, labels 36px and captions 44px. Cards use flex/grid centering instead of manual text coordinates. Content must visibly occupy at least 60% of the frame. The built-in themes are Modern dark and Playful bright; accents/layouts vary by scene.

### Motion system

Generated HTML scenes use GSAP easing/stagger, continuous background movement, a slow camera push/pan, progressive diagram construction, animated arrows/pulses and slide/wipe/zoom transitions. The QA threshold treats an effectively static stretch of 1.5 seconds or longer as a failure for tutorial production.

### Caption system

Caption generation prefers clause/phrase boundaries around seven words, prevents dangling articles/prepositions/conjunctions at line breaks, applies a safe bottom position and high-contrast styling, and emits an optional word-highlight WebVTT track with inline word timestamps.

### Required tutorial workflow

spec check -> script -> storyboard -> template/theme -> assets/voice -> build scenes -> 480p/15fps design preview -> design QA -> inspect 4 or 5 representative preview frames -> fix and repeat preview if needed -> final 1080p render -> FFprobe/full QA verification

Final rendering is blocked until the required preview/design/frame-review gates have actually passed. Meaningful script/storyboard/scene/caption changes invalidate stale downstream approvals so an old preview cannot authorize a changed final video.

HTML-scene DOM QA uses getBoundingClientRect to verify safe area, clipping, overlap, text size, container centering and frame coverage before encoding. The preview/final media QA also verifies motion, caption phrasing, stream/resolution/FPS state and existing audio/technical checks.

After a completed video, the AI reports the tools used and why, the observed machine specs, elapsed production-step timings when they can be derived from persisted timestamps, and the final output path. It must not invent missing timing data.
