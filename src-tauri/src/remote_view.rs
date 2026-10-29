//! User-initiated optional remote screen viewing through an existing RustDesk.
//! Not an AI/MCP tool and never starts screen sharing without a user click.
use std::{
    env,
    path::PathBuf,
    process::{Command, Stdio},
};

fn valid_peer_id(input: &str) -> bool {
    let id = input.trim();
    (5..=80).contains(&id.len())
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@'))
}

fn candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for env_var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(path) = env::var_os(env_var) {
                let root = PathBuf::from(path);
                paths.push(root.join("RustDesk").join("rustdesk.exe"));
                paths.push(root.join("Programs").join("RustDesk").join("rustdesk.exe"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from(
            "/Applications/RustDesk.app/Contents/MacOS/RustDesk",
        ));
        paths.push(PathBuf::from(
            "/Applications/RustDesk.app/Contents/MacOS/rustdesk",
        ));
        paths.push(PathBuf::from("/opt/homebrew/bin/rustdesk"));
        paths.push(PathBuf::from("/usr/local/bin/rustdesk"));
    }
    #[cfg(target_os = "linux")]
    {
        paths.push(PathBuf::from("/usr/bin/rustdesk"));
        paths.push(PathBuf::from("/usr/local/bin/rustdesk"));
        paths.push(PathBuf::from("/snap/bin/rustdesk"));
    }
    if let Some(search) = env::var_os("PATH") {
        for folder in env::split_paths(&search) {
            #[cfg(windows)]
            paths.push(folder.join("rustdesk.exe"));
            #[cfg(not(windows))]
            paths.push(folder.join("rustdesk"));
        }
    }
    paths
}
pub(crate) fn launch(peer_id: &str) -> Result<(), String> {
    if !valid_peer_id(peer_id) {
        return Err("Enter a valid RustDesk peer ID (5–80 letters, digits, dots, underscores, hyphens or @).".into());
    }
    let executable = candidates().into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| "RustDesk is not installed or not discoverable on this computer. Install it separately to use optional screen viewing; structured remote RepoTunnel tools work without it.".to_string())?;
    Command::new(executable)
        .arg("--connect")
        .arg(peer_id.trim())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Could not launch the installed RustDesk client.".to_string())?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::valid_peer_id;
    #[test]
    fn rustdesk_peer_id_is_never_interpreted_as_shell_input() {
        for good in ["123456789", "test-pc_01", "office@relay.example"] {
            assert!(valid_peer_id(good), "{good}");
        }
        for bad in [
            "",
            "abc",
            "--help",
            "; rm -rf",
            "abc/../../",
            "test\ndir",
            "https://host",
            "12345;whoami",
            "12345$(id)",
            "12345 --password secret",
        ] {
            assert!(!valid_peer_id(bad), "{bad}");
        }
    }
}
