use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::{
    models::{
        EnvironmentToolDiagnostic, EnvironmentVariableDiagnostic, Workspace,
        WorkspaceEnvironmentDiagnostics,
    },
    secret_guard, terminal,
};

struct ToolSpec {
    id: &'static str,
    label: &'static str,
    category: &'static str,
    names: &'static [&'static str],
    version_args: &'static [&'static str],
    capabilities: &'static [&'static str],
    file_types: &'static [&'static str],
    cli: bool,
    scriptable: bool,
    gui_controllable: bool,
    launchable: bool,
}

const TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        id: "git",
        label: "Git",
        category: "development",
        names: &["git"],
        version_args: &["--version"],
        capabilities: &["version-control"],
        file_types: &[],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "node",
        label: "Node.js",
        category: "runtime",
        names: &["node"],
        version_args: &["--version"],
        capabilities: &["javascript-runtime", "automation"],
        file_types: &["js", "mjs", "cjs"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "npm",
        label: "npm",
        category: "development",
        names: &["npm"],
        version_args: &["--version"],
        capabilities: &["package-management"],
        file_types: &["json"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "python3",
        label: "Python 3",
        category: "runtime",
        names: &["python3", "python"],
        version_args: &["--version"],
        capabilities: &["python-runtime", "automation", "application-scripting"],
        file_types: &["py"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "cargo",
        label: "Cargo",
        category: "development",
        names: &["cargo"],
        version_args: &["--version"],
        capabilities: &["rust-build", "package-management"],
        file_types: &["toml"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "rustc",
        label: "Rust",
        category: "runtime",
        names: &["rustc"],
        version_args: &["--version"],
        capabilities: &["rust-compiler"],
        file_types: &["rs"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "java",
        label: "Java",
        category: "runtime",
        names: &["java"],
        version_args: &["-version"],
        capabilities: &["java-runtime"],
        file_types: &["jar", "class"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "javac",
        label: "Java compiler",
        category: "development",
        names: &["javac"],
        version_args: &["-version"],
        capabilities: &["java-compiler"],
        file_types: &["java"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "adb",
        label: "Android Debug Bridge",
        category: "device",
        names: &["adb"],
        version_args: &["version"],
        capabilities: &["android-device-control", "file-transfer", "diagnostics"],
        file_types: &["apk"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "ffmpeg",
        label: "FFmpeg",
        category: "media",
        names: &["ffmpeg"],
        version_args: &["-version"],
        capabilities: &[
            "video-transcode",
            "audio-transcode",
            "mux",
            "demux",
            "frame-extraction",
            "waveform-processing",
        ],
        file_types: &[
            "mp4", "mkv", "webm", "mov", "avi", "wav", "mp3", "aac", "flac", "srt", "vtt",
        ],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "ffprobe",
        label: "FFprobe",
        category: "media",
        names: &["ffprobe"],
        version_args: &["-version"],
        capabilities: &["media-metadata", "stream-inspection", "decode-validation"],
        file_types: &[
            "mp4", "mkv", "webm", "mov", "avi", "wav", "mp3", "aac", "flac",
        ],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "imagemagick",
        label: "ImageMagick",
        category: "image",
        names: &["magick", "convert"],
        version_args: &["--version"],
        capabilities: &[
            "image-convert",
            "image-resize",
            "image-metadata",
            "contact-sheet",
        ],
        file_types: &["png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "svg"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "blender",
        label: "Blender",
        category: "3d-animation",
        names: &["blender"],
        version_args: &["--version"],
        capabilities: &[
            "3d-animation",
            "2d-grease-pencil",
            "rendering",
            "python-scripting",
            "headless-rendering",
        ],
        file_types: &["blend", "fbx", "obj", "glb", "gltf", "usd", "abc"],
        cli: true,
        scriptable: true,
        gui_controllable: true,
        launchable: true,
    },
    ToolSpec {
        id: "godot",
        label: "Godot",
        category: "2d-3d-engine",
        names: &["godot4", "godot"],
        version_args: &["--version"],
        capabilities: &[
            "2d-animation",
            "3d-animation",
            "scene-rendering",
            "headless-execution",
            "scripting",
        ],
        file_types: &["godot", "tscn", "tres", "gd"],
        cli: true,
        scriptable: true,
        gui_controllable: true,
        launchable: true,
    },
    ToolSpec {
        id: "rhubarb",
        label: "Rhubarb Lip Sync",
        category: "audio-animation",
        names: &["rhubarb"],
        version_args: &["--version"],
        capabilities: &["lip-sync", "phoneme-cues"],
        file_types: &["wav", "ogg", "json", "tsv", "xml"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "inkscape",
        label: "Inkscape",
        category: "vector-graphics",
        names: &["inkscape"],
        version_args: &["--version"],
        capabilities: &["vector-editing", "svg-rendering", "image-export"],
        file_types: &["svg", "pdf", "eps", "png"],
        cli: true,
        scriptable: true,
        gui_controllable: true,
        launchable: true,
    },
    ToolSpec {
        id: "krita",
        label: "Krita",
        category: "image",
        names: &["krita"],
        version_args: &["--version"],
        capabilities: &["digital-painting", "image-editing", "frame-animation"],
        file_types: &["kra", "png", "jpg", "jpeg", "webp", "psd"],
        cli: true,
        scriptable: false,
        gui_controllable: true,
        launchable: true,
    },
    ToolSpec {
        id: "kdenlive",
        label: "Kdenlive",
        category: "video-editor",
        names: &["kdenlive"],
        version_args: &["--version"],
        capabilities: &[
            "timeline-editing",
            "video-editing",
            "audio-mixing",
            "rendering",
        ],
        file_types: &["kdenlive", "mp4", "mkv", "webm", "mov"],
        cli: false,
        scriptable: false,
        gui_controllable: true,
        launchable: true,
    },
    ToolSpec {
        id: "audacity",
        label: "Audacity",
        category: "audio-editor",
        names: &["audacity"],
        version_args: &[],
        capabilities: &["audio-editing", "waveform-editing", "recording"],
        file_types: &["aup3", "wav", "mp3", "flac", "ogg"],
        cli: false,
        scriptable: false,
        gui_controllable: true,
        launchable: true,
    },
    ToolSpec {
        id: "opentoonz",
        label: "OpenToonz",
        category: "2d-animation",
        names: &["opentoonz", "OpenToonz"],
        version_args: &[],
        capabilities: &["2d-animation", "xsheet", "drawing", "compositing"],
        file_types: &["tnz", "tlv", "pli"],
        cli: false,
        scriptable: false,
        gui_controllable: true,
        launchable: true,
    },
    ToolSpec {
        id: "synfig",
        label: "Synfig",
        category: "2d-animation",
        names: &["synfig", "synfigstudio"],
        version_args: &["--version"],
        capabilities: &["2d-vector-animation", "rendering"],
        file_types: &["sif", "sifz"],
        cli: true,
        scriptable: true,
        gui_controllable: true,
        launchable: true,
    },
    ToolSpec {
        id: "davinci-resolve",
        label: "DaVinci Resolve",
        category: "video-editor",
        names: &[
            "resolve",
            "/opt/resolve/bin/resolve",
            "/Applications/DaVinci Resolve/DaVinci Resolve.app/Contents/MacOS/Resolve",
        ],
        version_args: &[],
        capabilities: &[
            "video-editing",
            "color-grading",
            "audio-mixing",
            "compositing",
            "rendering",
        ],
        file_types: &["drp", "dra", "mp4", "mov", "mxf"],
        cli: false,
        scriptable: true,
        gui_controllable: true,
        launchable: true,
    },
    ToolSpec {
        id: "yt-dlp",
        label: "yt-dlp",
        category: "download",
        names: &["yt-dlp"],
        version_args: &["--version"],
        capabilities: &["public-media-download", "metadata"],
        file_types: &["mp4", "webm", "m4a", "json"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "unzip",
        label: "unzip",
        category: "archive",
        names: &["unzip"],
        version_args: &["-v"],
        capabilities: &["archive-extract"],
        file_types: &["zip"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "tar",
        label: "tar",
        category: "archive",
        names: &["tar"],
        version_args: &["--version"],
        capabilities: &["archive-create", "archive-extract"],
        file_types: &["tar", "gz", "bz2", "xz"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "cmake",
        label: "CMake",
        category: "development",
        names: &["cmake"],
        version_args: &["--version"],
        capabilities: &["native-build"],
        file_types: &["cmake"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
    ToolSpec {
        id: "docker",
        label: "Docker CLI",
        category: "development",
        names: &["docker"],
        version_args: &["--version"],
        capabilities: &["container-runtime"],
        file_types: &["dockerfile"],
        cli: true,
        scriptable: true,
        gui_controllable: false,
        launchable: false,
    },
];

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(windows)]
fn executable_names(name: &str) -> Vec<String> {
    if Path::new(name).extension().is_some() {
        return vec![name.to_string()];
    }
    let extensions = env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
    let mut names = vec![name.to_string()];
    names.extend(
        extensions
            .split(';')
            .filter(|value| !value.trim().is_empty())
            .map(|extension| format!("{name}{}", extension.to_ascii_lowercase())),
    );
    names
}

#[cfg(not(windows))]
fn executable_names(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

fn search_directories() -> Vec<PathBuf> {
    let mut directories = BTreeSet::new();
    if let Some(path) = env::var_os("PATH") {
        directories.extend(env::split_paths(&path));
    }

    #[cfg(not(windows))]
    {
        for path in ["/usr/local/bin", "/usr/bin", "/bin", "/snap/bin"] {
            directories.insert(PathBuf::from(path));
        }
        if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
            directories.insert(home.join(".local/bin"));
            directories.insert(home.join(".cargo/bin"));
            directories.insert(home.join("go/bin"));
        }
    }

    #[cfg(windows)]
    {
        if let Some(profile) = env::var_os("USERPROFILE").map(PathBuf::from) {
            directories.insert(profile.join(".cargo/bin"));
            directories.insert(profile.join("go/bin"));
        }
        if let Some(appdata) = env::var_os("APPDATA").map(PathBuf::from) {
            directories.insert(appdata.join("npm"));
        }
    }

    directories.into_iter().collect()
}

fn find_program(names: &[&str], workspace_root: &Path) -> Option<PathBuf> {
    let directories = search_directories();
    for name in names {
        let direct = PathBuf::from(name);
        if direct.components().count() > 1 && executable_file(&direct) {
            let resolved = direct.canonicalize().unwrap_or(direct);
            if !resolved.starts_with(workspace_root) {
                return Some(resolved);
            }
        }
        for executable_name in executable_names(name) {
            for directory in &directories {
                if directory.as_os_str().is_empty() || !directory.is_absolute() {
                    continue;
                }
                let candidate = directory.join(&executable_name);
                if executable_file(&candidate) {
                    let resolved = candidate.canonicalize().unwrap_or(candidate);
                    if !resolved.starts_with(workspace_root) {
                        return Some(resolved);
                    }
                }
            }
        }
    }
    None
}

fn bounded_version(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let redacted = secret_guard::redact_text(line);
    Some(redacted.chars().take(300).collect())
}

fn tool_version(path: &Path, args: &[&str]) -> Option<String> {
    if args.is_empty() {
        return None;
    }
    let output = Command::new(path)
        .args(args)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .output()
        .ok()?;
    bounded_version(&String::from_utf8_lossy(&output.stdout))
        .or_else(|| bounded_version(&String::from_utf8_lossy(&output.stderr)))
}

fn variable(name: &str, hidden: bool) -> EnvironmentVariableDiagnostic {
    let value = env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    EnvironmentVariableDiagnostic {
        name: name.to_string(),
        present: value.is_some(),
        value: if hidden {
            None
        } else {
            value.as_deref().map(secret_guard::redact_text)
        },
        hidden: hidden && value.is_some(),
    }
}

fn build_tool(spec: &ToolSpec, workspace_root: &Path) -> EnvironmentToolDiagnostic {
    let host_path = find_program(spec.names, workspace_root);
    let sandbox_path = host_path
        .as_deref()
        .and_then(terminal::ai_sandbox_executable_for_diagnostics);
    let host_available = host_path.is_some();
    EnvironmentToolDiagnostic {
        id: spec.id.to_string(),
        label: spec.label.to_string(),
        category: spec.category.to_string(),
        capabilities: spec
            .capabilities
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        supported_file_types: spec
            .file_types
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        cli: spec.cli,
        scriptable: spec.scriptable,
        gui_controllable: spec.gui_controllable,
        launchable: spec.launchable && host_available,
        host_available,
        host_executable: host_path
            .as_deref()
            .map(|path| path.to_string_lossy().into_owned()),
        host_version: host_path
            .as_deref()
            .and_then(|path| tool_version(path, spec.version_args)),
        sandbox_available: sandbox_path.is_some(),
        sandbox_executable: sandbox_path,
        installation_required: !host_available,
    }
}

pub(crate) fn tool_capabilities(workspace: &Workspace) -> Vec<EnvironmentToolDiagnostic> {
    let workspace_root = PathBuf::from(&workspace.path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&workspace.path));
    TOOL_SPECS
        .iter()
        .map(|spec| build_tool(spec, &workspace_root))
        .collect()
}

pub(crate) fn diagnostics(workspace: &Workspace) -> WorkspaceEnvironmentDiagnostics {
    let tools = tool_capabilities(workspace);
    let host_process_path = env::var("PATH").unwrap_or_default();
    let sandbox_path = terminal::ai_sandbox_path_for_diagnostics();
    let sandbox_workspace_path = terminal::ai_sandbox_workspace_path_for_diagnostics(workspace);

    let sdk_variables = [
        "JAVA_HOME",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
        "ANDROID_AVD_HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "SDKMAN_DIR",
    ]
    .into_iter()
    .map(|name| variable(name, false))
    .collect::<Vec<_>>();

    let gui_variables = [
        ("DISPLAY", false),
        ("WAYLAND_DISPLAY", false),
        ("XDG_SESSION_TYPE", false),
        ("DESKTOP_SESSION", false),
        ("DBUS_SESSION_BUS_ADDRESS", true),
        ("XDG_RUNTIME_DIR", true),
    ]
    .into_iter()
    .map(|(name, hidden)| variable(name, hidden))
    .collect::<Vec<_>>();

    let mut differences = Vec::new();
    if workspace.path != sandbox_workspace_path {
        differences.push(format!(
            "Workspace path maps from {} on the host to {} inside the AI command sandbox.",
            workspace.path, sandbox_workspace_path
        ));
    }
    if host_process_path != sandbox_path {
        differences.push(
            "RepoTunnel's host-process PATH and AI command sandbox PATH are different.".to_string(),
        );
    }
    for tool in &tools {
        if tool.host_available && !tool.sandbox_available {
            differences.push(format!(
                "{} is available to RepoTunnel on the host{} but is not mapped into the AI command sandbox.",
                tool.label,
                tool.host_executable
                    .as_deref()
                    .map(|path| format!(" at {path}"))
                    .unwrap_or_default()
            ));
        }
    }

    WorkspaceEnvironmentDiagnostics {
        workspace_id: workspace.id.clone(),
        workspace_name: workspace.name.clone(),
        platform: env::consts::OS.to_string(),
        architecture: env::consts::ARCH.to_string(),
        host_workspace_path: workspace.path.clone(),
        sandbox_workspace_path,
        host_process_path: secret_guard::redact_text(&host_process_path),
        sandbox_path,
        tools,
        sdk_variables,
        gui_variables,
        differences,
        notes: vec![
            "Host PATH is the environment inherited by RepoTunnel. Diagnostics intentionally do not execute the user's shell profile or rc files.".to_string(),
            "A host-integrated RepoTunnel feature may use a detected host executable even when that executable is intentionally unavailable inside the AI command sandbox.".to_string(),
            "DBUS_SESSION_BUS_ADDRESS and XDG_RUNTIME_DIR report presence only; their raw values are intentionally hidden.".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_hiding_never_returns_hidden_value() {
        let name = "REPOTUNNEL_ENVIRONMENT_DIAGNOSTICS_SECRET_TEST";
        std::env::set_var(name, "private-local-value");
        let diagnostic = variable(name, true);
        assert!(diagnostic.present);
        assert!(diagnostic.hidden);
        assert_eq!(diagnostic.value, None);
        std::env::remove_var(name);
    }

    #[test]
    fn diagnostics_tool_catalog_contains_core_build_tools() {
        let ids = TOOL_SPECS
            .iter()
            .map(|tool| tool.id)
            .collect::<BTreeSet<_>>();
        for required in [
            "git", "python3", "cargo", "node", "ffmpeg", "blender", "godot",
        ] {
            assert!(ids.contains(required));
        }
    }
}
