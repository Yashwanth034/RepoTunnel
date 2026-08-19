use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::{connection, execution, models::RuntimeDiagnostics};

const LOG_RELATIVE_PATH: &str = "logs/repotunnel.log";
const MAX_LOG_BYTES: u64 = 1024 * 1024;
const ROTATED_LOGS: usize = 3;
const AUTOSTART_FILENAME: &str = "repotunnel.desktop";

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub(crate) fn log_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(LOG_RELATIVE_PATH, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve the RepoTunnel log path: {error}"))
}

fn data_directory(app: &AppHandle) -> Result<PathBuf, String> {
    let marker = app
        .path()
        .resolve("workspaces.json", BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve the RepoTunnel data directory: {error}"))?;
    marker
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "Could not resolve the RepoTunnel data directory.".to_string())
}

fn redact_sensitive(value: &str) -> String {
    const SENSITIVE_MARKERS: [&str; 10] = [
        "authorization",
        "authtoken",
        "ngrok_authtoken",
        "bearer ",
        "api_key",
        "api key",
        "control_plane_api_key",
        "private key",
        "secret=",
        "token=",
    ];

    let mut result = String::new();
    for line in value.lines().take(80) {
        let lowered = line.to_ascii_lowercase();
        if lowered.contains("sk-")
            || SENSITIVE_MARKERS
                .iter()
                .any(|marker| lowered.contains(marker))
        {
            result.push_str("[REDACTED]\n");
            continue;
        }

        let sanitized: String = line
            .chars()
            .map(|character| {
                if character.is_control() && character != '\t' {
                    ' '
                } else {
                    character
                }
            })
            .take(1024)
            .collect();
        result.push_str(&sanitized);
        result.push('\n');
    }

    result.trim_end().to_string()
}

fn rotate_logs(path: &Path) -> Result<(), String> {
    if path.metadata().map(|metadata| metadata.len()).unwrap_or(0) < MAX_LOG_BYTES {
        return Ok(());
    }

    for index in (1..=ROTATED_LOGS).rev() {
        let source = if index == 1 {
            path.to_path_buf()
        } else {
            path.with_extension(format!("log.{}", index - 1))
        };
        let destination = path.with_extension(format!("log.{index}"));

        if index == ROTATED_LOGS && destination.exists() {
            let _ = fs::remove_file(&destination);
        }
        if source.exists() {
            let _ = fs::rename(source, destination);
        }
    }

    Ok(())
}

fn append_log(path: &Path, level: &str, event: &str, detail: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create the RepoTunnel log directory: {error}"))?;
    }
    rotate_logs(path)?;

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not open the RepoTunnel log: {error}"))?;
    writeln!(
        file,
        "{} [{}] {} {}",
        now_millis(),
        level,
        event,
        redact_sensitive(detail)
    )
    .map_err(|error| format!("Could not write the RepoTunnel log: {error}"))
}

pub(crate) fn log_event(app: &AppHandle, level: &str, event: &str, detail: &str) {
    if let Ok(path) = log_path(app) {
        let _ = append_log(&path, level, event, detail);
    }
}

fn runtime_file_is_stale(path: &Path, name: &str) -> bool {
    let pid = name
        .split('-')
        .nth(2)
        .and_then(|value| value.parse::<u32>().ok());

    #[cfg(target_os = "linux")]
    if let Some(pid) = pid {
        if Path::new("/proc").join(pid.to_string()).exists() {
            return false;
        }
    }

    let age = path
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());

    age.map(|duration| duration.as_secs() >= 60 * 60)
        .unwrap_or(false)
}

fn cleanup_stale_runtime_files() -> usize {
    let Ok(entries) = fs::read_dir(env::temp_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let owned =
            name.starts_with("repotunnel-health-") || name.starts_with("repotunnel-tunnel-");
        if owned
            && entry.path().is_file()
            && runtime_file_is_stale(&entry.path(), &name)
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

fn install_panic_hook(path: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown".to_string());
        let payload = if let Some(message) = info.payload().downcast_ref::<&str>() {
            (*message).to_string()
        } else if let Some(message) = info.payload().downcast_ref::<String>() {
            message.clone()
        } else {
            "non-string panic payload".to_string()
        };
        let _ = append_log(
            &path,
            "ERROR",
            "panic",
            &format!("location={location} message={payload}"),
        );
        previous(info);
    }));
}

pub(crate) fn initialize(app: &AppHandle) -> Result<(), String> {
    let path = log_path(app)?;
    install_panic_hook(path);
    let removed = cleanup_stale_runtime_files();
    log_event(
        app,
        "INFO",
        "startup",
        &format!(
            "version={} stale_runtime_files_removed={removed}",
            app.package_info().version
        ),
    );
    Ok(())
}

pub(crate) fn shutdown(app: &AppHandle) {
    log_event(
        app,
        "INFO",
        "shutdown",
        "RepoTunnel is shutting down cleanly.",
    );
}

#[cfg(target_os = "linux")]
fn autostart_path() -> Result<PathBuf, String> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config_home)
            .join("autostart")
            .join(AUTOSTART_FILENAME));
    }
    let home = env::var_os("HOME")
        .ok_or_else(|| "HOME is unavailable; launch-at-login cannot be configured.".to_string())?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("autostart")
        .join(AUTOSTART_FILENAME))
}

#[cfg(not(target_os = "linux"))]
fn autostart_path() -> Result<PathBuf, String> {
    Err("Launch at login is currently supported only on Linux.".to_string())
}

#[cfg(target_os = "linux")]
fn launch_executable() -> Result<PathBuf, String> {
    if let Some(appimage) = env::var_os("APPIMAGE") {
        let path = PathBuf::from(appimage);
        if path.is_absolute() && path.is_file() {
            return Ok(path);
        }
    }
    env::current_exe().map_err(|error| format!("Could not find the RepoTunnel executable: {error}"))
}

#[cfg(target_os = "linux")]
fn desktop_exec_value(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$");
    format!("\"{escaped}\"")
}

pub(crate) fn launch_at_login_enabled() -> bool {
    autostart_path().map(|path| path.is_file()).unwrap_or(false)
}

pub(crate) fn set_launch_at_login(app: &AppHandle, enabled: bool) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        let _ = enabled;
        return Err("Launch at login is currently supported only on Linux.".to_string());
    }

    #[cfg(target_os = "linux")]
    {
        let path = autostart_path()?;
        if !enabled {
            if path.exists() {
                fs::remove_file(&path)
                    .map_err(|error| format!("Could not disable launch at login: {error}"))?;
            }
            log_event(app, "INFO", "autostart", "enabled=false");
            return Ok(());
        }

        let executable = launch_executable()?;
        if fs::symlink_metadata(&path)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(
                "Refusing to write the launch-at-login entry through a symbolic link.".to_string(),
            );
        }

        let parent = path
            .parent()
            .ok_or_else(|| "Could not resolve the Linux autostart directory.".to_string())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create the Linux autostart directory: {error}"))?;

        let contents = format!(
            "[Desktop Entry]\nType=Application\nName=RepoTunnel\nComment=Secure local workspace gateway for AI tools\nExec={}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            desktop_exec_value(&executable)
        );
        fs::write(&path, contents)
            .map_err(|error| format!("Could not enable launch at login: {error}"))?;
        log_event(app, "INFO", "autostart", "enabled=true");
        Ok(())
    }
}

fn command_available(program: &str, version_arg: &str) -> bool {
    Command::new(program)
        .arg(version_arg)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub(crate) fn diagnostics(app: &AppHandle) -> Result<RuntimeDiagnostics, String> {
    let execution = execution::execution_status();
    let tunnel_client = connection::detect_tunnel_client();
    let git_available = command_available("git", "--version");
    let data_directory = data_directory(app)?;
    let log_file = log_path(app)?;
    let mut warnings = Vec::new();

    if !execution.sandbox_available {
        warnings
            .push("Bubblewrap is unavailable; sandboxed project commands cannot run.".to_string());
    }
    if tunnel_client.is_none() {
        warnings.push(
            "OpenAI tunnel-client is unavailable; OpenAI Secure Tunnel mode is unavailable."
                .to_string(),
        );
    }
    if !git_available {
        warnings.push("Git is unavailable; Git workflow features cannot run.".to_string());
    }

    Ok(RuntimeDiagnostics {
        version: app.package_info().version.to_string(),
        platform: env::consts::OS.to_string(),
        architecture: env::consts::ARCH.to_string(),
        data_directory: data_directory.to_string_lossy().into_owned(),
        log_file: log_file.to_string_lossy().into_owned(),
        launch_at_login: launch_at_login_enabled(),
        sandbox_available: execution.sandbox_available,
        tunnel_client_available: tunnel_client.is_some(),
        git_available,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::redact_sensitive;

    #[test]
    fn redacts_common_secret_markers() {
        assert_eq!(redact_sensitive("Authorization: Bearer abc"), "[REDACTED]");
        assert_eq!(redact_sensitive("CONTROL_PLANE_API_KEY=abc"), "[REDACTED]");
        assert_eq!(redact_sensitive("token=abc"), "[REDACTED]");
        assert_eq!(redact_sensitive("ngrok_authtoken=abc"), "[REDACTED]");
    }

    #[test]
    fn keeps_normal_diagnostics() {
        assert_eq!(
            redact_sensitive("gateway started on loopback"),
            "gateway started on loopback"
        );
    }
}
