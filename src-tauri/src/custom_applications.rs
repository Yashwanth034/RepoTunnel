use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::models::LaunchApplication;

const STORE_SCHEMA: u32 = 1;
const STORE_FILE: &str = "custom-applications.json";

static STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CustomApplication {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) executable: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredCustomApplication {
    id: String,
    name: String,
    executable: String,
    #[serde(default)]
    executable_sha256: String,
    #[serde(default)]
    executable_identity: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CustomApplicationStore {
    schema: u32,
    applications: Vec<StoredCustomApplication>,
}

impl Default for CustomApplicationStore {
    fn default() -> Self {
        Self {
            schema: STORE_SCHEMA,
            applications: Vec::new(),
        }
    }
}

fn load_store(path: &Path) -> Result<CustomApplicationStore, String> {
    if !path.exists() {
        let backup = path.with_extension("json.backup");
        if cfg!(windows) && backup.is_file() {
            fs::copy(&backup, path).map_err(|error| {
                format!("Could not recover custom application settings: {error}")
            })?;
        } else {
            return Ok(CustomApplicationStore::default());
        }
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read custom application settings: {error}"))?;
    if text.trim().is_empty() {
        return Ok(CustomApplicationStore::default());
    }
    let store: CustomApplicationStore = serde_json::from_str(&text)
        .map_err(|error| format!("Saved custom application settings are invalid: {error}"))?;
    if store.schema != STORE_SCHEMA {
        return Err("Saved custom application settings use an unsupported schema.".to_string());
    }
    Ok(store)
}

fn save_store(path: &Path, store: &CustomApplicationStore) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve custom application storage directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not prepare custom application storage: {error}"))?;
    let text = serde_json::to_string_pretty(store)
        .map_err(|error| format!("Could not serialize custom application settings: {error}"))?;
    let temporary = parent.join(format!(".custom-applications.{}.tmp", std::process::id()));
    fs::write(&temporary, text)
        .map_err(|error| format!("Could not stage custom application settings: {error}"))?;

    if cfg!(windows) && path.exists() {
        let backup = path.with_extension("json.backup");
        let _ = fs::remove_file(&backup);
        fs::rename(path, &backup).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            format!(
                "Could not prepare existing custom application settings for replacement: {error}"
            )
        })?;
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::rename(&backup, path);
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "Could not save custom application settings: {error}"
            ));
        }
        let _ = fs::remove_file(backup);
        return Ok(());
    }

    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "Could not save custom application settings: {error}"
        ));
    }
    Ok(())
}

fn normalized_program_name(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}

fn reject_restricted_program(path: &Path) -> Result<(), String> {
    let name = normalized_program_name(path);
    let normalized = name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>();
    let exact_blocked = [
        "sh",
        "bash",
        "dash",
        "zsh",
        "fish",
        "cmd",
        "powershell",
        "pwsh",
        "python",
        "python2",
        "python3",
        "node",
        "nodejs",
        "ruby",
        "perl",
        "php",
        "java",
        "jshell",
        "xterm",
        "konsole",
        "kitty",
        "alacritty",
        "wezterm",
        "foot",
        "tmux",
        "screen",
        "gnometerminal",
        "xfce4terminal",
        "docker",
    ];
    let browser_tokens = [
        "chrome",
        "chromium",
        "firefox",
        "librewolf",
        "bravebrowser",
        "microsoftedge",
        "msedge",
        "safari",
        "vivaldi",
        "opera",
        "arc",
        "zenbrowser",
        "waterfox",
        "floorp",
        "thorium",
    ];
    if normalized.contains("repotunnel")
        || exact_blocked.iter().any(|blocked| normalized == *blocked)
        || browser_tokens
            .iter()
            .any(|blocked| normalized.contains(blocked))
    {
        return Err(
            "That application is not allowed as a custom AI Workspace app. Browsers, shells, terminals, interpreters, Docker, and RepoTunnel itself use dedicated protected flows."
                .to_string(),
        );
    }
    Ok(())
}

fn validate_native_binary_format(path: &Path) -> Result<(), String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Could not inspect the selected application binary: {error}"))?;
    let mut header = [0_u8; 4];
    let read = file
        .read(&mut header)
        .map_err(|error| format!("Could not inspect the selected application binary: {error}"))?;
    #[cfg(windows)]
    let native = read >= 2 && header[..2] == *b"MZ";
    #[cfg(target_os = "linux")]
    let native = read == 4 && header == [0x7f, b'E', b'L', b'F'];
    #[cfg(target_os = "macos")]
    let native = read == 4
        && matches!(
            header,
            [0xfe, 0xed, 0xfa, 0xce]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
                | [0xca, 0xfe, 0xba, 0xbf]
                | [0xbf, 0xba, 0xfe, 0xca]
        );
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let native = false;
    if !native {
        return Err(
            "Custom AI Workspace applications must be native application binaries; scripts and interpreter launchers are not allowed."
                .to_string(),
        );
    }
    Ok(())
}

fn executable_sha256(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Could not fingerprint the selected application: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Could not fingerprint the selected application: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn executable_identity(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not identify the selected application: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Device and inode alone can be reused immediately after uninstall.
        // Two byte-identical binaries at the same path must not silently share
        // approval after that replacement. ctime has nanosecond precision and
        // changes when the underlying inode is recreated or its metadata is
        // modified. Legacy dev/inode-only records fail closed and need approval.
        Ok(format!(
            "unix:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ))
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(format!(
            "windows:{}:{}",
            metadata.creation_time(),
            metadata.file_size()
        ))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        Ok(format!("generic:{}:{modified}", metadata.len()))
    }
}

fn validate_regular_executable(path: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect the selected application: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Select a regular application executable, not a symbolic link.".to_string());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err("The selected file is not executable.".to_string());
        }
    }

    #[cfg(windows)]
    if !path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return Err("Custom applications on Windows must be .exe files.".to_string());
    }

    validate_native_binary_format(path)?;

    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("Could not resolve the selected application: {error}"))?;
    reject_restricted_program(&canonical)?;
    Ok(canonical)
}

fn resolve_selected_executable(path: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect the selected application: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("Custom applications cannot be symbolic links.".to_string());
    }
    if metadata.is_file() {
        return validate_regular_executable(path);
    }
    if metadata.is_dir()
        && path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
    {
        let macos = path.join("Contents").join("MacOS");
        let preferred = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(|name| macos.join(name));
        if let Some(preferred) = preferred {
            if preferred.is_file() {
                return validate_regular_executable(&preferred);
            }
        }
        let candidates = fs::read_dir(&macos)
            .map_err(|error| format!("Could not inspect the selected .app bundle: {error}"))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|candidate| validate_regular_executable(candidate).is_ok())
            .collect::<Vec<_>>();
        if candidates.len() == 1 {
            return validate_regular_executable(&candidates[0]);
        }
        return Err(
            "RepoTunnel could not identify one safe executable inside that .app bundle."
                .to_string(),
        );
    }
    Err("Select an installed application executable or a macOS .app bundle.".to_string())
}

fn display_name(selected: &Path, executable: &Path) -> String {
    selected
        .file_stem()
        .or_else(|| executable.file_stem())
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Custom application")
        .chars()
        .take(120)
        .collect()
}

fn application_id(executable: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(executable.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let suffix = digest[..10]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("custom-{suffix}")
}

fn public_application(application: &StoredCustomApplication) -> CustomApplication {
    CustomApplication {
        id: application.id.clone(),
        name: application.name.clone(),
        executable: application.executable.clone(),
    }
}

fn custom_to_launch(application: &StoredCustomApplication) -> LaunchApplication {
    LaunchApplication {
        id: application.id.clone(),
        name: application.name.clone(),
        category: "custom".to_string(),
        executable: application.executable.clone(),
        supports_urls: false,
        supports_paths: true,
    }
}

fn classify_stored_applications_at(
    path: &Path,
) -> Result<(Vec<StoredCustomApplication>, Vec<StoredCustomApplication>), String> {
    let store = load_store(path)?;
    let mut applications = Vec::new();
    let mut revoked = Vec::new();
    for application in store.applications {
        let valid = if application.executable_sha256.is_empty()
            || application.executable_identity.is_empty()
        {
            false
        } else {
            let executable = Path::new(&application.executable);
            validate_regular_executable(executable).is_ok()
                && executable_sha256(executable)
                    .is_ok_and(|current| current == application.executable_sha256)
                && executable_identity(executable)
                    .is_ok_and(|current| current == application.executable_identity)
        };
        if valid {
            applications.push(application);
        } else {
            revoked.push(application);
        }
    }
    Ok((applications, revoked))
}

fn reconcile_stored_applications_at(
    path: &Path,
) -> Result<(Vec<StoredCustomApplication>, Vec<StoredCustomApplication>), String> {
    let (applications, revoked) = classify_stored_applications_at(path)?;
    if !revoked.is_empty() {
        save_store(
            path,
            &CustomApplicationStore {
                schema: STORE_SCHEMA,
                applications: applications.clone(),
            },
        )?;
    }
    Ok((applications, revoked))
}

fn list_stored_applications_at(path: &Path) -> Result<Vec<StoredCustomApplication>, String> {
    reconcile_stored_applications_at(path).map(|(applications, _)| applications)
}

#[cfg(test)]
fn list_applications_at(path: &Path) -> Result<Vec<CustomApplication>, String> {
    Ok(list_stored_applications_at(path)?
        .iter()
        .map(public_application)
        .collect())
}

fn add_application_at(path: &Path, selected: &Path) -> Result<CustomApplication, String> {
    add_application_at_named(path, selected, None)
}

fn add_application_at_named(
    path: &Path,
    selected: &Path,
    requested_name: Option<&str>,
) -> Result<CustomApplication, String> {
    let executable = resolve_selected_executable(selected)?;
    let executable_sha256 = executable_sha256(&executable)?;
    let executable_identity = executable_identity(&executable)?;
    let id = application_id(&executable);
    let mut applications = list_stored_applications_at(path)?;
    if let Some(existing) = applications.iter().find(|application| application.id == id) {
        return Ok(public_application(existing));
    }
    let application = StoredCustomApplication {
        id,
        name: requested_name
            .map(str::trim)
            .filter(|value| {
                !value.is_empty() && value.len() <= 120 && !value.chars().any(char::is_control)
            })
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| display_name(selected, &executable)),
        executable: executable.to_string_lossy().into_owned(),
        executable_sha256,
        executable_identity,
    };
    applications.push(application.clone());
    applications.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    save_store(
        path,
        &CustomApplicationStore {
            schema: STORE_SCHEMA,
            applications,
        },
    )?;
    Ok(public_application(&application))
}

fn remove_application_at(
    path: &Path,
    application_id: &str,
) -> Result<Vec<CustomApplication>, String> {
    let applications = list_stored_applications_at(path)?;
    if !applications
        .iter()
        .any(|application| application.id == application_id)
    {
        return Err("That custom application is not currently approved.".to_string());
    }
    let remaining = applications
        .into_iter()
        .filter(|application| application.id != application_id)
        .collect::<Vec<_>>();
    save_store(
        path,
        &CustomApplicationStore {
            schema: STORE_SCHEMA,
            applications: remaining.clone(),
        },
    )?;
    Ok(remaining.iter().map(public_application).collect())
}

#[cfg(test)]
fn resolve_launch_application_at(
    path: &Path,
    application_id: &str,
) -> Result<LaunchApplication, String> {
    list_stored_applications_at(path)?
        .into_iter()
        .find(|application| application.id == application_id)
        .map(|application| custom_to_launch(&application))
        .ok_or_else(|| "That custom application is no longer installed or approved.".to_string())
}

fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|directory| directory.join(STORE_FILE))
        .map_err(|error| {
            format!("Could not resolve RepoTunnel custom application storage: {error}")
        })
}

fn revoke_running_application_ids(
    app: &AppHandle,
    application_ids: &[String],
) -> Result<(), String> {
    if application_ids.is_empty() {
        return Ok(());
    }
    let state = app.state::<crate::app_state::AppState>();
    for application_id in application_ids {
        if let Some((workspace_id, app_session_ids)) = state
            .ai_workspace
            .app_sessions_for_application(app, application_id)?
        {
            for app_session_id in app_session_ids {
                if let Some(recording) =
                    crate::video_production::get_recording_status(&workspace_id, None)?
                {
                    if recording.status == "recording" && recording.app_session_id == app_session_id
                    {
                        crate::video_production::stop_recording(
                            &workspace_id,
                            &recording.project_id,
                        )?;
                    }
                }
                if let Err(error) =
                    state
                        .ai_workspace
                        .stop_app_session(app, &workspace_id, &app_session_id)
                {
                    if state
                        .ai_workspace
                        .app_session_exists(&workspace_id, &app_session_id)?
                    {
                        return Err(format!(
                            "Could not revoke the running custom application session: {error}"
                        ));
                    }
                }
                crate::ai_resources::release_ai_workspace_app_all_owners(
                    &workspace_id,
                    &app_session_id,
                );
            }
        }
    }
    Ok(())
}

fn reconcile_registered_applications(
    app: &AppHandle,
    path: &Path,
) -> Result<Vec<StoredCustomApplication>, String> {
    let (applications, revoked) = classify_stored_applications_at(path)?;
    if revoked.is_empty() {
        return Ok(applications);
    }
    let revoked_ids = revoked
        .iter()
        .map(|application| application.id.clone())
        .collect::<Vec<_>>();
    revoke_running_application_ids(app, &revoked_ids)?;
    save_store(
        path,
        &CustomApplicationStore {
            schema: STORE_SCHEMA,
            applications: applications.clone(),
        },
    )?;
    Ok(applications)
}

pub(crate) fn list(app: &AppHandle) -> Result<Vec<CustomApplication>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Custom application settings are unavailable.".to_string())?;
    Ok(reconcile_registered_applications(app, &store_path(app)?)?
        .iter()
        .map(public_application)
        .collect())
}

pub(crate) fn list_launch_applications(app: &AppHandle) -> Result<Vec<LaunchApplication>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Custom application settings are unavailable.".to_string())?;
    Ok(reconcile_registered_applications(app, &store_path(app)?)?
        .iter()
        .map(custom_to_launch)
        .collect())
}

pub(crate) fn resolve_launch_application(
    app: &AppHandle,
    application_id: &str,
) -> Result<LaunchApplication, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Custom application settings are unavailable.".to_string())?;
    reconcile_registered_applications(app, &store_path(app)?)?
        .into_iter()
        .find(|application| application.id == application_id)
        .map(|application| custom_to_launch(&application))
        .ok_or_else(|| "That custom application is no longer installed or approved.".to_string())
}

// Read-only desktop-entry discovery. The UI presents names, never raw file
// paths or shell commands. Selection re-scans and re-validates the native ELF
// before using the existing identity-pinned custom-app approval store.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoverableApplication {
    id: String,
    name: String,
    launch_mode: String,
}

#[cfg(target_os = "linux")]
fn installed_desktop_entry_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
        PathBuf::from("/var/lib/snapd/desktop/applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/applications"));
        roots.push(home.join(".local/share/flatpak/exports/share/applications"));
    }
    roots
}

#[cfg(target_os = "linux")]
fn parse_desktop_entry(contents: &str) -> Option<(String, String)> {
    let mut in_entry = false;
    let (mut name, mut exec) = (None, None);
    let mut is_application = false;
    for line in contents.lines().take(550) {
        let line = line.trim();
        if line == "[Desktop Entry]" {
            in_entry = true;
            continue;
        }
        if line.starts_with('[') {
            in_entry = false;
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("Type=") {
            if value != "Application" {
                return None;
            }
            is_application = true;
        }
        if line == "Hidden=true"
            || line == "NoDisplay=true"
            || line == "Terminal=true"
            || line == "Type=Link"
        {
            return None;
        }
        if let Some(value) = line.strip_prefix("Name=") {
            if !value.trim().is_empty() && value.len() <= 120 {
                name = Some(value.trim().to_string());
            }
        }
        if let Some(value) = line.strip_prefix("Exec=") {
            // The executable is the *first* argument only; never evaluate
            // desktop-entry commands, shell expansions, env wrappers or flags.
            let binary = if let Some(rest) = value.strip_prefix('"') {
                rest.split_once('"')?.0
            } else {
                value.split_whitespace().next()?
            };
            if !binary.is_empty()
                && !binary.starts_with('-')
                && !binary.contains('%')
                && !binary.contains('=')
                && binary.len() <= 1024
            {
                exec = Some(binary.to_string());
            }
        }
    }
    if !is_application {
        return None;
    }
    Some((name?, exec?))
}

#[cfg(target_os = "linux")]
fn resolve_desktop_binary(program: &str) -> Option<PathBuf> {
    // Do not treat common command dispatchers as GUI apps. Many desktop
    // entries use 'flatpak run ...' or 'env ...'; running the dispatcher
    // without its reviewed arguments is not the intended application.
    let basename = Path::new(program)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    if matches!(
        basename.as_str(),
        "env"
            | "flatpak"
            | "snap"
            | "gtk-launch"
            | "gio"
            | "xdg-open"
            | "pkexec"
            | "sudo"
            | "doas"
            | "dbus-launch"
            | "systemd-run"
            | "sh"
            | "bash"
            | "python"
            | "python3"
            | "node"
    ) {
        return None;
    }
    if program.starts_with('/') {
        return Some(PathBuf::from(program));
    }
    if program.contains('/') || program.contains('\\') || program.contains(':') {
        return None;
    }
    // A desktop entry may choose its own name but cannot inject an invocation
    // or launch a wrapper: the candidate still must pass validate_regular_executable.
    for dir in [
        "/usr/bin",
        "/usr/local/bin",
        "/bin",
        "/usr/games",
        "/opt/bin",
    ] {
        let path = Path::new(dir).join(program);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn desktop_entry_candidates(roots: &[PathBuf]) -> Vec<(DiscoverableApplication, PathBuf)> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for directory in roots {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        // Bounded enumeration; do not scan arbitrary recursively nested folders.
        for entry in entries.take(2500).flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > 48 * 1024
            {
                continue;
            }
            let Ok(contents) = fs::read_to_string(&path) else {
                continue;
            };
            let Some((name, program)) = parse_desktop_entry(&contents) else {
                continue;
            };
            let Some(candidate) = resolve_desktop_binary(&program) else {
                continue;
            };
            let Ok(executable) = validate_regular_executable(&candidate) else {
                continue;
            };
            if !seen.insert(executable.clone()) {
                continue;
            }
            let id = application_id(&executable);
            found.push((
                DiscoverableApplication {
                    id,
                    name,
                    launch_mode: "native".to_string(),
                },
                executable,
            ));
            if found.len() >= 300 {
                break;
            }
        }
        if found.len() >= 300 {
            break;
        }
    }
    found.sort_by(|a, b| {
        a.0.name
            .to_ascii_lowercase()
            .cmp(&b.0.name.to_ascii_lowercase())
    });
    found
}

// Package launchers use their own isolation model. They can be pinned and
// launched through the normal per-project Review/Automatic command policy,
// but they are NOT approved as native AI Workspace executables.
#[cfg(target_os = "linux")]
fn package_desktop_candidates(roots: &[PathBuf]) -> Vec<(DiscoverableApplication, Vec<String>)> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for directory in roots {
        // Only OS/package-manager exported .desktop entries, not arbitrary
        // desktop files elsewhere in the user's filesystem.
        let root = directory.to_string_lossy();
        let flatpak = root.contains("/flatpak/exports/share/applications");
        let snap = root == "/var/lib/snapd/desktop/applications";
        if !flatpak && !snap {
            continue;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.take(2500).flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > 48 * 1024
            {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(content) = std::str::from_utf8(&bytes) else {
                continue;
            };
            let Some((name, _)) = parse_desktop_entry(content) else {
                continue;
            };
            // Never offer RepoTunnel recursion even as a package shortcut.
            if name.to_ascii_lowercase().contains("repotunnel") {
                continue;
            }
            let exec = content
                .lines()
                .take(550)
                .take_while(|line| !line.starts_with("[Desktop Action"))
                .find_map(|line| line.strip_prefix("Exec="))
                .unwrap_or("");
            let argv = exec.split_whitespace().collect::<Vec<_>>();
            let launch = if flatpak {
                // The ID must match the installed exported Flatpak entry.
                let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let valid_id = id.len() <= 160
                    && !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
                let flatpak_exec = argv
                    .iter()
                    .any(|s| *s == "flatpak" || *s == "/usr/bin/flatpak");
                let has_run = argv.contains(&"run");
                if !valid_id
                    || !flatpak_exec
                    || !has_run
                    || id.to_ascii_lowercase().contains("repotunnel")
                {
                    continue;
                }
                vec!["flatpak".to_string(), "run".to_string(), id.to_string()]
            } else {
                // Snap desktop entries use a snap-managed /snap/bin wrapper
                // (often with an 'env BAMF_DESKTOP_FILE_HINT=...' prefix).
                let snapbin = argv.iter().find(|s| s.starts_with("/snap/bin/"));
                let Some(snapbin) = snapbin else {
                    continue;
                };
                let command = Path::new(snapbin);
                let Some(basename) = command.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if basename.to_ascii_lowercase().contains("repotunnel")
                    || basename.is_empty()
                    || basename.len() > 150
                    || !basename
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
                {
                    continue;
                }
                if !command.exists() {
                    continue;
                }
                vec!["snap".to_string(), (*snapbin).to_string()]
            };
            let mut hash = Sha256::new();
            hash.update(path.as_os_str().to_string_lossy().as_bytes());
            hash.update(bytes.as_slice());
            // An update/reinstall with identical .desktop contents must still
            // invalidate the old pin, not silently restore its approval.
            let Ok(file_identity) = executable_identity(&path) else {
                continue;
            };
            hash.update(file_identity.as_bytes());
            let suffix = hash.finalize();
            let id = format!(
                "desktop-{}",
                suffix[..12]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            if !seen.insert(id.clone()) {
                continue;
            }
            found.push((
                DiscoverableApplication {
                    id,
                    name,
                    launch_mode: "desktop".to_string(),
                },
                launch,
            ));
            if found.len() >= 250 {
                break;
            }
        }
    }
    found.sort_by(|a, b| {
        a.0.name
            .to_ascii_lowercase()
            .cmp(&b.0.name.to_ascii_lowercase())
    });
    found
}

#[cfg(target_os = "linux")]
fn list_discoverable_applications() -> Vec<DiscoverableApplication> {
    let roots = installed_desktop_entry_roots();
    let mut native = desktop_entry_candidates(&roots)
        .into_iter()
        .map(|(app, _)| app)
        .collect::<Vec<_>>();
    native.extend(
        package_desktop_candidates(&roots)
            .into_iter()
            .map(|(app, _)| app),
    );
    native.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    native
}

pub(crate) fn desktop_application_name(id: &str) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        if !id.starts_with("desktop-") {
            return Err("Not a supported installed app shortcut.".into());
        }
        package_desktop_candidates(&installed_desktop_entry_roots())
            .into_iter()
            .find(|(entry, _)| entry.id == id)
            .map(|(entry, _)| entry.name)
            .ok_or_else(|| {
                "That Flatpak/Snap app changed or is no longer installed. Add it again.".to_string()
            })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = id;
        Err("Flatpak/Snap desktop shortcuts require Linux.".into())
    }
}

pub(crate) fn launch_desktop_application(id: &str) -> Result<u32, String> {
    #[cfg(target_os = "linux")]
    {
        let (_, argv) = package_desktop_candidates(&installed_desktop_entry_roots())
            .into_iter()
            .find(|(entry, _)| entry.id == id && entry.launch_mode == "desktop")
            .ok_or_else(|| {
                "That installed application changed. Remove and add its shortcut again.".to_string()
            })?;
        let (program, args) = argv
            .split_first()
            .ok_or("Installed application launch command is missing.")?;
        let executable = if program == "flatpak" {
            PathBuf::from("/usr/bin/flatpak")
        } else if program == "snap" {
            PathBuf::from(
                args.first()
                    .ok_or("Snap executable path is missing.")?
                    .as_str(),
            )
        } else {
            return Err("Unsupported packaged application launcher.".into());
        };
        if !executable.exists() {
            return Err("The package manager launcher is no longer available.".into());
        }
        let cmdargs = if program == "snap" {
            vec![]
        } else {
            args.to_vec()
        };
        let mut command = std::process::Command::new(&executable);
        command
            .args(cmdargs)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|e| format!("Could not launch the installed application: {e}"))?;
        let pid = child.id();
        std::thread::spawn(move || {
            let _ = child.wait_with_output();
        });
        Ok(pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = id;
        Err("Packaged desktop launch is only supported on Linux.".into())
    }
}

#[tauri::command]
pub(crate) async fn discover_custom_applications() -> Result<Vec<DiscoverableApplication>, String> {
    tauri::async_runtime::spawn_blocking(|| -> Result<Vec<DiscoverableApplication>, String> {
        #[cfg(target_os = "linux")]
        {
            Ok(list_discoverable_applications())
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Vec::new())
        }
    })
    .await
    .map_err(|e| format!("Application discovery did not finish: {e}"))?
}

#[tauri::command]
pub(crate) async fn add_discovered_custom_application(
    app: AppHandle,
    application_id: String,
) -> Result<CustomApplication, String> {
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "linux")]
        {
            let (entry, executable) = desktop_entry_candidates(&installed_desktop_entry_roots())
                .into_iter().find(|(entry, _)| entry.id == application_id && entry.launch_mode == "native")
                .ok_or_else(|| "That installed app is unavailable or cannot be safely approved. Try another app.".to_string())?;
            let _guard = STORE_LOCK.lock().map_err(|_| "Custom application settings are unavailable.".to_string())?;
            add_application_at_named(&store_path(&app)?, &executable, Some(&entry.name))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = app;
            let _ = application_id;
            Err("Automatic custom app discovery is currently available on Linux.".to_string())
        }
    }).await.map_err(|e| format!("Application approval did not finish: {e}"))?
}

#[tauri::command]
pub(crate) fn list_custom_applications(app: AppHandle) -> Result<Vec<CustomApplication>, String> {
    list(&app)
}

#[tauri::command]
pub(crate) async fn pick_custom_application(
    app: AppHandle,
) -> Result<Option<CustomApplication>, String> {
    // Use the callback-based picker so the UI event loop remains free while the
    // native dialog is open. The blocking picker freezes RepoTunnel on Linux.
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Add application for RepoTunnel AI Workspace")
        .pick_file(move |selected| {
            let _ = sender.send(selected);
        });

    let selected = receiver
        .await
        .map_err(|_| "The application picker closed unexpectedly.".to_string())?;
    let Some(selected) = selected else {
        return Ok(None);
    };
    let selected = selected.into_path().map_err(|error| {
        format!("Could not resolve the application selected for RepoTunnel: {error}")
    })?;

    // Binary inspection and hashing can take time for large applications. Keep
    // this filesystem work off both the UI thread and the async executor.
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = STORE_LOCK
            .lock()
            .map_err(|_| "Custom application settings are unavailable.".to_string())?;
        add_application_at(&store_path(&app)?, &selected).map(Some)
    })
    .await
    .map_err(|error| format!("Could not register the selected application: {error}"))?
}

#[tauri::command]
pub(crate) fn remove_custom_application(
    app: AppHandle,
    application_id: String,
) -> Result<Vec<CustomApplication>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "Custom application settings are unavailable.".to_string())?;
    let application_id = application_id.trim();
    if application_id.is_empty() {
        return Err("Custom application ID cannot be empty.".to_string());
    }
    let path = store_path(&app)?;
    let approved = reconcile_registered_applications(&app, &path)?;
    if !approved
        .iter()
        .any(|application| application.id == application_id)
    {
        return Err("That custom application is not currently approved.".to_string());
    }

    revoke_running_application_ids(&app, &[application_id.to_string()])?;
    remove_application_at(&path, application_id)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::tempdir;

    #[cfg(target_os = "linux")]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, b"\x7fELFfake-native-test-binary").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(target_os = "macos")]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, [0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0]).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(windows)]
    fn make_executable(path: &Path) {
        fs::write(path, b"MZtest").unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn flatpak_desktop_shortcuts_are_discoverable_but_not_native_approved() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("flatpak/exports/share/applications");
        fs::create_dir_all(&folder).unwrap();
        let desktop = folder.join("org.example.Paint.desktop");
        fs::write(&desktop,
            "[Desktop Entry]\nType=Application\nName=Paint in Flatpak\nExec=/usr/bin/flatpak run --branch=stable org.example.Paint %U\n"
        ).unwrap();
        let items = super::package_desktop_candidates(std::slice::from_ref(&folder));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0.name, "Paint in Flatpak");
        assert_eq!(items[0].0.launch_mode, "desktop");
        assert_eq!(items[0].1, vec!["flatpak", "run", "org.example.Paint"]);
        assert!(items[0].0.id.starts_with("desktop-"));
        let old_id = items[0].0.id.clone();

        // A replacement or update is not automatically given the previous
        // launch approval: the caller must select the new fingerprint.
        fs::write(&desktop,
            "[Desktop Entry]\nType=Application\nName=Paint in Flatpak\nExec=/usr/bin/flatpak run org.example.Paint %U\n"
        ).unwrap();
        let replaced = super::package_desktop_candidates(std::slice::from_ref(&folder));
        assert_ne!(old_id, replaced[0].0.id);
        let changed_id = replaced[0].0.id.clone();
        let same_contents = fs::read(&desktop).unwrap();
        fs::remove_file(&desktop).unwrap();
        fs::write(&desktop, same_contents).unwrap();
        let reinstalled = super::package_desktop_candidates(&[folder]);
        assert_ne!(
            changed_id, reinstalled[0].0.id,
            "Reinstalled desktop app must require fresh approval even with identical bytes"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_launcher_ignores_non_flatpak_commands_and_bad_ids() {
        let temp = tempdir().unwrap();
        let folder = temp.path().join("flatpak/exports/share/applications");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join("wrong.desktop"),
            "[Desktop Entry]\nType=Application\nName=Bad\nExec=/usr/bin/bash -c surprise\n",
        )
        .unwrap();
        fs::write(
            folder.join("tricky;id.desktop"),
            "[Desktop Entry]\nType=Application\nName=Bad id\nExec=/usr/bin/flatpak run tricky;id\n",
        )
        .unwrap();
        fs::write(folder.join("hidden.desktop"),
            "[Desktop Entry]\nType=Application\nName=Hidden\nNoDisplay=true\nExec=/usr/bin/flatpak run hidden\n").unwrap();
        assert!(super::package_desktop_candidates(&[folder]).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn installed_app_discovery_finds_native_gui_without_executing_desktop_commands() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let apps = temp.path().join("applications");
        fs::create_dir(&apps).unwrap();
        let painter = temp.path().join("Painter");
        make_executable(&painter);
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=Friendly Painter\nExec={} --project %f\n",
            painter.display()
        );
        fs::write(apps.join("painter.desktop"), content).unwrap();
        fs::write(
            apps.join("blocked-browser.desktop"),
            "[Desktop Entry]\nType=Application\nName=Browser\nExec=/usr/bin/google-chrome\n",
        )
        .unwrap();
        let script = temp.path().join("Script");
        fs::write(&script, b"#!/bin/sh\ntrue\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            apps.join("malicious.desktop"),
            format!(
                "[Desktop Entry]\nName=Malicious\nExec={}\n",
                script.display()
            ),
        )
        .unwrap();
        fs::write(
            apps.join("hidden.desktop"),
            format!(
                "[Desktop Entry]\nName=Secret\nNoDisplay=true\nExec={}\n",
                painter.display()
            ),
        )
        .unwrap();
        let discovered = super::desktop_entry_candidates(&[apps]);
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].0.name, "Friendly Painter");
        assert_eq!(discovered[0].0.launch_mode, "native");
        assert_eq!(discovered[0].1, painter.canonicalize().unwrap());
        assert_eq!(discovered[0].0.id, super::application_id(&discovered[0].1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_entries_never_execute_shell_wrappers_or_untrusted_arguments() {
        assert!(super::parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Other\nExec=env X=1 /usr/bin/app\n"
        )
        .is_some());
        // An env wrapper may be parsed for display but must never be accepted
        // as an executable (non-native / forbidden shell path).
        assert!(
            super::resolve_desktop_binary("env").is_none()
                || super::validate_regular_executable(
                    &super::resolve_desktop_binary("env").unwrap()
                )
                .is_err()
        );
        assert!(super::parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Hidden\nHidden=true\nExec=/tmp/app\n"
        )
        .is_none());
        assert!(super::parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Web\nExec=/usr/bin/python3 -c 'hack'\n"
        )
        .is_some());
        assert!(super::parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Fake\nExec=/usr/bin/whoever;touch /tmp/bad\n"
        )
        .is_some());
        assert!(super::resolve_desktop_binary("not/a/command").is_none());
        for launcher in [
            "flatpak",
            "/usr/bin/flatpak",
            "snap",
            "xdg-open",
            "gio",
            "pkexec",
        ] {
            assert!(
                super::resolve_desktop_binary(launcher).is_none(),
                "{launcher} must not be approved as a GUI app"
            );
        }
    }

    #[test]
    fn add_list_remove_and_missing_cleanup_are_persistent() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let executable = directory.path().join(if cfg!(windows) {
            "Painter.exe"
        } else {
            "Painter"
        });
        make_executable(&executable);

        let first = super::add_application_at(&store, &executable).unwrap();
        let duplicate = super::add_application_at(&store, &executable).unwrap();
        assert_eq!(first.id, duplicate.id);
        assert_eq!(super::list_applications_at(&store).unwrap().len(), 1);

        fs::remove_file(&executable).unwrap();
        let (remaining, revoked) = super::reconcile_stored_applications_at(&store).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(revoked.len(), 1);
        assert_eq!(revoked[0].id, first.id);

        make_executable(&executable);
        assert!(
            super::list_applications_at(&store).unwrap().is_empty(),
            "an application removed because it was uninstalled must not silently return after reinstall"
        );

        let added_again = super::add_application_at(&store, &executable).unwrap();
        assert_eq!(added_again.id, first.id);
        assert_eq!(
            super::remove_application_at(&store, &added_again.id)
                .unwrap()
                .len(),
            0
        );
        assert!(super::list_applications_at(&store).unwrap().is_empty());
    }

    #[test]
    fn delete_and_reinstall_same_binary_at_same_path_requires_fresh_approval() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let executable = directory.path().join(if cfg!(windows) {
            "Painter.exe"
        } else {
            "Painter"
        });
        make_executable(&executable);
        super::add_application_at(&store, &executable).unwrap();
        assert_eq!(super::list_applications_at(&store).unwrap().len(), 1);

        fs::remove_file(&executable).unwrap();
        make_executable(&executable);

        assert!(
            super::list_applications_at(&store).unwrap().is_empty(),
            "deleting and reinstalling the same binary at the same path must require fresh approval even if RepoTunnel did not observe the missing interval"
        );
    }

    #[cfg(unix)]
    #[test]
    fn legacy_inode_only_approval_requires_fresh_explicit_approval() {
        use std::os::unix::fs::MetadataExt;

        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let executable = directory.path().join("Designer");
        make_executable(&executable);

        super::add_application_at(&store, &executable).unwrap();
        assert_eq!(super::list_applications_at(&store).unwrap().len(), 1);
        let metadata = fs::metadata(&executable).unwrap();
        let mut old_store = super::load_store(&store).unwrap();
        old_store.applications[0].executable_identity =
            format!("unix:{}:{}", metadata.dev(), metadata.ino());
        super::save_store(&store, &old_store).unwrap();

        assert!(
            super::list_applications_at(&store).unwrap().is_empty(),
            "older inode-only approvals must fail closed, not bypass reinstall detection"
        );
        let approved_again = super::add_application_at(&store, &executable).unwrap();
        assert!(!approved_again.id.is_empty());
        assert_eq!(super::list_applications_at(&store).unwrap().len(), 1);
    }

    #[test]
    fn removing_an_unapproved_or_builtin_application_id_is_rejected() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let executable = directory.path().join(if cfg!(windows) {
            "Designer.exe"
        } else {
            "Designer"
        });
        make_executable(&executable);
        super::add_application_at(&store, &executable).unwrap();

        assert!(super::remove_application_at(&store, "vscode").is_err());
        assert!(super::remove_application_at(&store, "custom-not-approved").is_err());
        assert_eq!(super::list_applications_at(&store).unwrap().len(), 1);
    }

    #[test]
    fn replacing_the_binary_at_the_same_path_revokes_approval() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let executable = directory.path().join(if cfg!(windows) {
            "Painter.exe"
        } else {
            "Painter"
        });
        make_executable(&executable);
        let approved = super::add_application_at(&store, &executable).unwrap();
        assert_eq!(super::list_applications_at(&store).unwrap().len(), 1);

        #[cfg(windows)]
        fs::write(&executable, b"MZdifferent-native-binary").unwrap();
        #[cfg(target_os = "linux")]
        fs::write(&executable, b"\x7fELFdifferent-native-binary").unwrap();
        #[cfg(target_os = "macos")]
        fs::write(
            &executable,
            [0xcf, 0xfa, 0xed, 0xfe, b'd', b'i', b'f', b'f'],
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&executable, permissions).unwrap();
        }

        assert!(
            super::list_applications_at(&store).unwrap().is_empty(),
            "replacing or reinstalling an approved executable at the same path must require fresh approval"
        );
        let added_again = super::add_application_at(&store, &executable).unwrap();
        assert_eq!(added_again.id, approved.id);
        assert_eq!(super::list_applications_at(&store).unwrap().len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_rejects_mach_o_as_a_custom_application() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let executable = directory.path().join("MacOnlyApp");
        fs::write(&executable, [0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0]).unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        assert!(super::add_application_at(&store, &executable).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_rejects_elf_as_a_custom_application() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let executable = directory.path().join("LinuxOnlyApp");
        fs::write(&executable, b"\x7fELFfake").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        assert!(super::add_application_at(&store, &executable).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn renamed_scripts_are_rejected_as_custom_gui_applications() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let executable = directory.path().join("FriendlyDesigner");
        fs::write(&executable, b"#!/bin/sh\necho nope\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();

        let error = super::add_application_at(&store, &executable).unwrap_err();
        assert!(error.to_ascii_lowercase().contains("native"));
    }

    #[test]
    fn dangerous_shells_browsers_and_repotunnel_are_rejected() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let names = if cfg!(windows) {
            vec![
                "powershell.exe",
                "chrome.exe",
                "safari.exe",
                "arc.exe",
                "repotunnel.exe",
            ]
        } else {
            vec![
                "bash",
                "google-chrome",
                "safari",
                "librewolf",
                "arc",
                "repotunnel",
            ]
        };
        for name in names {
            let executable = directory.path().join(name);
            make_executable(&executable);
            let error = super::add_application_at(&store, &executable).unwrap_err();
            assert!(
                error.to_ascii_lowercase().contains("not allowed"),
                "unexpected rejection for {name}: {error}"
            );
        }
    }

    #[test]
    fn launchable_custom_apps_have_safe_fixed_capabilities() {
        let directory = tempdir().unwrap();
        let store = directory.path().join("custom-applications.json");
        let executable = directory.path().join(if cfg!(windows) {
            "Designer.exe"
        } else {
            "Designer"
        });
        make_executable(&executable);
        let application = super::add_application_at(&store, &executable).unwrap();

        let launchable = super::resolve_launch_application_at(&store, &application.id).unwrap();
        assert_eq!(launchable.id, application.id);
        assert_eq!(launchable.category, "custom");
        assert!(!launchable.supports_urls);
        assert!(launchable.supports_paths);
        assert_eq!(
            launchable.executable,
            executable.canonicalize().unwrap().to_string_lossy()
        );
    }
}
