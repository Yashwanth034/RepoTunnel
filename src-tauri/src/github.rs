use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{mpsc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::launcher;

const GITHUB_HOST: &str = "github.com";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEVICE_VERIFICATION_URL: &str = "https://github.com/login/device";

struct AuthRuntime {
    child: Child,
    output_rx: mpsc::Receiver<String>,
    device_code: Option<String>,
    started: Instant,
}

static AUTH_RUNTIME: OnceLock<Mutex<Option<AuthRuntime>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GithubConnectionStatus {
    pub available: bool,
    pub connected: bool,
    pub connecting: bool,
    pub username: Option<String>,
    pub device_code: Option<String>,
    pub verification_url: Option<String>,
    pub message: Option<String>,
}

fn auth_runtime() -> &'static Mutex<Option<AuthRuntime>> {
    AUTH_RUNTIME.get_or_init(|| Mutex::new(None))
}

fn executable_name() -> &'static str {
    if cfg!(windows) {
        "gh.exe"
    } else {
        "gh"
    }
}

fn github_cli_path() -> Option<PathBuf> {
    if let Some(path_value) = env::var_os("PATH") {
        if let Some(candidate) = env::split_paths(&path_value)
            .map(|directory| directory.join(executable_name()))
            .find(|candidate| candidate.is_file())
        {
            return Some(candidate);
        }
    }

    #[cfg(not(windows))]
    for candidate in [
        "/usr/bin/gh",
        "/usr/local/bin/gh",
        "/opt/homebrew/bin/gh",
        "/opt/local/bin/gh",
    ] {
        let path = Path::new(candidate);
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }

    #[cfg(windows)]
    for variable in ["ProgramFiles", "ProgramW6432", "LocalAppData"] {
        let Some(root) = env::var_os(variable) else {
            continue;
        };
        let root = PathBuf::from(root);
        for relative in [
            Path::new("GitHub CLI").join("gh.exe"),
            Path::new("Programs").join("GitHub CLI").join("gh.exe"),
        ] {
            let candidate = root.join(relative);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

fn run_gh(gh: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new(gh)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Could not run GitHub CLI: {error}"))
}

fn username(gh: &Path) -> Option<String> {
    let output = run_gh(gh, &["api", "user", "--jq", ".login"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn raw_status(gh: &Path) -> GithubConnectionStatus {
    let auth = match run_gh(gh, &["auth", "status", "--hostname", GITHUB_HOST]) {
        Ok(output) => output,
        Err(error) => {
            return GithubConnectionStatus {
                available: true,
                connected: false,
                connecting: false,
                username: None,
                device_code: None,
                verification_url: None,
                message: Some(error),
            };
        }
    };

    if !auth.status.success() {
        return GithubConnectionStatus {
            available: true,
            connected: false,
            connecting: false,
            username: None,
            device_code: None,
            verification_url: None,
            message: None,
        };
    }

    GithubConnectionStatus {
        available: true,
        connected: true,
        connecting: false,
        username: username(gh),
        device_code: None,
        verification_url: None,
        message: None,
    }
}

fn extract_device_code(line: &str) -> Option<String> {
    line.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-'))
        .find(|token| {
            let bytes = token.as_bytes();
            bytes.len() == 9
                && bytes.get(4) == Some(&b'-')
                && bytes.iter().enumerate().all(|(index, byte)| {
                    index == 4 || byte.is_ascii_uppercase() || byte.is_ascii_digit()
                })
        })
        .map(str::to_string)
}

fn spawn_output_reader<R: Read + Send + 'static>(reader: R, sender: mpsc::Sender<String>) {
    let _ = thread::Builder::new()
        .name("repotunnel-github-auth-output".to_string())
        .spawn(move || {
            for line in BufReader::new(reader).lines().map_while(Result::ok) {
                let _ = sender.send(line);
            }
        });
}

fn setup_git_credentials(gh: &Path) -> Option<String> {
    match run_gh(gh, &["auth", "setup-git", "--hostname", GITHUB_HOST]) {
        Ok(output) if output.status.success() => None,
        Ok(_) => Some(
            "GitHub connected, but Git credential setup did not complete. GitHub CLI operations are available, but HTTPS Git may need setup.".to_string(),
        ),
        Err(error) => Some(format!(
            "GitHub connected, but Git credential setup could not run: {error}"
        )),
    }
}

fn take_runtime() -> Option<AuthRuntime> {
    auth_runtime().lock().ok()?.take()
}

fn stop_runtime() {
    if let Some(mut runtime) = take_runtime() {
        if runtime.child.try_wait().ok().flatten().is_none() {
            let _ = runtime.child.kill();
        }
        let _ = runtime.child.wait();
    }
}

pub(crate) fn status() -> GithubConnectionStatus {
    let Some(gh) = github_cli_path() else {
        stop_runtime();
        return GithubConnectionStatus {
            available: false,
            connected: false,
            connecting: false,
            username: None,
            device_code: None,
            verification_url: None,
            message: Some("GitHub CLI is not installed on this computer.".to_string()),
        };
    };

    let mut current = raw_status(&gh);
    if current.connected {
        if let Some(mut runtime) = take_runtime() {
            if runtime.child.try_wait().ok().flatten().is_none() {
                let _ = runtime.child.kill();
            }
            let _ = runtime.child.wait();
            current.message = setup_git_credentials(&gh);
        }
        return current;
    }

    let mut runtime_guard = match auth_runtime().lock() {
        Ok(guard) => guard,
        Err(_) => {
            current.message = Some("GitHub connection state is unavailable.".to_string());
            return current;
        }
    };
    let Some(runtime) = runtime_guard.as_mut() else {
        return current;
    };

    for line in runtime.output_rx.try_iter() {
        if runtime.device_code.is_none() {
            runtime.device_code = extract_device_code(&line);
        }
    }

    if runtime.started.elapsed() >= LOGIN_TIMEOUT {
        let _ = runtime.child.kill();
        let _ = runtime.child.wait();
        *runtime_guard = None;
        current.message =
            Some("GitHub connection timed out. Click GitHub to try again.".to_string());
        return current;
    }

    match runtime.child.try_wait() {
        Ok(Some(exit)) => {
            let device_code = runtime.device_code.clone();
            *runtime_guard = None;
            drop(runtime_guard);
            let mut final_status = raw_status(&gh);
            if final_status.connected {
                final_status.message = setup_git_credentials(&gh);
                return final_status;
            }
            final_status.device_code = device_code;
            final_status.message = Some(if exit.success() {
                "GitHub sign-in finished but the connection could not be verified.".to_string()
            } else {
                "GitHub connection was not completed. Click GitHub to try again.".to_string()
            });
            final_status
        }
        Ok(None) => {
            current.connecting = true;
            current.device_code = runtime.device_code.clone();
            current.verification_url = Some(DEVICE_VERIFICATION_URL.to_string());
            current
        }
        Err(error) => {
            let _ = runtime.child.kill();
            let _ = runtime.child.wait();
            *runtime_guard = None;
            current.message = Some(format!("Could not monitor GitHub sign-in: {error}"));
            current
        }
    }
}

pub(crate) fn connect() -> Result<GithubConnectionStatus, String> {
    let Some(gh) = github_cli_path() else {
        return Err(
            "GitHub CLI is required for the GitHub connection and was not found on this computer."
                .to_string(),
        );
    };

    let current = status();
    if current.connected || current.connecting {
        return Ok(current);
    }

    let mut child = Command::new(&gh)
        .args([
            "auth",
            "login",
            "--hostname",
            GITHUB_HOST,
            "--git-protocol",
            "https",
            "--web",
            "--scopes",
            "repo,workflow,read:org,gist",
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not start GitHub sign-in: {error}"))?;

    if let Err(error) = launcher::open_default_url_now(DEVICE_VERIFICATION_URL) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "Could not open GitHub sign-in in your default browser: {error}"
        ));
    }

    if let Some(mut stdin) = child.stdin.take() {
        let _ = thread::Builder::new()
            .name("repotunnel-github-auth-input".to_string())
            .spawn(move || {
                // gh's browser device flow asks for Enter before opening the browser on
                // older releases. A piped newline handles that prompt without exposing
                // a terminal, while the one-time code is surfaced in RepoTunnel's UI.
                let _ = stdin.write_all(b"\n");
                let _ = stdin.flush();
            });
    }

    let (output_tx, output_rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        spawn_output_reader(stdout, output_tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_output_reader(stderr, output_tx);
    }

    let runtime = AuthRuntime {
        child,
        output_rx,
        device_code: None,
        started: Instant::now(),
    };
    let mut runtime_guard = auth_runtime()
        .lock()
        .map_err(|_| "GitHub connection state is unavailable.".to_string())?;
    *runtime_guard = Some(runtime);
    drop(runtime_guard);

    // Give gh a brief moment to emit the device code so the first UI response often
    // contains it; later status polls fill it in if startup takes longer.
    thread::sleep(Duration::from_millis(120));
    Ok(status())
}

pub(crate) fn cancel_connection() -> Result<GithubConnectionStatus, String> {
    stop_runtime();
    Ok(status())
}

pub(crate) fn disconnect() -> Result<GithubConnectionStatus, String> {
    stop_runtime();
    let Some(gh) = github_cli_path() else {
        return Err("GitHub CLI is not installed on this computer.".to_string());
    };

    let current = raw_status(&gh);
    if !current.connected {
        return Ok(current);
    }

    let mut args = vec!["auth", "logout", "--hostname", GITHUB_HOST];
    if let Some(ref username) = current.username {
        args.extend(["--user", username.as_str()]);
    }
    let output = run_gh(&gh, &args)?;
    if !output.status.success() {
        return Err("Could not disconnect GitHub. Please try again.".to_string());
    }

    Ok(raw_status(&gh))
}

#[cfg(test)]
mod tests {
    use super::{executable_name, extract_device_code};

    #[test]
    fn github_cli_name_matches_platform() {
        if cfg!(windows) {
            assert_eq!(executable_name(), "gh.exe");
        } else {
            assert_eq!(executable_name(), "gh");
        }
    }

    #[test]
    fn extracts_github_device_code_without_exposing_other_output() {
        assert_eq!(
            extract_device_code("! First copy your one-time code: A1B2-C3D4"),
            Some("A1B2-C3D4".to_string())
        );
        assert_eq!(extract_device_code("Press Enter to open GitHub"), None);
        assert_eq!(extract_device_code("token abcdef1234567890"), None);
    }
}
