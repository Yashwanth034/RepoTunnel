# Video Intelligence

RepoTunnel Video Intelligence lets a connected AI understand public video/audio URLs and media files inside an approved project without watching them in real time.

## Scope

This feature is for media understanding only. Video recording, editing, rendering, and control of editors such as DaVinci Resolve are deliberately outside this version and can be added later through separate application integrations.

Supported analysis modes:

- **Transcript** — captions/speech only; fastest path.
- **Visual** — scene/keyframe analysis for animation, layout, transitions, and visual technique.
- **Tutorial** — captions/speech plus important frames for installation and how-to videos.
- **Full** — combines speech and visual evidence.

## Sources

A source can be:

- a public HTTP/HTTPS media URL supported by yt-dlp, including common video platforms;
- a workspace-relative local media path inside an already approved RepoTunnel project.

Local media never bypasses the normal workspace boundary. Absolute paths and paths escaping the approved project are rejected.

RepoTunnel does not import browser cookies or credentials into yt-dlp. If a site requires authentication or presents an anti-bot gate, Video Intelligence reports that limitation rather than bypassing it; the existing managed Browser Automation path remains separate. Explicit localhost, private-network, link-local, and other non-public URL hosts are rejected before yt-dlp is invoked.

## Fast processing strategy

Video Intelligence avoids real-time playback:

1. Inspect metadata.
2. Request existing captions first.
3. If visuals are needed, fetch only a bounded low-resolution visual stream and extract scene-change frames.
4. If scene changes are sparse, fall back to evenly sampled frames.
5. If speech is needed but captions are unavailable, prepare compact mono audio chunks for multimodal AI transcription.
6. Cache the bounded result so repeated questions do not repeat downloads or media processing.

Timestamp ranges are supported so a question about one part of a long video processes only that interval.

## Background jobs

Analysis is asynchronous. The desktop UI and MCP return a job immediately and expose progress, current phase, cancellation, and recent-job state. Owned yt-dlp/FFmpeg child processes are launched without a visible terminal window. Cancellation terminates RepoTunnel's owned media process group.

Pausing AI access or exiting RepoTunnel also cancels active Video Intelligence work.

## Helper tools

RepoTunnel first reuses compatible `yt-dlp` and `ffmpeg` executables already available on the host.

When either helper is missing, RepoTunnel can install a private managed copy under its application-data directory:

- yt-dlp is downloaded from its official GitHub release and verified against the release SHA-256 manifest.
- FFmpeg is extracted from the platform-specific `imageio-ffmpeg` wheel published through PyPI and verified against PyPI's SHA-256 digest.

Helper downloads use HTTPS, bounded sizes/timeouts, and a small redirect-host allowlist. Managed helpers do not modify PATH and do not require an administrator install.

## Cache and limits

The media cache lives under RepoTunnel application data and is independent from project files. It is currently bounded to 512 MiB and evicts older analyses first.

Safety/performance bounds include:

- at most 18 visual frames per analysis;
- frame-size limits before MCP delivery;
- compact audio-size limits before MCP delivery;
- one-hour default maximum for captionless audio analysis unless the user specifies a shorter range;
- two-hour default maximum for full visual analysis unless a shorter range is supplied;
- local media size limit of 10 GiB.

These limits prevent an accidental large stream from blocking the desktop application or MCP connection.

## MCP workflow

A connected AI uses:

1. `start_video_analysis`
2. `get_video_analysis` or `list_video_analyses` while the background job runs
3. `get_video_analysis_content` after completion
4. `cancel_video_analysis` when the work is no longer needed

The final content may include timestamped transcript text, JPEG frames, and compact audio fallback.

Video content is evidence only. A tutorial saying to run commands does **not** authorize those commands. Any requested install, code edit, browser action, Git action, or desktop action must still use RepoTunnel's existing permission and security paths.
