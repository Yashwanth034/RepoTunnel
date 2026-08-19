use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::Serialize;
use tauri::AppHandle;
use url::Url;

use crate::{commands::register_workspace_path, models::Workspace, storage::load_workspaces};

const GITHUB_HOST: &str = "github.com";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepositoryCloneResult {
    pub(crate) workspace: Workspace,
    pub(crate) repository: String,
    pub(crate) reused_existing_checkout: bool,
}

#[derive(Clone, Debug)]
struct GithubRepository {
    owner: String,
    name: String,
    clone_url: String,
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn parse_repository(input: &str) -> Result<GithubRepository, String> {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Err("Repository URL cannot be empty.".to_string());
    }

    let (owner, repo) = if raw.starts_with("https://") {
        let parsed = Url::parse(raw)
            .map_err(|_| "Provide a valid GitHub HTTPS repository URL.".to_string())?;
        if parsed.scheme() != "https" || parsed.host_str() != Some(GITHUB_HOST) {
            return Err("RepoTunnel automatic cloning currently accepts GitHub HTTPS repository links only.".to_string());
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(
                "GitHub repository links cannot include query strings or fragments.".to_string(),
            );
        }
        let segments = parsed
            .path_segments()
            .map(|segments| segments.filter(|part| !part.is_empty()).collect::<Vec<_>>())
            .unwrap_or_default();
        if segments.len() != 2 {
            return Err(
                "Use a repository link such as https://github.com/owner/repository.".to_string(),
            );
        }
        (
            segments[0].to_string(),
            segments[1].trim_end_matches(".git").to_string(),
        )
    } else {
        let pieces = raw.split('/').collect::<Vec<_>>();
        if pieces.len() != 2 {
            return Err(
                "Use either owner/repository or a full https://github.com/owner/repository link."
                    .to_string(),
            );
        }
        (
            pieces[0].to_string(),
            pieces[1].trim_end_matches(".git").to_string(),
        )
    };

    if !valid_segment(&owner) || !valid_segment(&repo) {
        return Err(
            "The GitHub owner or repository name contains unsupported characters.".to_string(),
        );
    }

    Ok(GithubRepository {
        clone_url: format!("https://github.com/{owner}/{repo}.git"),
        owner,
        name: repo,
    })
}

fn projects_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        "RepoTunnel could not resolve your home directory for automatic cloning.".to_string()
    })?;
    Ok(home.join("Projects"))
}

fn normalize_remote(value: &str) -> String {
    value
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase()
}

fn checkout_matches(path: &Path, repository: &GithubRepository) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "origin"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let remote = String::from_utf8_lossy(&output.stdout);
            normalize_remote(&remote) == normalize_remote(&repository.clone_url)
        }
        _ => false,
    }
}

fn clone_with_git(repository: &GithubRepository, destination: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .arg("clone")
        .arg("--origin")
        .arg("origin")
        .arg("--")
        .arg(&repository.clone_url)
        .arg(destination)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("Could not start Git to clone the repository: {error}"))?;

    if output.status.success() {
        return Ok(());
    }

    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if detail.is_empty() {
        "Git clone failed. Check network access and your existing Git/GitHub authentication."
            .to_string()
    } else {
        detail.chars().take(1_200).collect()
    };
    Err(format!("Could not clone the GitHub repository. {detail}"))
}

pub(crate) fn clone_and_register(
    app: &AppHandle,
    input: &str,
) -> Result<RepositoryCloneResult, String> {
    let repository = parse_repository(input)?;
    let root = projects_root()?;
    fs::create_dir_all(&root)
        .map_err(|error| format!("Could not create the local Projects folder: {error}"))?;
    let destination = root.join(&repository.name);

    let canonical_destination = destination.canonicalize().ok();
    if let Some(existing) = load_workspaces(app)?.into_iter().find(|workspace| {
        canonical_destination
            .as_ref()
            .and_then(|path| {
                Path::new(&workspace.path)
                    .canonicalize()
                    .ok()
                    .map(|workspace_path| workspace_path == path.as_path())
            })
            .unwrap_or(false)
    }) {
        if checkout_matches(&destination, &repository) {
            return Ok(RepositoryCloneResult {
                workspace: existing,
                repository: format!("{}/{}", repository.owner, repository.name),
                reused_existing_checkout: true,
            });
        }
        return Err(format!(
            "A different approved project already uses ~/Projects/{}. Choose a different local folder manually before cloning this repository.",
            repository.name
        ));
    }

    let reused_existing_checkout = if destination.exists() {
        if !destination.is_dir() || !checkout_matches(&destination, &repository) {
            return Err(format!(
                "~/Projects/{} already exists and is not the requested GitHub checkout. RepoTunnel will not overwrite it.",
                repository.name
            ));
        }
        true
    } else {
        clone_with_git(&repository, &destination)?;
        false
    };

    let workspace = register_workspace_path(app, destination.to_string_lossy().into_owned())?;
    Ok(RepositoryCloneResult {
        workspace,
        repository: format!("{}/{}", repository.owner, repository.name),
        reused_existing_checkout,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_repository;

    #[test]
    fn accepts_github_url_and_shorthand() {
        let url = parse_repository("https://github.com/sample-owner/sample-repo").unwrap();
        assert_eq!(url.name, "sample-repo");
        let short = parse_repository("sample-owner/sample-repo").unwrap();
        assert_eq!(short.clone_url, url.clone_url);
    }

    #[test]
    fn rejects_non_github_hosts() {
        assert!(parse_repository("https://example.com/owner/repo").is_err());
    }
}
