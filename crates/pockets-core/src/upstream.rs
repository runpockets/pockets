//! Upstream Git update checking and safe pulling for linked Pockets.
//!
//! Any linked Pocket whose folder is a Git repository with an `origin` remote
//! (including every Pocket pulled from Pockets Hub or `pockets add`) can check
//! for upstream updates and pull them safely:
//!
//! - **Unmodified clone** (`behind > 0`, `ahead == 0`, clean working tree):
//!   fast-forwards cleanly with `git merge --ff-only`, validates the incoming
//!   `pocket.yaml`, `Containerfile`, storage mount path, and full snapshot, and
//!   syncs Pockets' working source without touching the running container.
//! - **Customized / remixed clone** (`ahead > 0` or uncommitted local edits):
//!   never silently overwrites local work. Reports `UpstreamState::Diverged`
//!   so the user can either merge `origin/<branch>` in their coding tool or
//!   explicitly choose `PullMode::ReplaceWithUpstream` (which requires a clean
//!   working tree and saves a `pockets-backup-<sha>` branch before resetting).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::development;
use crate::git;
use crate::hub::parse_git_remote_github_repo;
use crate::manifest;
use crate::projects::Project;
use crate::source;
use crate::AppContext;

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const FETCH_COOLDOWN: Duration = Duration::from_secs(5 * 60);
const MAX_NOTES: usize = 10;
const MAX_NOTE_CHARS: usize = 200;
const MAX_ROOT_FILE_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpstreamState {
    /// Not linked to a local folder, or the folder is unavailable.
    Unlinked,
    /// Linked folder is not a git repo, has no `origin` remote, or no upstream ref.
    NoRemote,
    /// Remote has no commits newer than the local branch.
    UpToDate,
    /// Remote has new commits and the local repo can fast-forward cleanly.
    UpdateAvailable,
    /// Remote has new commits, and the user has uncommitted changes or local commits.
    Diverged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PullMode {
    /// Fast-forward only; refuses if dirty or if local commits exist.
    FastForward,
    /// Saves a `pockets-backup-<sha>` branch and resets to upstream; requires a
    /// clean working tree with no colliding untracked files.
    ReplaceWithUpstream,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamStatus {
    pub state: UpstreamState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_commit: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub local_dirty: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_error: Option<String>,
}

impl UpstreamStatus {
    fn empty(state: UpstreamState) -> Self {
        Self {
            state,
            repo: None,
            repo_url: None,
            branch: None,
            upstream_ref: None,
            local_commit: None,
            upstream_commit: None,
            ahead: 0,
            behind: 0,
            local_dirty: false,
            current_version: None,
            upstream_version: None,
            notes: Vec::new(),
            fetch_error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamPullResult {
    pub project: Project,
    pub previous_commit: String,
    pub current_commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_branch: Option<String>,
    pub updated: bool,
}

static FETCH_COOLDOWNS: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();

fn cooldowns() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    FETCH_COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn should_fetch(folder: &Path, force: bool) -> bool {
    if force {
        return true;
    }
    let guard = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    match guard.get(folder) {
        Some(at) => at.elapsed() >= FETCH_COOLDOWN,
        None => true,
    }
}

fn mark_fetched(folder: &Path) {
    let mut guard = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(folder.to_path_buf(), Instant::now());
}

async fn run_git_fetch(git_bin: &Path, folder: &Path) -> Result<(), String> {
    let mut cmd = Command::new(git_bin);
    cmd.arg("-C")
        .arg(folder)
        .args([
            "-c",
            "core.quotepath=off",
            "-c",
            "color.ui=false",
            "fetch",
            "--no-tags",
            "--quiet",
            "origin",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|err| format!("Could not start git fetch: {err}"))?;
    let output = tokio::time::timeout(FETCH_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "Checking upstream timed out after {} seconds.",
                FETCH_TIMEOUT.as_secs()
            )
        })?
        .map_err(|err| format!("Could not finish git fetch: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("Could not reach the remote repository.")
            .trim();
        return Err(first_line.to_owned());
    }
    Ok(())
}

fn is_git_work_tree(git_bin: &Path, folder: &Path) -> bool {
    git::run_text(git_bin, folder, &["rev-parse", "--is-inside-work-tree"])
        .is_ok_and(|out| out == "true")
}

fn resolve_upstream_ref(git_bin: &Path, folder: &Path, branch: Option<&str>) -> Option<String> {
    if let Ok(upstream) = git::run_text(
        git_bin,
        folder,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    ) {
        if !upstream.is_empty() {
            return Some(upstream);
        }
    }
    if let Some(branch) = branch {
        let candidate = format!("refs/remotes/origin/{branch}");
        if git::run(git_bin, folder, &["rev-parse", "--verify", "--quiet", &candidate]).is_ok() {
            return Some(format!("origin/{branch}"));
        }
    }
    if git::run(
        git_bin,
        folder,
        &["rev-parse", "--verify", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .is_ok()
    {
        return Some("origin/HEAD".to_owned());
    }
    None
}

fn is_ignored_untracked(path: &str) -> bool {
    let trimmed = path.trim_end_matches('/');
    trimmed == ".DS_Store"
        || trimmed.ends_with("/.DS_Store")
        || trimmed == "data"
        || trimmed.starts_with("data/")
        || trimmed == "node_modules"
        || trimmed.starts_with("node_modules/")
}

/// Returns true if the working tree has modified/staged tracked files or
/// meaningful untracked files (excluding OS junk like `.DS_Store` or local
/// `data/` / `node_modules/` directories).
fn working_tree_is_dirty(git_bin: &Path, folder: &Path) -> Result<bool, String> {
    let tracked = git::run(
        git_bin,
        folder,
        &[
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=no",
            "--",
            ".",
        ],
    )?;
    if !tracked.is_empty() {
        return Ok(true);
    }
    let untracked = git::run(
        git_bin,
        folder,
        &["ls-files", "--others", "--exclude-standard", "-z", "--", "."],
    )?;
    Ok(git::split_nul(&untracked)
        .into_iter()
        .any(|path| !is_ignored_untracked(&path)))
}

fn parse_ahead_behind(git_bin: &Path, folder: &Path, upstream_ref: &str) -> Result<(u32, u32), String> {
    let range = format!("HEAD...{upstream_ref}");
    let out = git::run_text(git_bin, folder, &["rev-list", "--left-right", "--count", &range])?;
    let mut parts = out.split_whitespace();
    let ahead = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| format!("Unexpected git ahead/behind output: {out:?}"))?;
    let behind = parts
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| format!("Unexpected git ahead/behind output: {out:?}"))?;
    Ok((ahead, behind))
}

fn git_show_text(git_bin: &Path, folder: &Path, spec: &str) -> Option<String> {
    let bytes = git::run(git_bin, folder, &["show", spec]).ok()?;
    if bytes.len() > MAX_ROOT_FILE_BYTES || bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn parse_bullet_notes(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let item = line
                .strip_prefix("- ")
                .or_else(|| line.strip_prefix("* "))?
                .trim();
            if item.is_empty() {
                return None;
            }
            Some(truncate_note(item))
        })
        .collect()
}

fn truncate_note(text: &str) -> String {
    if text.chars().count() > MAX_NOTE_CHARS {
        let cut: String = text.chars().take(MAX_NOTE_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        text.to_owned()
    }
}

fn incoming_notes(git_bin: &Path, folder: &Path, upstream_ref: &str) -> Vec<String> {
    // Prefer new bullet lines in POCKETS_CHANGES.md when the upstream repo maintains one.
    let changes_file = crate::versions::CHANGES_FILE;
    let base_ref = git::run_text(git_bin, folder, &["merge-base", "HEAD", upstream_ref])
        .unwrap_or_else(|_| "HEAD".to_owned());
    let before = git_show_text(git_bin, folder, &format!("{base_ref}:{changes_file}"));
    let after = git_show_text(git_bin, folder, &format!("{upstream_ref}:{changes_file}"));

    if let Some(after_text) = after {
        let mut seen: HashMap<String, usize> = HashMap::new();
        if let Some(before_text) = before {
            for note in parse_bullet_notes(&before_text) {
                *seen.entry(note).or_default() += 1;
            }
        }
        let mut added = Vec::new();
        for note in parse_bullet_notes(&after_text) {
            match seen.get_mut(&note) {
                Some(count) if *count > 0 => *count -= 1,
                _ => added.push(note),
            }
        }
        if !added.is_empty() {
            if added.len() > MAX_NOTES {
                added.drain(..added.len() - MAX_NOTES);
            }
            return added;
        }
    }

    // Fall back to incoming commit subjects (newest first, up to MAX_NOTES).
    let range = format!("HEAD..{upstream_ref}");
    let limit = format!("--max-count={MAX_NOTES}");
    let raw = git::run(
        git_bin,
        folder,
        &["log", "--no-merges", &limit, "--format=%s", &range],
    )
    .or_else(|_| git::run(git_bin, folder, &["log", &limit, "--format=%s", &range]))
    .unwrap_or_default();

    String::from_utf8_lossy(&raw)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(truncate_note)
        .collect()
}

fn inspect_local_upstream(
    git_bin: &Path,
    folder: &Path,
    project: &Project,
    fetch_error: Option<String>,
) -> Result<UpstreamStatus, String> {
    if !is_git_work_tree(git_bin, folder) {
        return Ok(UpstreamStatus::empty(UpstreamState::NoRemote));
    }
    let Ok(remote_url) = git::run_text(git_bin, folder, &["remote", "get-url", "origin"]) else {
        return Ok(UpstreamStatus::empty(UpstreamState::NoRemote));
    };
    if remote_url.is_empty() {
        return Ok(UpstreamStatus::empty(UpstreamState::NoRemote));
    }

    let gh = parse_git_remote_github_repo(&remote_url);
    let repo = gh.as_ref().map(|r| r.display_slug());
    let repo_url = gh.as_ref().map(|r| r.html_url());

    let branch = git::run_text(git_bin, folder, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty());
    let Some(upstream_ref) = resolve_upstream_ref(git_bin, folder, branch.as_deref()) else {
        return Ok(UpstreamStatus {
            state: UpstreamState::NoRemote,
            repo,
            repo_url,
            branch,
            fetch_error,
            ..UpstreamStatus::empty(UpstreamState::NoRemote)
        });
    };

    let local_commit = git::run_text(git_bin, folder, &["rev-parse", "--short", "HEAD"]).ok();
    let upstream_commit =
        git::run_text(git_bin, folder, &["rev-parse", "--short", &upstream_ref]).ok();
    let (ahead, behind) = parse_ahead_behind(git_bin, folder, &upstream_ref)?;
    let local_dirty = working_tree_is_dirty(git_bin, folder)?;

    let current_version = Some(project.manifest.metadata.version.clone());
    let upstream_version = git_show_text(git_bin, folder, &format!("{upstream_ref}:pocket.yaml"))
        .and_then(|yaml| manifest::parse_manifest(&yaml).ok())
        .map(|m| m.metadata.version);

    let state = if behind == 0 {
        UpstreamState::UpToDate
    } else if ahead == 0 && !local_dirty {
        UpstreamState::UpdateAvailable
    } else {
        UpstreamState::Diverged
    };

    let notes = if behind > 0 {
        incoming_notes(git_bin, folder, &upstream_ref)
    } else {
        Vec::new()
    };

    Ok(UpstreamStatus {
        state,
        repo,
        repo_url,
        branch,
        upstream_ref: Some(upstream_ref),
        local_commit,
        upstream_commit,
        ahead,
        behind,
        local_dirty,
        current_version,
        upstream_version,
        notes,
        fetch_error,
    })
}

/// Checks whether a linked Pocket's upstream Git remote has newer commits.
///
/// Uses a 5-minute per-folder cooldown for `git fetch` unless `force` is true,
/// while always inspecting local `HEAD`, dirty state, and ahead/behind counts
/// fresh so edits in the user's coding tool show up immediately.
pub async fn check_project_upstream(
    ctx: &AppContext,
    project_id: &str,
    force: bool,
) -> Result<UpstreamStatus, String> {
    let store = ctx.store();
    let pid = project_id.to_owned();
    let (project, folder, git_bin, has_origin) = tokio::task::spawn_blocking(move || {
        let project = store.load(&pid)?;
        let Some(folder) = development::linked_folder(&store, &project.id)? else {
            return Ok::<_, String>((project, None, None, false));
        };
        if !folder.is_dir() {
            return Ok((project, None, None, false));
        }
        let Some(git_bin) = git::find_git() else {
            return Ok((project, Some(folder), None, false));
        };
        let has_origin = is_git_work_tree(&git_bin, &folder)
            && git::run_text(&git_bin, &folder, &["remote", "get-url", "origin"])
                .is_ok_and(|u| !u.is_empty());
        Ok((project, Some(folder), Some(git_bin), has_origin))
    })
    .await
    .map_err(|_| "Checking upstream updates was interrupted.".to_owned())??;

    let Some(folder) = folder else {
        return Ok(UpstreamStatus::empty(UpstreamState::Unlinked));
    };
    let Some(git_bin) = git_bin else {
        return Ok(UpstreamStatus::empty(UpstreamState::NoRemote));
    };
    if !has_origin {
        return Ok(UpstreamStatus::empty(UpstreamState::NoRemote));
    }

    let fetch_error = if should_fetch(&folder, force) {
        match run_git_fetch(&git_bin, &folder).await {
            Ok(()) => {
                mark_fetched(&folder);
                None
            }
            Err(err) => Some(err),
        }
    } else {
        None
    };

    tokio::task::spawn_blocking(move || {
        inspect_local_upstream(&git_bin, &folder, &project, fetch_error)
    })
    .await
    .map_err(|_| "Reading upstream status was interrupted.".to_owned())?
}

fn validate_upstream_tree(
    git_bin: &Path,
    folder: &Path,
    project: &Project,
    upstream_ref: &str,
) -> Result<(), String> {
    let yaml = git_show_text(git_bin, folder, &format!("{upstream_ref}:pocket.yaml"))
        .ok_or("The upstream update is missing a valid UTF-8 pocket.yaml at the root.")?;
    let upstream_manifest = manifest::parse_manifest(&yaml)
        .map_err(|err| format!("The upstream pocket.yaml is invalid: {err}"))?;
    upstream_manifest.validate_host().map_err(|err| {
        format!("The upstream update does not support this computer's architecture: {err}")
    })?;
    source::enforce_storage_pinned(project, &upstream_manifest)?;

    let containerfile = git_show_text(git_bin, folder, &format!("{upstream_ref}:Containerfile"))
        .ok_or("The upstream update is missing Containerfile at the root.")?;
    if containerfile.trim().is_empty() {
        return Err("The upstream update has an empty Containerfile.".into());
    }
    Ok(())
}

fn allocate_backup_branch(git_bin: &Path, folder: &Path, short_sha: &str) -> Result<String, String> {
    let base = format!("pockets-backup-{short_sha}");
    if git::run(
        git_bin,
        folder,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{base}")],
    )
    .is_err()
    {
        return Ok(base);
    }
    for n in 2..=100 {
        let candidate = format!("{base}-{n}");
        if git::run(
            git_bin,
            folder,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ],
        )
        .is_err()
        {
            return Ok(candidate);
        }
    }
    Ok(format!(
        "{base}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ))
}

/// Pulls upstream commits into a linked Pocket's folder and syncs Pockets'
/// managed working snapshot. Never builds, restarts, or modifies the running
/// installed app until the user explicitly chooses Update.
pub async fn pull_project_upstream(
    ctx: &AppContext,
    project_id: &str,
    mode: PullMode,
) -> Result<UpstreamPullResult, String> {
    let _lifecycle = ctx.controller.lifecycle_guard().await;
    let _building = ctx
        .build_jobs
        .try_claim()
        .ok_or("Wait for the current Build/Update to finish before pulling upstream updates.")?;

    let store = ctx.store();
    let pid = project_id.to_owned();
    let (project, folder, git_bin) = tokio::task::spawn_blocking(move || {
        let project = store.load(&pid)?;
        let folder = development::folder(&store, &project.id)?;
        let git_bin = git::find_git()
            .ok_or("Git was not found on this computer. Install Git, then try again.")?;
        if !is_git_work_tree(&git_bin, &folder) {
            return Err("This Pocket's folder is not a Git repository.".into());
        }
        let remote_url = git::run_text(&git_bin, &folder, &["remote", "get-url", "origin"])
            .map_err(|_| "This Pocket does not have an 'origin' Git remote.".to_owned())?;
        if remote_url.is_empty() {
            return Err("This Pocket does not have an 'origin' Git remote.".into());
        }
        Ok::<_, String>((project, folder, git_bin))
    })
    .await
    .map_err(|_| "Preparing the upstream pull was interrupted.".to_owned())??;

    run_git_fetch(&git_bin, &folder).await?;
    mark_fetched(&folder);

    let store = ctx.store();
    tokio::task::spawn_blocking(move || {
        let branch = git::run_text(&git_bin, &folder, &["symbolic-ref", "--short", "-q", "HEAD"])
            .ok()
            .filter(|b| !b.is_empty());
        let upstream_ref = resolve_upstream_ref(&git_bin, &folder, branch.as_deref())
            .ok_or("Could not determine the upstream tracking branch for this Pocket.")?;

        let prev_full = git::run_text(&git_bin, &folder, &["rev-parse", "HEAD"])?;
        let prev_short = git::run_text(&git_bin, &folder, &["rev-parse", "--short", "HEAD"])?;
        let (ahead, behind) = parse_ahead_behind(&git_bin, &folder, &upstream_ref)?;

        if behind == 0 {
            let synced = development::sync(&store, &project.id)?;
            return Ok(UpstreamPullResult {
                project: synced,
                previous_commit: prev_short.clone(),
                current_commit: prev_short,
                backup_branch: None,
                updated: false,
            });
        }

        if working_tree_is_dirty(&git_bin, &folder)? {
            return Err(
                "Your folder has uncommitted changes. Commit or stash them in your coding tool first, \
                 or ask your coding agent to merge the upstream update."
                    .into(),
            );
        }

        // Pre-flight validate the upstream manifest and Containerfile before touching disk.
        validate_upstream_tree(&git_bin, &folder, &project, &upstream_ref)?;

        let backup_branch = match mode {
            PullMode::FastForward => {
                if ahead > 0 {
                    return Err(format!(
                        "This Pocket has {} local {} not in {}. Merge {} in your coding tool to keep \
                         your changes, or choose Replace with upstream.",
                        ahead,
                        if ahead == 1 { "commit" } else { "commits" },
                        upstream_ref,
                        upstream_ref
                    ));
                }
                git::run(&git_bin, &folder, &["merge", "--ff-only", &upstream_ref]).map_err(
                    |err| {
                        format!(
                            "Could not fast-forward to {upstream_ref}: {err}. \
                             Open the folder in your coding tool to merge manually."
                        )
                    },
                )?;
                None
            }
            PullMode::ReplaceWithUpstream => {
                // Ensure no untracked files collide with paths in the upstream tree.
                let untracked = git::split_nul(&git::run(
                    &git_bin,
                    &folder,
                    &["ls-files", "--others", "--exclude-standard", "-z", "--", "."],
                )?);
                if !untracked.is_empty() {
                    let target_files: HashSet<String> = git::split_nul(&git::run(
                        &git_bin,
                        &folder,
                        &["ls-tree", "-r", "-z", "--name-only", &upstream_ref],
                    )?)
                    .into_iter()
                    .collect();
                    if let Some(collision) = untracked.iter().find(|p| target_files.contains(*p)) {
                        return Err(format!(
                            "{collision} is untracked and would be overwritten by upstream. \
                             Commit or move it aside first."
                        ));
                    }
                }

                let backup = if ahead > 0 {
                    let name = allocate_backup_branch(&git_bin, &folder, &prev_short)?;
                    git::run(&git_bin, &folder, &["branch", &name, &prev_full])?;
                    Some(name)
                } else {
                    None
                };

                git::run(&git_bin, &folder, &["reset", "--hard", &upstream_ref]).map_err(|err| {
                    format!("Could not reset to {upstream_ref}: {err}")
                })?;
                backup
            }
        };

        // Sync into Pockets' managed snapshot; roll back if Pockets rejects the pulled tree
        // (for example, if upstream introduced symlinks or oversized files).
        let synced = match development::sync(&store, &project.id) {
            Ok(synced) => synced,
            Err(sync_err) => {
                let _ = git::run(&git_bin, &folder, &["reset", "--hard", &prev_full]);
                return Err(format!(
                    "The upstream update was rolled back because Pockets rejected its files: {sync_err}"
                ));
            }
        };

        let curr_short = git::run_text(&git_bin, &folder, &["rev-parse", "--short", "HEAD"])?;
        Ok(UpstreamPullResult {
            project: synced,
            previous_commit: prev_short,
            current_commit: curr_short,
            backup_branch,
            updated: true,
        })
    })
    .await
    .map_err(|_| "Pulling upstream updates was interrupted.".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_cmd(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn manifest_yaml(version: &str, storage_path: &str) -> String {
        [
            "apiVersion: pockets.dev/v1alpha1",
            "kind: Pocket",
            "metadata:",
            "  id: sample-pocket",
            "  name: Sample Pocket",
            &format!("  version: {version}"),
            "  description: A sample pocket.",
            "runtime:",
            "  image: localhost/pockets-sample-pocket:local",
            "  architectures: [arm64, amd64]",
            "web:",
            "  port: 8080",
            "  healthcheck: /health",
            "storage:",
            "  name: data",
            &format!("  path: {storage_path}"),
            "",
        ]
        .join("\n")
    }

    struct RepoFixture {
        _tmp: tempfile::TempDir,
        ctx: AppContext,
        author: PathBuf,
        user_dir: PathBuf,
        project: Project,
    }

    fn setup_shallow_clone_fixture() -> RepoFixture {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let bare = tmp.path().join("remote.git");
        let author = tmp.path().join("author");
        let user_dir = tmp.path().join("user");
        let data_dir = tmp.path().join("pockets-data");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&author).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        git_cmd(&bare, &["init", "-q", "--bare"]);
        git_cmd(&author, &["init", "-q"]);
        git_cmd(&author, &["config", "user.name", "Author"]);
        git_cmd(&author, &["config", "user.email", "author@example.com"]);
        std::fs::write(author.join("pocket.yaml"), manifest_yaml("0.1.0", "/data")).unwrap();
        std::fs::write(author.join("Containerfile"), "FROM scratch\n").unwrap();
        std::fs::write(author.join("server.js"), "console.log('v1');\n").unwrap();
        std::fs::write(
            author.join("POCKETS_CHANGES.md"),
            "# Changes\n\n- Initial release\n",
        )
        .unwrap();
        git_cmd(&author, &["add", "-A"]);
        git_cmd(&author, &["commit", "-q", "-m", "Initial commit"]);
        git_cmd(&author, &["remote", "add", "origin", bare.to_str().unwrap()]);
        git_cmd(&author, &["push", "-q", "origin", "HEAD:main"]);
        git_cmd(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        // Clone with --depth=1 --single-branch, matching add_github_pocket.
        let clone_out = std::process::Command::new("git")
            .args([
                "clone",
                "-q",
                "--depth=1",
                "--single-branch",
                &format!("file://{}", bare.display()),
                user_dir.to_str().unwrap(),
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("git clone");
        assert!(
            clone_out.status.success(),
            "clone failed: {}",
            String::from_utf8_lossy(&clone_out.stderr)
        );
        git_cmd(&user_dir, &["config", "user.name", "User"]);
        git_cmd(&user_dir, &["config", "user.email", "user@example.com"]);

        let ctx = AppContext::new(data_dir);
        let project = development::link(&ctx.store(), None, &user_dir).expect("link");
        RepoFixture {
            _tmp: tmp,
            ctx,
            author,
            user_dir,
            project,
        }
    }

    #[tokio::test]
    async fn clean_clone_detects_and_fast_forwards_upstream_update() {
        let fx = setup_shallow_clone_fixture();

        let status = check_project_upstream(&fx.ctx, &fx.project.id, true)
            .await
            .expect("check");
        assert_eq!(status.state, UpstreamState::UpToDate);
        assert_eq!(status.ahead, 0);
        assert_eq!(status.behind, 0);

        // Author pushes a new version with POCKETS_CHANGES.md notes.
        std::fs::write(
            fx.author.join("pocket.yaml"),
            manifest_yaml("0.2.0", "/data"),
        )
        .unwrap();
        std::fs::write(fx.author.join("server.js"), "console.log('v2');\n").unwrap();
        std::fs::write(
            fx.author.join("POCKETS_CHANGES.md"),
            "# Changes\n\n- Initial release\n- Add dark mode\n",
        )
        .unwrap();
        git_cmd(&fx.author, &["add", "-A"]);
        git_cmd(&fx.author, &["commit", "-q", "-m", "Add dark mode"]);
        git_cmd(&fx.author, &["push", "-q", "origin", "HEAD:main"]);

        let status = check_project_upstream(&fx.ctx, &fx.project.id, true)
            .await
            .expect("check after push");
        assert_eq!(status.state, UpstreamState::UpdateAvailable);
        assert_eq!(status.behind, 1);
        assert_eq!(status.ahead, 0);
        assert_eq!(status.current_version.as_deref(), Some("0.1.0"));
        assert_eq!(status.upstream_version.as_deref(), Some("0.2.0"));
        assert_eq!(status.notes, vec!["Add dark mode".to_owned()]);

        let pulled = pull_project_upstream(&fx.ctx, &fx.project.id, PullMode::FastForward)
            .await
            .expect("fast-forward pull");
        assert!(pulled.updated);
        assert!(pulled.backup_branch.is_none());
        assert_eq!(pulled.project.manifest.metadata.version, "0.2.0");
        assert_eq!(
            std::fs::read_to_string(fx.user_dir.join("server.js")).unwrap(),
            "console.log('v2');\n"
        );

        let after = check_project_upstream(&fx.ctx, &fx.project.id, true)
            .await
            .expect("check after pull");
        assert_eq!(after.state, UpstreamState::UpToDate);
    }

    #[tokio::test]
    async fn diverged_clone_protects_local_work_and_backs_up_on_replace() {
        let fx = setup_shallow_clone_fixture();

        // User makes a local customization and commits it.
        std::fs::write(fx.user_dir.join("custom.js"), "// user tweak\n").unwrap();
        git_cmd(&fx.user_dir, &["add", "custom.js"]);
        git_cmd(&fx.user_dir, &["commit", "-q", "-m", "My custom tweak"]);

        // Before author pushes anything, local commits must NOT look like an upstream update.
        let before_push = check_project_upstream(&fx.ctx, &fx.project.id, true)
            .await
            .expect("check");
        assert_eq!(before_push.state, UpstreamState::UpToDate);
        assert_eq!(before_push.ahead, 1);
        assert_eq!(before_push.behind, 0);

        // Now author pushes an upstream change.
        std::fs::write(fx.author.join("server.js"), "console.log('upstream v2');\n").unwrap();
        git_cmd(&fx.author, &["add", "-A"]);
        git_cmd(&fx.author, &["commit", "-q", "-m", "Fix startup crash"]);
        git_cmd(&fx.author, &["push", "-q", "origin", "HEAD:main"]);

        let diverged = check_project_upstream(&fx.ctx, &fx.project.id, true)
            .await
            .expect("check diverged");
        assert_eq!(diverged.state, UpstreamState::Diverged);
        assert_eq!(diverged.ahead, 1);
        assert_eq!(diverged.behind, 1);
        assert!(!diverged.local_dirty);
        assert_eq!(diverged.notes, vec!["Fix startup crash".to_owned()]);

        // Fast-forward refuses and leaves the user's file untouched.
        let err = pull_project_upstream(&fx.ctx, &fx.project.id, PullMode::FastForward)
            .await
            .unwrap_err();
        assert!(err.contains("local commit"), "{err}");
        assert!(fx.user_dir.join("custom.js").is_file());

        // ReplaceWithUpstream creates a backup branch and resets cleanly.
        let replaced = pull_project_upstream(&fx.ctx, &fx.project.id, PullMode::ReplaceWithUpstream)
            .await
            .expect("replace");
        assert!(replaced.updated);
        let backup = replaced.backup_branch.expect("backup branch created");
        assert!(backup.starts_with("pockets-backup-"), "{backup}");
        assert_eq!(
            git_cmd(&fx.user_dir, &["show", &format!("{backup}:custom.js")]),
            "// user tweak"
        );
        assert!(!fx.user_dir.join("custom.js").exists());
        assert_eq!(
            std::fs::read_to_string(fx.user_dir.join("server.js")).unwrap(),
            "console.log('upstream v2');\n"
        );
    }

    #[tokio::test]
    async fn dirty_working_tree_refuses_both_pull_modes() {
        let fx = setup_shallow_clone_fixture();

        std::fs::write(fx.author.join("server.js"), "console.log('v2');\n").unwrap();
        git_cmd(&fx.author, &["add", "-A"]);
        git_cmd(&fx.author, &["commit", "-q", "-m", "Upstream change"]);
        git_cmd(&fx.author, &["push", "-q", "origin", "HEAD:main"]);

        // User makes an uncommitted edit.
        std::fs::write(fx.user_dir.join("server.js"), "console.log('wip');\n").unwrap();

        let status = check_project_upstream(&fx.ctx, &fx.project.id, true)
            .await
            .expect("check");
        assert_eq!(status.state, UpstreamState::Diverged);
        assert!(status.local_dirty);

        for mode in [PullMode::FastForward, PullMode::ReplaceWithUpstream] {
            let err = pull_project_upstream(&fx.ctx, &fx.project.id, mode)
                .await
                .unwrap_err();
            assert!(err.contains("uncommitted changes"), "{err}");
        }
        assert_eq!(
            std::fs::read_to_string(fx.user_dir.join("server.js")).unwrap(),
            "console.log('wip');\n"
        );
    }

    #[tokio::test]
    async fn invalid_upstream_manifest_is_rejected_without_touching_folder() {
        let fx = setup_shallow_clone_fixture();

        std::fs::write(fx.author.join("pocket.yaml"), "not: valid: yaml: [").unwrap();
        git_cmd(&fx.author, &["add", "-A"]);
        git_cmd(&fx.author, &["commit", "-q", "-m", "Broken manifest"]);
        git_cmd(&fx.author, &["push", "-q", "origin", "HEAD:main"]);

        let err = pull_project_upstream(&fx.ctx, &fx.project.id, PullMode::FastForward)
            .await
            .unwrap_err();
        assert!(err.contains("invalid"), "{err}");
        assert_eq!(
            std::fs::read_to_string(fx.user_dir.join("server.js")).unwrap(),
            "console.log('v1');\n"
        );
    }

    #[tokio::test]
    async fn local_project_without_remote_reports_no_remote() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let dir = tmp.path().join("personal");
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(dir.join("pocket.yaml"), manifest_yaml("0.1.0", "/data")).unwrap();
        std::fs::write(dir.join("Containerfile"), "FROM scratch\n").unwrap();
        std::fs::write(dir.join("server.js"), "console.log('hi');\n").unwrap();

        let ctx = AppContext::new(data_dir);
        let project = development::link(&ctx.store(), None, &dir).expect("link");
        let status = check_project_upstream(&ctx, &project.id, true)
            .await
            .expect("check");
        assert_eq!(status.state, UpstreamState::NoRemote);
    }
}
