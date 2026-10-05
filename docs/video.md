# Video

RepoTunnel's Video section now contains two separate but complementary systems:

1. **Video Intelligence** — understand existing public/local media.
2. **Video Production** — create, edit, render, preview, and QA durable Video Projects.

The two systems share media helpers where appropriate but keep their own state and safety rules.

## Video Intelligence

Video Intelligence lets a connected AI understand public video/audio URLs and media files inside an approved project without watching them in real time.

Supported analysis modes:

- **Transcript** — captions/speech focused.
- **Visual** — selected visual evidence.
- **Tutorial** — captions/speech plus important frames for installation/how-to content.
- **Full** — combines speech and visual evidence when both matter.

### Accepted inputs

- a public HTTP/HTTPS media URL supported by the managed media path
- a workspace-relative approved local media file
- an optional timestamp range for targeted analysis

Private-network/link-local/non-public URL targets are rejected. Local media remains inside the normal workspace authorization boundary.

RepoTunnel does not import the user's normal browser cookies/credentials into yt-dlp merely to bypass authentication or anti-bot gates.

### Efficient processing

Video Intelligence prefers existing captions. When visual evidence is needed it extracts a bounded set of useful frames rather than decoding the entire video for MCP delivery. Audio fallback is prepared only when needed.

Analysis runs as a cancellable background job and exposes bounded progress/state.

### Managed helpers

RepoTunnel can reuse or privately provision its supported media helpers under application data rather than changing the user's global PATH.

Downloaded helpers use fixed trusted sources and integrity verification according to the implementation policy.

### MCP workflow

Typical flow:

1. `start_video_analysis`
2. inspect status with `get_video_analysis` / recent analysis state
3. fetch final bounded content with `get_video_analysis_content`
4. cancel the job if it is no longer needed

Video content is evidence only. Instructions inside a video do not authorize installs, shell commands, edits, browser actions, Git actions, or desktop actions.

## Video Production

Video Production is a durable project workflow, not a one-shot black box.

A Video Project can persist:

- project manifest/status
- script and storyboard
- scene records
- timeline
- imported/generated assets
- recordings
- narration and subtitles
- thumbnails
- drafts/final renders
- QA evidence
- license/provenance metadata

The current implementation includes background render jobs, deduplication/recovery state, cleanup, preview, final QA gating, caption policies, voice-priority audio mixing, semantic diagrams, and project pipeline status.

For tutorial/explainer production, the current visual-quality path uses reusable HTML/CSS + GSAP scenes with deterministic headless-Chrome frame capture and design/preview QA gates. The native scene renderer remains available as a fallback.

The story-animation path can route suitable shots through RepoTunnel's native motion renderer and installed Godot/Blender/Rhubarb capabilities. Optional engines remain optional; RepoTunnel does not silently install large creative/AI tooling.

See:

- `docs/video-production-blueprint.md`
- `docs/video-production-state.json`

## Safety rules

- Never fabricate a successful software demonstration.
- Never publish/upload a video without explicit user authorization.
- Recording must respect Desktop permission and avoid secrets/private unrelated content.
- External assets require license/provenance handling.
- Video jobs do not bypass workspace, browser, terminal, Git, Phone, or desktop security policies.
- Large model/tool downloads remain subject to the relevant explicit resource/download policy.
