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
use serde_json::{json, Value};

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
static STATUS_CACHE: OnceLock<Mutex<Option<GithubConnectionStatus>>> = OnceLock::new();

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

fn status_cache() -> &'static Mutex<Option<GithubConnectionStatus>> {
    STATUS_CACHE.get_or_init(|| Mutex::new(None))
}

fn cache_status(status: &GithubConnectionStatus) {
    if let Ok(mut cached) = status_cache().lock() {
        *cached = Some(status.clone());
    }
}

pub(crate) fn cached_status() -> Option<GithubConnectionStatus> {
    status_cache().lock().ok()?.clone()
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

fn github_cli_config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("GitHub CLI"));
    }

    #[cfg(not(windows))]
    {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|root| root.join(".config").join("gh"))
    }
}

pub(crate) fn configure_cli_command(command: &mut Command) {
    // RepoTunnel desktop/AI Workspace processes may carry an isolated
    // XDG_CONFIG_HOME. GH_CONFIG_DIR has higher priority and pins GitHub CLI to
    // the human's normal persistent GitHub CLI profile without copying a token
    // into the AI sandbox or AI Workspace environment.
    if let Some(config_dir) = github_cli_config_dir() {
        command.env("GH_CONFIG_DIR", config_dir);
    }
}

fn run_gh(gh: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let mut command = Command::new(gh);
    configure_cli_command(&mut command);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Could not run GitHub CLI: {error}"))
}

fn run_gh_in_dir(gh: &Path, cwd: &Path, args: &[String]) -> Result<std::process::Output, String> {
    let mut command = Command::new(gh);
    configure_cli_command(&mut command);
    command
        .current_dir(cwd)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("Could not run authenticated GitHub CLI command: {error}"))
}

fn authenticated_cli() -> Result<PathBuf, String> {
    let gh = github_cli_path()
        .ok_or_else(|| "GitHub CLI is not installed on this computer.".to_string())?;
    let status = raw_status(&gh);
    if !status.connected {
        return Err(status.message.unwrap_or_else(|| {
            "GitHub is not connected in RepoTunnel. Connect GitHub and try again.".to_string()
        }));
    }
    Ok(gh)
}

pub(crate) fn create_pull_request(
    cwd: &Path,
    title: String,
    body: Option<String>,
    base: Option<String>,
    head: Option<String>,
    draft: bool,
) -> Result<Value, String> {
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("Pull request title cannot be empty.".to_string());
    }
    if title.len() > 512 {
        return Err("Pull request title is too large.".to_string());
    }
    let body = body.unwrap_or_default();
    if body.len() > 64 * 1024 {
        return Err("Pull request body is too large.".to_string());
    }

    let gh = authenticated_cli()?;
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--title".to_string(),
        title.clone(),
        "--body".to_string(),
        body,
    ];
    if let Some(base) = base
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        args.extend(["--base".to_string(), base]);
    }
    if let Some(head) = head
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        args.extend(["--head".to_string(), head]);
    }
    if draft {
        args.push("--draft".to_string());
    }
    let output = run_gh_in_dir(&gh, cwd, &args)?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "GitHub pull request creation failed.".to_string()
        } else {
            format!("GitHub pull request creation failed: {detail}")
        });
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(json!({
        "created": true,
        "title": title,
        "url": url,
        "draft": draft
    }))
}

pub(crate) fn merge_pull_request(
    cwd: &Path,
    number: u64,
    method: &str,
    delete_branch: bool,
    auto: bool,
    admin: bool,
) -> Result<Value, String> {
    if number == 0 {
        return Err("Pull request number must be greater than zero.".to_string());
    }
    let method_flag = match method {
        "merge" => "--merge",
        "squash" => "--squash",
        "rebase" => "--rebase",
        _ => return Err("Merge method must be merge, squash, or rebase.".to_string()),
    };
    let gh = authenticated_cli()?;
    let mut args = vec![
        "pr".to_string(),
        "merge".to_string(),
        number.to_string(),
        method_flag.to_string(),
    ];
    if delete_branch {
        args.push("--delete-branch".to_string());
    }
    if auto {
        args.push("--auto".to_string());
    }
    if admin {
        args.push("--admin".to_string());
    }
    let output = run_gh_in_dir(&gh, cwd, &args)?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            "GitHub pull request merge failed.".to_string()
        } else {
            format!("GitHub pull request merge failed: {detail}")
        });
    }
    Ok(json!({
        "merged": true,
        "number": number,
        "method": method,
        "deleteBranch": delete_branch,
        "auto": auto,
        "admin": admin,
        "output": String::from_utf8_lossy(&output.stdout).trim()
    }))
}

fn auth_status_username(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );

    for line in text.lines() {
        let words = line.split_whitespace().collect::<Vec<_>>();
        for (index, word) in words.iter().enumerate() {
            if !word.eq_ignore_ascii_case("account") {
                continue;
            }
            let Some(candidate) = words.get(index + 1) else {
                continue;
            };
            let username = candidate
                .trim_matches(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-'))
                .to_string();
            if !username.is_empty()
                && username.len() <= 39
                && username
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Some(username);
            }
        }
    }

    None
}

fn auth_output_reports_invalid_credential(stdout: &[u8], stderr: &[u8]) -> bool {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    )
    .to_ascii_lowercase();
    text.contains("token") && (text.contains("invalid") || text.contains("expired"))
}

fn connected_status(username: Option<String>) -> GithubConnectionStatus {
    GithubConnectionStatus {
        available: true,
        connected: true,
        connecting: false,
        username,
        device_code: None,
        verification_url: None,
        message: None,
    }
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

    let invalid_credential = auth_output_reports_invalid_credential(&auth.stdout, &auth.stderr);
    if !auth.status.success() || invalid_credential {
        return GithubConnectionStatus {
            available: true,
            connected: false,
            connecting: false,
            username: None,
            device_code: None,
            verification_url: None,
            message: invalid_credential.then(|| {
                "Saved GitHub credential is invalid or expired. Reconnect GitHub once to refresh it."
                    .to_string()
            }),
        };
    }

    // "gh auth status" is the authoritative persisted-auth check. Derive the
    // display username from the same CLI result instead of making a second
    // network request on every Commands-page mount.
    connected_status(auth_status_username(&auth.stdout, &auth.stderr))
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

fn compute_status() -> GithubConnectionStatus {
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

pub(crate) fn status() -> GithubConnectionStatus {
    let current = compute_status();
    cache_status(&current);
    current
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

    let mut command = Command::new(&gh);
    configure_cli_command(&mut command);
    let mut child = command
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
        cache_status(&current);
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

    let disconnected = raw_status(&gh);
    cache_status(&disconnected);
    Ok(disconnected)
}

#[cfg(test)]
mod tests {
    use super::{
        auth_output_reports_invalid_credential, auth_status_username, connected_status,
        executable_name, extract_device_code, github_cli_config_dir,
    };

    #[test]
    fn parses_username_from_github_auth_status_without_api_round_trip() {
        assert_eq!(
            auth_status_username(
                b"",
                b"github.com\n  Logged in to github.com account Yashwanth034 (keyring)\n"
            ),
            Some("Yashwanth034".to_string())
        );
        assert_eq!(auth_status_username(b"Logged in to github.com", b""), None);
    }

    #[test]
    fn authenticated_status_stays_connected_when_username_lookup_is_unavailable() {
        let status = connected_status(None);
        assert!(status.connected);
        assert_eq!(status.username, None);
        assert!(!status.connecting);
    }

    #[test]
    fn github_cli_name_matches_platform() {
        if cfg!(windows) {
            assert_eq!(executable_name(), "gh.exe");
        } else {
            assert_eq!(executable_name(), "gh");
        }
    }

    #[test]
    fn github_cli_config_ignores_isolated_xdg_profiles() {
        let Some(config_dir) = github_cli_config_dir() else {
            return;
        };

        #[cfg(not(windows))]
        assert!(config_dir.ends_with(".config/gh"));

        #[cfg(windows)]
        assert!(config_dir.ends_with("GitHub CLI"));

        let mut command = std::process::Command::new(executable_name());
        super::configure_cli_command(&mut command);
        let debug = format!("{command:?}");
        assert!(debug.contains("GH_CONFIG_DIR"));
        assert!(debug.contains(config_dir.to_string_lossy().as_ref()));
    }

    #[test]
    fn detects_invalid_github_credentials_even_when_cli_exit_status_is_unreliable() {
        assert!(auth_output_reports_invalid_credential(
            b"",
            b"The token in /home/example/.config/gh/hosts.yml is invalid."
        ));
        assert!(!auth_output_reports_invalid_credential(
            b"Logged in to github.com",
            b""
        ));
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
