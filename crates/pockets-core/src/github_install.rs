//! Safe GitHub repository inspection and pull (`pockets add <owner/repo>`) for
//! standalone Pockets.
//!
//! Pre-flight inspection fetches `pocket.yaml` and verifies `Containerfile`
//! from `https://raw.githubusercontent.com/<owner>/<repo>/HEAD/...`, parsing
//! the manifest through Pockets' canonical [`crate::manifest::parse_manifest`]
//! validator before cloning anything.
//!
//! Pulling clones a public `https://github.com/<owner>/<repo>` repository with
//! `--depth=1` into Pockets' local repos directory, re-validates the full tree
//! through [`crate::development::link`], and registers it as a linked draft
//! without executing any code or building any container until the user
//! explicitly chooses Preview or Build & start.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::development;
use crate::manifest::{self, Manifest};
use crate::projects::Project;
use crate::{AppContext, APP_VERSION};

const CLONE_TIMEOUT: Duration = Duration::from_secs(60);
const INSPECT_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_ROOT_FILE_BYTES: usize = 128 * 1024;
const RAW_GITHUB_BASE: &str = "https://raw.githubusercontent.com";
const GITHUB_API_BASE: &str = "https://api.github.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepo {
    pub owner: String,
    pub repo: String,
}

impl GitHubRepo {
    pub fn clone_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.repo)
    }

    pub fn html_url(&self) -> String {
        format!("https://github.com/{}/{}", self.owner, self.repo)
    }

    pub fn display_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    pub fn repo_folder_name(&self) -> String {
        format!(
            "{}__{}",
            self.owner.to_lowercase(),
            self.repo.to_lowercase()
        )
    }
}

/// Verified pre-flight metadata for a public GitHub Pocket repository before
/// cloning it onto the machine.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPocketPreview {
    pub owner: String,
    pub repo: String,
    pub slug: String,
    pub repo_url: String,
    pub manifest: Manifest,
    pub host_supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_project_id: Option<String>,
}

#[derive(Deserialize)]
struct GitHubRepoMeta {
    default_branch: Option<String>,
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

/// Parses either `owner/repo`, `github.com/owner/repo`, or
/// `https://github.com/owner/repo[.git]`.
/// Rejects non-HTTPS schemes, non-GitHub hosts, flag-like inputs, and traversal.
pub fn parse_github_repo(raw: &str) -> Result<GitHubRepo, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("Enter a GitHub repository (for example, runpockets/coinkiwi).".into());
    }
    if trimmed.starts_with('-') {
        return Err("Invalid GitHub repository reference.".into());
    }

    let path = if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("https://www.github.com/") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("github.com/") {
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

pub(crate) fn http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(format!("pockets/{APP_VERSION}"))
        .build()
        .map_err(|err| format!("Could not initialize HTTPS client: {err}"))
}

enum RawFileResult {
    Found(String),
    NotFound,
}

async fn fetch_bounded_raw_file(
    client: &reqwest::Client,
    url: &str,
    label: &str,
) -> Result<RawFileResult, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("Could not reach GitHub while checking {label}: {err}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(RawFileResult::NotFound);
    }
    if !response.status().is_success() {
        return Err(format!(
            "GitHub returned HTTP {} while checking {label}.",
            response.status().as_u16()
        ));
    }
    if let Some(len) = response.content_length() {
        if len as usize > MAX_ROOT_FILE_BYTES {
            return Err(format!(
                "{label} exceeds the 128 KiB maximum size allowed for a Pocket."
            ));
        }
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("Could not read {label} from GitHub: {err}"))?;
    if bytes.len() > MAX_ROOT_FILE_BYTES {
        return Err(format!(
            "{label} exceeds the 128 KiB maximum size allowed for a Pocket."
        ));
    }
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|_| format!("{label} must be valid UTF-8 text."))?;
    Ok(RawFileResult::Found(text))
}

async fn probe_github_repo_branch(
    client: &reqwest::Client,
    api_base: &str,
    repo: &GitHubRepo,
) -> Result<Option<String>, String> {
    let url = format!("{api_base}/repos/{}/{}", repo.owner, repo.repo);
    let response = match client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(_) => return Ok(None),
    };

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!(
            "Repository {} was not found on GitHub (or is private).",
            repo.display_slug()
        ));
    }
    if !response.status().is_success() {
        return Ok(None);
    }

    let meta = response
        .bytes()
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<GitHubRepoMeta>(&bytes).ok());
    Ok(meta.and_then(|m| m.default_branch))
}

/// Validates and parses the `pocket.yaml` + `Containerfile` of a remote GitHub
/// repository using raw GitHub URLs (avoiding API rate limits in the common case).
pub(crate) async fn fetch_validated_manifest_from_bases(
    client: &reqwest::Client,
    raw_base: &str,
    api_base: &str,
    repo: &GitHubRepo,
) -> Result<Manifest, String> {
    let mut git_ref = "HEAD".to_owned();
    let manifest_url = format!(
        "{raw_base}/{}/{}/{git_ref}/pocket.yaml",
        repo.owner, repo.repo
    );

    let yaml = match fetch_bounded_raw_file(client, &manifest_url, "pocket.yaml").await? {
        RawFileResult::Found(text) => text,
        RawFileResult::NotFound => {
            // Distinguish missing/private repo from a repo without pocket.yaml,
            // and retry with the explicit default branch if GitHub reports one.
            if let Some(branch) = probe_github_repo_branch(client, api_base, repo).await? {
                if branch != "HEAD" && valid_segment(&branch) {
                    git_ref = branch;
                    let retry_url = format!(
                        "{raw_base}/{}/{}/{git_ref}/pocket.yaml",
                        repo.owner, repo.repo
                    );
                    match fetch_bounded_raw_file(client, &retry_url, "pocket.yaml").await? {
                        RawFileResult::Found(text) => text,
                        RawFileResult::NotFound => {
                            return Err(format!(
                                "{} is not a valid Pocket repository: missing pocket.yaml at the repository root.",
                                repo.display_slug()
                            ));
                        }
                    }
                } else {
                    return Err(format!(
                        "{} is not a valid Pocket repository: missing pocket.yaml at the repository root.",
                        repo.display_slug()
                    ));
                }
            } else {
                return Err(format!(
                    "Could not find pocket.yaml in {}. Make sure the repository exists, is public, and has pocket.yaml at its root.",
                    repo.display_slug()
                ));
            }
        }
    };

    let manifest = manifest::parse_manifest(&yaml).map_err(|err| {
        format!(
            "{} has an invalid pocket.yaml: {err}",
            repo.display_slug()
        )
    })?;

    let containerfile_url = format!(
        "{raw_base}/{}/{}/{git_ref}/Containerfile",
        repo.owner, repo.repo
    );
    match fetch_bounded_raw_file(client, &containerfile_url, "Containerfile").await? {
        RawFileResult::Found(text) if !text.trim().is_empty() => Ok(manifest),
        RawFileResult::Found(_) => Err(format!(
            "{} is not a valid Pocket repository: Containerfile is empty.",
            repo.display_slug()
        )),
        RawFileResult::NotFound => Err(format!(
            "{} is not a valid Pocket repository: missing Containerfile at the repository root.",
            repo.display_slug()
        )),
    }
}

/// Fetches and validates a public GitHub Pocket's manifest using the default
/// GitHub endpoints. Shared by both repo inspection and the Pockets Hub catalog.
pub(crate) async fn fetch_validated_manifest(
    client: &reqwest::Client,
    repo: &GitHubRepo,
) -> Result<Manifest, String> {
    fetch_validated_manifest_from_bases(client, RAW_GITHUB_BASE, GITHUB_API_BASE, repo).await
}

fn normalized_slug(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Checks if a GitHub Pocket is already present in Pockets — either cloned into
/// `<data_dir>/repos/<owner>__<repo>` or already linked/imported locally under
/// the same manifest ID, repo slug, or Pocket display name.
pub(crate) fn find_existing_project_for_repo(
    ctx: &AppContext,
    repo: &GitHubRepo,
    manifest_id_hint: Option<&str>,
    name_hint: Option<&str>,
) -> Option<String> {
    let dest = ctx.data_dir().join("repos").join(repo.repo_folder_name());
    let expected = std::fs::canonicalize(&dest).unwrap_or(dest);
    let store = ctx.store();
    let projects = store.list().ok()?;

    for project in &projects {
        if let Ok(Some(folder)) = development::linked_folder(&store, &project.id) {
            if folder == expected {
                return Some(project.id.clone());
            }
        }
    }

    let mut candidate_ids = Vec::new();
    if let Some(id) = manifest_id_hint.map(str::trim).filter(|s| !s.is_empty()) {
        candidate_ids.push(normalized_slug(id));
    }
    let repo_slug = normalized_slug(&repo.repo);
    if !repo_slug.is_empty() && !candidate_ids.contains(&repo_slug) {
        candidate_ids.push(repo_slug);
    }
    let candidate_name = name_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalized_slug);

    for project in &projects {
        let proj_id_norm = normalized_slug(&project.manifest.metadata.id);
        if !proj_id_norm.is_empty() && candidate_ids.iter().any(|c| c == &proj_id_norm) {
            return Some(project.id.clone());
        }
        if let Some(ref want_name) = candidate_name {
            let proj_name_norm = normalized_slug(&project.manifest.metadata.name);
            if !proj_name_norm.is_empty() && proj_name_norm == *want_name {
                return Some(project.id.clone());
            }
        }
    }

    None
}

/// Inspects a public GitHub repository without cloning it, validating its
/// `pocket.yaml` and `Containerfile` and checking host architecture support.
pub async fn inspect_github_pocket(
    ctx: &AppContext,
    source: &str,
) -> Result<GitHubPocketPreview, String> {
    let repo = parse_github_repo(source)?;
    let client = http_client(INSPECT_TIMEOUT)?;
    let manifest = fetch_validated_manifest(&client, &repo).await?;
    let host_error = manifest.validate_host().err();
    let host_supported = host_error.is_none();

    let ctx_clone = ctx.clone();
    let repo_clone = repo.clone();
    let manifest_id = manifest.metadata.id.clone();
    let manifest_name = manifest.metadata.name.clone();
    let existing_project_id = tokio::task::spawn_blocking(move || {
        find_existing_project_for_repo(
            &ctx_clone,
            &repo_clone,
            Some(&manifest_id),
            Some(&manifest_name),
        )
    })
    .await
    .ok()
    .flatten();

    Ok(GitHubPocketPreview {
        owner: repo.owner.clone(),
        repo: repo.repo.clone(),
        slug: repo.display_slug(),
        repo_url: repo.html_url(),
        manifest,
        host_supported,
        host_error,
        existing_project_id,
    })
}

fn find_git() -> Result<PathBuf, String> {
    let candidates: &[&str] = if cfg!(windows) {
        &["git.exe"]
    } else {
        &[
            "/usr/bin/git",
            "/opt/homebrew/bin/git",
            "/usr/local/bin/git",
            "git",
        ]
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
    let folder_name = repo.repo_folder_name();
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
            crate::fs_browser::abbreviate_home_dir(&dest)
        ));
    }

    std::fs::create_dir_all(&parent).map_err(|err| {
        format!(
            "Could not create '{}': {err}",
            crate::fs_browser::abbreviate_home_dir(&parent)
        )
    })?;

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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_http(routes: Vec<(&'static str, u16, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (status, body) = routes
                        .iter()
                        .find(|(route, _, _)| *route == path)
                        .map(|(_, s, b)| (*s, *b))
                        .unwrap_or((404, "Not Found"));
                    let reason = match status {
                        200 => "OK",
                        404 => "Not Found",
                        _ => "Error",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    const VALID_MANIFEST: &str = "\
apiVersion: pockets.dev/v1alpha1
kind: Pocket
metadata:
  id: coinkiwi
  name: CoinKiwi
  version: 0.2.0
  description: Track crypto portfolios and NZD prices locally.
runtime:
  image: localhost/pockets-coinkiwi:0.2.0
  architectures: [arm64, amd64]
web:
  port: 8080
  healthcheck: /health
storage:
  name: coinkiwi-data
  path: /data
";

    #[test]
    fn parses_shorthand_and_https_github_urls() {
        assert_eq!(
            parse_github_repo("runpockets/coinkiwi").unwrap(),
            GitHubRepo {
                owner: "runpockets".into(),
                repo: "coinkiwi".into(),
            }
        );
        assert_eq!(
            parse_github_repo("github.com/runpockets/coinkiwi").unwrap(),
            GitHubRepo {
                owner: "runpockets".into(),
                repo: "coinkiwi".into(),
            }
        );
        assert_eq!(
            parse_github_repo("https://github.com/runpockets/coinkiwi/").unwrap(),
            GitHubRepo {
                owner: "runpockets".into(),
                repo: "coinkiwi".into(),
            }
        );
        assert_eq!(
            parse_github_repo("https://github.com/runpockets/coinkiwi.git").unwrap(),
            GitHubRepo {
                owner: "runpockets".into(),
                repo: "coinkiwi".into(),
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
            assert!(
                parse_github_repo(bad).is_err(),
                "expected error for {bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn validates_remote_pocket_manifest_and_containerfile() {
        let base = spawn_mock_http(vec![
            ("/runpockets/coinkiwi/HEAD/pocket.yaml", 200, VALID_MANIFEST),
            (
                "/runpockets/coinkiwi/HEAD/Containerfile",
                200,
                "FROM node:22-alpine\n",
            ),
        ])
        .await;

        let client = http_client(Duration::from_secs(5)).unwrap();
        let repo = parse_github_repo("runpockets/coinkiwi").unwrap();
        let manifest = fetch_validated_manifest_from_bases(&client, &base, &base, &repo)
            .await
            .expect("valid repo must pass");
        assert_eq!(manifest.metadata.id, "coinkiwi");
        assert_eq!(manifest.metadata.name, "CoinKiwi");
        assert_eq!(
            manifest.metadata.description,
            "Track crypto portfolios and NZD prices locally."
        );
    }

    #[tokio::test]
    async fn rejects_repo_missing_containerfile_or_manifest() {
        let base = spawn_mock_http(vec![
            (
                "/runpockets/no-container/HEAD/pocket.yaml",
                200,
                VALID_MANIFEST,
            ),
            (
                "/repos/runpockets/not-a-pocket",
                200,
                r#"{"default_branch":"main"}"#,
            ),
        ])
        .await;

        let client = http_client(Duration::from_secs(5)).unwrap();

        let no_cf = parse_github_repo("runpockets/no-container").unwrap();
        let err = fetch_validated_manifest_from_bases(&client, &base, &base, &no_cf)
            .await
            .expect_err("missing Containerfile must fail");
        assert!(err.contains("Containerfile"), "unexpected error: {err}");

        let not_pocket = parse_github_repo("runpockets/not-a-pocket").unwrap();
        let err = fetch_validated_manifest_from_bases(&client, &base, &base, &not_pocket)
            .await
            .expect_err("missing pocket.yaml must fail");
        assert!(err.contains("missing pocket.yaml"), "unexpected error: {err}");

        let missing = parse_github_repo("runpockets/does-not-exist").unwrap();
        let err = fetch_validated_manifest_from_bases(&client, &base, &base, &missing)
            .await
            .expect_err("404 repo must fail");
        assert!(
            err.contains("not found on GitHub"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn detects_existing_locally_developed_pocket_by_id_or_name() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = temp.path().join("app-data");
        let work_parent = temp.path().join("my-dev-projects");
        std::fs::create_dir_all(&work_parent).unwrap();
        let ctx = AppContext::new(data_dir);
        let store = ctx.store();

        let local = crate::scaffold::create(&store, &work_parent, "CoinKiwi", "coin-kiwi")
            .expect("scaffold local coinkiwi");
        let repo = parse_github_repo("runpockets/coinkiwi").unwrap();

        assert_eq!(
            find_existing_project_for_repo(&ctx, &repo, None, Some("CoinKiwi")).as_deref(),
            Some(local.id.as_str())
        );

        let other_repo = parse_github_repo("runpockets/unrelated-app").unwrap();
        assert_eq!(
            find_existing_project_for_repo(&ctx, &other_repo, None, Some("Unrelated")),
            None
        );
    }
}
