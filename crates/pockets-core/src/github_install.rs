//! Safe GitHub repository pull (`pockets add <owner/repo>`) for standalone Pockets.
//!
//! Clones a public `https://github.com/<owner>/<repo>` repository with
//! `--depth=1` into Pockets' local repos directory, validates its `pocket.yaml`,
//! and registers it as a linked draft without executing any code or building
//! any container until the user explicitly chooses Preview or Build & start.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::development;
use crate::projects::Project;
use crate::AppContext;

const CLONE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepo {
    pub owner: String,
    pub repo: String,
}

impl GitHubRepo {
    pub fn clone_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.repo)
    }

    pub fn display_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('-')
        && !s.starts_with('.')
        && !s.ends_with('.')
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Parses either `owner/repo` shorthand or `https://github.com/owner/repo[.git]`.
/// Rejects non-HTTPS schemes, non-GitHub hosts, flag-like inputs, and traversal.
pub fn parse_github_repo(raw: &str) -> Result<GitHubRepo, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("Enter a GitHub repository (for example, runpockets/crypto-tracker).".into());
    }
    if trimmed.starts_with('-') {
        return Err("Invalid GitHub repository reference.".into());
    }

    let path = if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        rest
    } else if trimmed.contains("://") || trimmed.starts_with("git@") {
        return Err(
            "Only public https://github.com/<owner>/<repo> repositories are supported.".into(),
        );
    } else {
        trimmed
    };

    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or("");
    let raw_repo = parts.next().unwrap_or("");
    if parts.next().is_some() {
        return Err(
            "Use owner/repo or https://github.com/owner/repo without extra path segments.".into(),
        );
    }

    let repo = raw_repo.strip_suffix(".git").unwrap_or(raw_repo);
    if !valid_segment(owner) || !valid_segment(repo) {
        return Err(
            "Enter a valid GitHub repository like owner/repo or https://github.com/owner/repo."
                .into(),
        );
    }

    Ok(GitHubRepo {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    })
}

fn find_git() -> Result<PathBuf, String> {
    let candidates: &[&str] = if cfg!(windows) {
        &["git.exe"]
    } else {
        &["/usr/bin/git", "/opt/homebrew/bin/git", "/usr/local/bin/git", "git"]
    };
    for candidate in candidates {
        let path = Path::new(candidate);
        if path.is_absolute() && path.is_file() {
            return Ok(path.to_path_buf());
        }
    }
    if let Some(path_env) = std::env::var_os("PATH") {
        let exe = if cfg!(windows) { "git.exe" } else { "git" };
        for dir in std::env::split_paths(&path_env) {
            let full = dir.join(exe);
            if full.is_file() {
                return Ok(full);
            }
        }
    }
    Err("Git was not found on this computer. Install Git to pull Pockets from GitHub.".into())
}

async fn run_git_clone(git: &Path, url: &str, dest: &Path) -> Result<(), String> {
    let dest_str = dest
        .to_str()
        .ok_or("Destination folder path must be valid UTF-8.")?;
    let mut cmd = Command::new(git);
    cmd.args(["clone", "--depth=1", "--single-branch", "--", url, dest_str])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|err| format!("Could not start git clone: {err}"))?;
    let output = tokio::time::timeout(CLONE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "Cloning {url} timed out after {} seconds.",
                CLONE_TIMEOUT.as_secs()
            )
        })?
        .map_err(|err| format!("Could not finish git clone: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("git clone failed")
            .trim();
        return Err(format!("Could not clone {url}: {first_line}"));
    }
    Ok(())
}

/// Clones a public GitHub Pocket repository into `<data_dir>/repos/<owner>__<repo>`
/// (or a custom destination parent when specified), validates its `pocket.yaml`,
/// and registers it in Pockets without executing any code.
pub async fn add_github_pocket(
    ctx: &AppContext,
    source: &str,
    dest_parent: Option<PathBuf>,
) -> Result<Project, String> {
    let repo = parse_github_repo(source)?;
    let git = find_git()?;

    let parent = match dest_parent {
        Some(dir) => dir,
        None => ctx.data_dir().join("repos"),
    };
    let folder_name = format!("{}__{}", repo.owner.to_lowercase(), repo.repo.to_lowercase());
    let dest = parent.join(&folder_name);

    // If this exact repository was already cloned earlier, relink it cleanly.
    if dest.is_dir() && dest.join("pocket.yaml").is_file() {
        let store = ctx.store();
        let canonical = std::fs::canonicalize(&dest).unwrap_or(dest);
        return tokio::task::spawn_blocking(move || development::link(&store, None, &canonical))
            .await
            .map_err(|_| "Registering the GitHub Pocket was interrupted.".to_owned())?;
    }

    if dest.exists() {
        return Err(format!(
            "Destination '{}' already exists and does not contain a pocket.yaml.",
            dest.display()
        ));
    }

    std::fs::create_dir_all(&parent)
        .map_err(|err| format!("Could not create '{}': {err}", parent.display()))?;

    let clone_url = repo.clone_url();
    if let Err(err) = run_git_clone(&git, &clone_url, &dest).await {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(err);
    }

    let _building = ctx
        .build_jobs
        .try_claim()
        .ok_or("Wait for the current Build/Update to finish before adding a Pocket.")?;
    let _lifecycle = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    let link_dest = dest.clone();
    let linked = tokio::task::spawn_blocking(move || development::link(&store, None, &link_dest))
        .await
        .map_err(|_| "Registering the GitHub Pocket was interrupted.".to_owned())?;

    match linked {
        Ok(project) => Ok(project),
        Err(err) => {
            let _ = std::fs::remove_dir_all(&dest);
            Err(format!(
                "Cloned {} but it is not a valid Pocket: {err}",
                repo.display_slug()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shorthand_and_https_github_urls() {
        assert_eq!(
            parse_github_repo("runpockets/crypto-tracker").unwrap(),
            GitHubRepo {
                owner: "runpockets".into(),
                repo: "crypto-tracker".into(),
            }
        );
        assert_eq!(
            parse_github_repo("https://github.com/runpockets/crypto-tracker/").unwrap(),
            GitHubRepo {
                owner: "runpockets".into(),
                repo: "crypto-tracker".into(),
            }
        );
        assert_eq!(
            parse_github_repo("https://github.com/runpockets/crypto-tracker.git").unwrap(),
            GitHubRepo {
                owner: "runpockets".into(),
                repo: "crypto-tracker".into(),
            }
        );
    }

    #[test]
    fn rejects_non_github_schemes_flags_and_traversal() {
        for bad in [
            "",
            "-u=evil/repo",
            "file:///etc/passwd",
            "ssh://github.com/runpockets/pockets",
            "git@github.com:runpockets/pockets.git",
            "http://github.com/runpockets/pockets",
            "https://evil.example/runpockets/pockets",
            "runpockets/../other",
            "runpockets/repo/extra",
            "runpockets/",
        ] {
            assert!(parse_github_repo(bad).is_err(), "expected error for {bad:?}");
        }
    }
}
