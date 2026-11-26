//! Installed-version history: one entry per successful Build/Update.
//!
//! Source revisions ([`crate::revisions`]) are deduplicated by digest and
//! labelled by whatever first archived them, so they cannot say which
//! states were ever *installed*. This module keeps that record separately,
//! as a small append-only log beside the project record:
//!
//! ```text
//! <store>/<pid>/versions.json   private, newest last
//! ```
//!
//! Each entry names the archived revision the install was built from, so
//! the files that changed between consecutive installs can be worked out
//! from the archives Pockets already keeps. Nothing here builds, starts,
//! restores or writes source; listing is read-only, and recording only
//! appends after an install has been committed.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::projects::{InstalledRevision, Project, Store};
use crate::source;

const VERSIONS_FILE: &str = "versions.json";
/// The changes log a coding agent keeps in the source folder: one `- `
/// line per change, newest last. The lines a version adds are its notes.
pub(crate) const CHANGES_FILE: &str = "POCKETS_CHANGES.md";
/// Notes are one-liners; longer lines are cut, and a version lists at most
/// this many, so a pasted document can never flood the panel.
const MAX_NOTE_CHARS: usize = 200;
const MAX_NOTES: usize = 30;
/// The log holds small fixed-shape entries; anything larger is corruption.
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// Most entries kept. The oldest are dropped past this, since the log only
/// describes history and every archived revision itself is untouched.
const MAX_ENTRIES: usize = 500;

/// One recorded install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Entry {
    revision_id: String,
    source_digest: String,
    installed_at_seconds: i64,
    /// Accept older logs that stored a manual note and drop it on next write.
    #[serde(default, skip_serializing, rename = "note")]
    _legacy_note: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Log {
    versions: Vec<Entry>,
}

/// How one file differs between two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

/// One changed file. Line counts are `None` for files that are not text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
}

/// Starter template default `metadata.version`; omitted on the first install
/// unless the owner explicitly changed it.
const DEFAULT_STARTER_VERSION: &str = "0.1.0";

/// Normalizes a manifest version for equality comparisons (ignoring whitespace
/// and any leading `v`/`V`).
fn normalized_manifest_version(version: &str) -> &str {
    let trimmed = version.trim();
    let bare = trimmed.trim_start_matches(['v', 'V']);
    if bare.is_empty() {
        trimmed
    } else {
        bare
    }
}

/// One installed version, newest first in [`VersionsReport::versions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionSummary {
    pub revision_id: String,
    pub installed_at_seconds: i64,
    /// Whether this is what the installed app runs now.
    pub live: bool,
    /// The first recorded install: it has no earlier version to compare to,
    /// so `changes` is empty and `files` counts the whole snapshot.
    pub first: bool,
    pub files: usize,
    /// Changed files, without the changes file itself.
    pub changes: Vec<FileChange>,
    /// Lines the coding agent added to [`CHANGES_FILE`] for this version.
    pub notes: Vec<String>,
    /// `metadata.version` from `pocket.yaml` when this install changed it
    /// relative to the previous install (or started at a non-default version).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_version: Option<String>,
}

/// Synced source that is not live yet, compared with the live version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingChanges {
    pub changes: Vec<FileChange>,
    pub notes: Vec<String>,
    /// `metadata.version` from the synced `pocket.yaml` when it differs from
    /// the live install's version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_version: Option<String>,
}

/// Whether the source lives in a folder Pockets can see git in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SourceControl {
    /// No linked folder: Pockets' managed copy is the only source.
    Unlinked,
    /// A linked folder without a `.git` entry.
    Untracked,
    /// A linked folder tracked with git.
    Git,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionsReport {
    pub versions: Vec<VersionSummary>,
    pub pending: Option<PendingChanges>,
    pub source_control: SourceControl,
}

fn log_path(store: &Store, project_id: &str) -> Result<PathBuf, String> {
    let source_dir = store.source_path(project_id)?;
    let dir = source_dir
        .parent()
        .ok_or_else(|| format!("the project directory for {project_id} could not be derived"))?;
    Ok(dir.join(VERSIONS_FILE))
}

fn read_log(path: &Path) -> Result<Log, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Log::default()),
        Err(error) => return Err(format!("The version history could not be read: {error}")),
    };
    if !metadata.is_file() || metadata.len() > MAX_LOG_BYTES {
        return Err(format!(
            "The version history at {} must be a bounded regular file.",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_LOG_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("The version history could not be read: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "The version history at {} is invalid: {error}",
            path.display()
        )
    })
}

fn write_log(path: &Path, log: &Log) -> Result<(), String> {
    let staging = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(log).map_err(|error| error.to_string())?;
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&staging)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&staging, path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staging);
        return Err(format!("The version history could not be saved: {error}"));
    }
    Ok(())
}

/// Appends the project's current install to its version log. A repeat of
/// the newest entry's source is not a new version and adds nothing. When
/// the log is still empty, the install being replaced (`previous`) is
/// recorded first, so Pockets installed before the log existed keep the
/// version they were updated from.
pub(crate) fn record_install(
    store: &Store,
    project: &Project,
    previous: Option<&InstalledRevision>,
) -> Result<(), String> {
    let Some(installed) = &project.installed else {
        return Ok(());
    };
    let Some(revision_id) = &installed.revision_id else {
        return Ok(());
    };
    let path = log_path(store, &project.id)?;
    let mut log = read_log(&path)?;
    if log.versions.is_empty() {
        if let Some(previous) = previous {
            if let (Some(id), Some(seconds)) = (&previous.revision_id, previous.built_at_seconds) {
                if previous.source_digest != installed.source_digest {
                    log.versions.push(Entry {
                        revision_id: id.clone(),
                        source_digest: previous.source_digest.clone(),
                        installed_at_seconds: seconds,
                        _legacy_note: None,
                    });
                }
            }
        }
    }
    if log
        .versions
        .last()
        .is_some_and(|last| last.source_digest == installed.source_digest)
    {
        return Ok(());
    }
    let installed_at_seconds = match installed.built_at_seconds {
        Some(seconds) => seconds,
        None => now_seconds()?,
    };
    log.versions.push(Entry {
        revision_id: revision_id.clone(),
        source_digest: installed.source_digest.clone(),
        installed_at_seconds,
        _legacy_note: None,
    });
    if log.versions.len() > MAX_ENTRIES {
        let excess = log.versions.len() - MAX_ENTRIES;
        log.versions.drain(..excess);
    }
    write_log(&path, &log)
}

fn now_seconds() -> Result<i64, String> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "the system clock is before the Unix epoch".to_owned())?
        .as_secs();
    i64::try_from(seconds).map_err(|_| "the system clock is out of range".to_owned())
}

/// The recorded installs, oldest first. Pockets installed before the log
/// existed list their current install alone.
fn recorded_entries(store: &Store, project: &Project) -> Result<Vec<Entry>, String> {
    let mut entries = read_log(&log_path(store, &project.id)?)?.versions;
    if entries.is_empty() {
        if let Some(installed) = &project.installed {
            if let Some(revision_id) = &installed.revision_id {
                let created = crate::revisions::list(store, &project.id)?
                    .into_iter()
                    .find(|revision| &revision.id == revision_id)
                    .map(|revision| revision.created_at_seconds);
                if let Some(seconds) = installed.built_at_seconds.or(created) {
                    entries.push(Entry {
                        revision_id: revision_id.clone(),
                        source_digest: installed.source_digest.clone(),
                        installed_at_seconds: seconds,
                        _legacy_note: None,
                    });
                }
            }
        }
    }
    Ok(entries)
}

/// Every recorded version of `project_id`, newest first, with the files
/// each changed relative to the one before it, plus synced changes that
/// are not live yet. Projects installed before the log existed list their
/// current install alone.
pub(crate) fn list(store: &Store, project_id: &str) -> Result<VersionsReport, String> {
    let project = store.load(project_id)?;
    let entries = recorded_entries(store, &project)?;

    let live_digest = project.installed.as_ref().map(|i| i.source_digest.as_str());
    let live_index = entries
        .iter()
        .rposition(|entry| Some(entry.source_digest.as_str()) == live_digest);

    // Walk oldest to newest holding only the previous snapshot, so memory
    // stays at two snapshots however long the history is.
    let snapshot_of =
        |revision_id: &str| -> Result<(String, BTreeMap<String, Vec<u8>>), String> {
            let snap = crate::revisions::revision_snapshot(store, &project.id, revision_id)?;
            Ok((snap.manifest_version, snap.files.into_iter().collect()))
        };
    let mut versions = Vec::with_capacity(entries.len());
    let mut previous: Option<BTreeMap<String, Vec<u8>>> = None;
    let mut previous_version: Option<String> = None;
    let mut live_files: Option<BTreeMap<String, Vec<u8>>> = None;
    for (index, entry) in entries.iter().enumerate() {
        let (raw_version, current) = snapshot_of(&entry.revision_id)?;
        let current_norm = normalized_manifest_version(&raw_version);
        let manifest_version = match &previous_version {
            None => (current_norm != DEFAULT_STARTER_VERSION).then(|| raw_version.clone()),
            Some(prev) => (current_norm != normalized_manifest_version(prev))
                .then(|| raw_version.clone()),
        };
        let changes = match &previous {
            Some(previous) => without_changes_file(diff_snapshots(previous, &current)),
            None => Vec::new(),
        };
        let notes = new_notes(previous.as_ref(), &current);
        versions.push(VersionSummary {
            revision_id: entry.revision_id.clone(),
            installed_at_seconds: entry.installed_at_seconds,
            live: Some(index) == live_index,
            first: index == 0,
            files: current.len(),
            changes,
            notes,
            manifest_version,
        });
        if Some(index) == live_index {
            live_files = Some(current.clone());
        }
        previous = Some(current);
        previous_version = Some(raw_version);
    }

    let pending = match (&project.installed, live_files) {
        (Some(installed), Some(live)) if installed.source_digest != project.source_digest => {
            let managed = store.source_path(&project.id)?;
            let working = source::capture_record_files(&managed, &project.files)?;
            source::verify_against_record("the managed source", &working, &project)?;
            let working: BTreeMap<String, Vec<u8>> = working.into_iter().collect();
            let working_ver = &project.manifest.metadata.version;
            let live_ver = &installed.manifest.metadata.version;
            let manifest_version = (normalized_manifest_version(working_ver)
                != normalized_manifest_version(live_ver))
            .then(|| working_ver.clone());
            Some(PendingChanges {
                changes: without_changes_file(diff_snapshots(&live, &working)),
                notes: new_notes(Some(&live), &working),
                manifest_version,
            })
        }
        _ => None,
    };

    versions.reverse();
    Ok(VersionsReport {
        versions,
        pending,
        source_control: source_control(store, &project.id)?,
    })
}

/// What a restore did to the linked folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRestore {
    /// The Pockets record after the restored folder was synced.
    pub project: Project,
    /// Short id of the commit that recorded the restore, or `None` when
    /// the folder already matched that version.
    pub commit: Option<String>,
}

/// Longest commit subject accepted from the caller.
const MAX_SUBJECT_CHARS: usize = 120;

/// Puts a recorded version's files back into the project's linked folder
/// and commits that change with the user's own git identity, then syncs
/// the folder so the preview and "not live yet" reflect it. The installed
/// app is untouched until the user chooses Update.
///
/// Nothing is written unless the folder is a git work tree whose tracked
/// files have no uncommitted changes, git knows who the user is, and every
/// file the restore would overwrite or delete is tracked — so whatever is
/// replaced can always be recovered from the folder's own history. Only
/// files inside the Pocket's source snapshot are touched; `.git`,
/// `node_modules` and the other excluded folders never are.
pub(crate) fn restore_into_folder(
    store: &Store,
    project_id: &str,
    revision_id: &str,
    subject: &str,
) -> Result<FolderRestore, String> {
    let project = store.load(project_id)?;
    let entry = recorded_entries(store, &project)?
        .into_iter()
        .find(|entry| entry.revision_id == revision_id)
        .ok_or("That version is not in this Pocket's history.")?;
    let subject = commit_subject(subject)?;
    let folder = crate::development::folder(store, &project.id)?;
    let git = crate::git::find_git()
        .ok_or("Git was not found on this computer. Install it, then restore again.")?;

    let inside = crate::git::run_text(&git, &folder, &["rev-parse", "--is-inside-work-tree"])
        .unwrap_or_default();
    if inside != "true" {
        return Err(
            "This folder isn't tracked with git. Run git init and commit your work, then restore."
                .into(),
        );
    }
    let changed = crate::git::run(
        &git,
        &folder,
        &[
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=no",
            "--",
            ".",
        ],
    )?;
    if !changed.is_empty() {
        return Err(
            "Your folder has uncommitted changes. Commit or stash them, then restore.".into(),
        );
    }
    crate::git::run(&git, &folder, &["var", "GIT_AUTHOR_IDENT"]).map_err(|_| {
        "Git doesn't know your name and email yet. Set git config user.name and user.email, then restore."
            .to_owned()
    })?;

    let target: BTreeMap<String, Vec<u8>> =
        crate::revisions::revision_files(store, &project.id, &entry.revision_id)?
            .into_iter()
            .collect();
    let (current, _, _) = source::capture_approved(&folder, "the linked folder")?;
    let current: BTreeMap<String, Vec<u8>> = current.into_iter().collect();
    let changes = diff_snapshots(&current, &target);
    if changes.is_empty() {
        let project = crate::development::sync(store, &project.id)?;
        return Ok(FolderRestore {
            project,
            commit: None,
        });
    }

    let tracked: std::collections::HashSet<String> = crate::git::split_nul(&crate::git::run(
        &git,
        &folder,
        &["ls-files", "-z", "--", "."],
    )?)
    .into_iter()
    .collect();
    for change in &changes {
        let exists = std::fs::symlink_metadata(folder.join(&change.path)).is_ok();
        if exists && !tracked.contains(&change.path) {
            return Err(format!(
                "{} isn't committed to git, so restoring would replace it with no copy kept. \
                 Commit it first, then restore.",
                change.path
            ));
        }
    }

    let root = std::fs::canonicalize(&folder)
        .map_err(|error| format!("The linked folder could not be resolved: {error}"))?;
    for change in &changes {
        let path = folder.join(&change.path);
        match change.kind {
            ChangeKind::Removed => {
                std::fs::remove_file(&path)
                    .map_err(|error| restore_failed(&change.path, &error.to_string()))?;
            }
            ChangeKind::Added | ChangeKind::Modified => {
                let parent = path
                    .parent()
                    .ok_or_else(|| restore_failed(&change.path, "it has no parent folder"))?;
                std::fs::create_dir_all(parent)
                    .map_err(|error| restore_failed(&change.path, &error.to_string()))?;
                // Never write through a planted symlink out of the folder.
                let resolved = std::fs::canonicalize(parent)
                    .map_err(|error| restore_failed(&change.path, &error.to_string()))?;
                if !resolved.starts_with(&root)
                    || std::fs::symlink_metadata(&path)
                        .is_ok_and(|meta| meta.file_type().is_symlink())
                {
                    return Err(restore_failed(
                        &change.path,
                        "it resolves outside the folder",
                    ));
                }
                std::fs::write(&path, &target[&change.path])
                    .map_err(|error| restore_failed(&change.path, &error.to_string()))?;
            }
        }
    }

    let mut add: Vec<&str> = vec!["add", "-A", "--"];
    add.extend(changes.iter().map(|change| change.path.as_str()));
    let message = format!(
        "{subject}\n\nRestored by Pockets from the version made live at {} (Unix time).",
        entry.installed_at_seconds
    );
    let committed = crate::git::run(&git, &folder, &add)
        .and_then(|_| crate::git::run(&git, &folder, &["commit", "-q", "-m", &message]))
        .and_then(|_| crate::git::run_text(&git, &folder, &["rev-parse", "--short", "HEAD"]));
    let commit = committed.map_err(|error| {
        format!(
            "The version's files were put back in your folder, but git could not commit them: \
             {error} Review the changes in your coding tool."
        )
    })?;
    let project = crate::development::sync(store, &project.id).map_err(|error| {
        format!("The version was restored and committed as {commit}, but Pockets could not sync it: {error}")
    })?;
    Ok(FolderRestore {
        project,
        commit: Some(commit),
    })
}

fn restore_failed(path: &str, detail: &str) -> String {
    format!(
        "Restoring stopped at {path}: {detail}. Files changed so far can be undone with git \
         (git checkout -- .)."
    )
}

/// The caller's one-line commit subject, bounded and free of control text.
fn commit_subject(subject: &str) -> Result<String, String> {
    let subject = subject.trim();
    if subject.is_empty()
        || subject.chars().count() > MAX_SUBJECT_CHARS
        || subject.chars().any(char::is_control)
    {
        return Err("The restore needs a short, single-line description.".into());
    }
    Ok(subject.to_owned())
}

fn without_changes_file(changes: Vec<FileChange>) -> Vec<FileChange> {
    changes
        .into_iter()
        .filter(|change| change.path != CHANGES_FILE)
        .collect()
}

/// The `- ` / `* ` list items of a changes file, trimmed and bounded.
fn change_notes(bytes: Option<&Vec<u8>>) -> Vec<String> {
    let Some(text) = bytes.and_then(|bytes| text(bytes)) else {
        return Vec::new();
    };
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
            Some(if item.chars().count() > MAX_NOTE_CHARS {
                let cut: String = item.chars().take(MAX_NOTE_CHARS - 1).collect();
                format!("{cut}…")
            } else {
                item.to_owned()
            })
        })
        .collect()
}

/// The notes `after` adds to its changes file relative to `before`, in
/// file order. Comparison is by line, so edits to old lines don't repeat
/// them and lines reordered by hand don't count as new.
fn new_notes(
    before: Option<&BTreeMap<String, Vec<u8>>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for note in change_notes(before.and_then(|files| files.get(CHANGES_FILE))) {
        *seen.entry(note).or_default() += 1;
    }
    let mut notes = Vec::new();
    for note in change_notes(after.get(CHANGES_FILE)) {
        match seen.get_mut(&note) {
            Some(count) if *count > 0 => *count -= 1,
            _ => notes.push(note),
        }
    }
    // The newest lines matter most when a version piles up many.
    if notes.len() > MAX_NOTES {
        notes.drain(..notes.len() - MAX_NOTES);
    }
    notes
}

fn source_control(store: &Store, project_id: &str) -> Result<SourceControl, String> {
    let Some(folder) = crate::development::linked_folder(store, project_id)? else {
        return Ok(SourceControl::Unlinked);
    };
    // `.git` is a directory in a normal clone and a file in a worktree or
    // submodule; either, here or in a parent folder, means it is tracked.
    let tracked = folder
        .ancestors()
        .any(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok());
    Ok(if tracked {
        SourceControl::Git
    } else {
        SourceControl::Untracked
    })
}

/// Files added, modified and removed going from `before` to `after`, in
/// path order. Unchanged files are omitted.
pub(crate) fn diff_snapshots(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> Vec<FileChange> {
    let mut changes = Vec::new();
    for (path, new) in after {
        match before.get(path) {
            None => {
                let added = text(new).map(count_lines);
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Added,
                    added_lines: added,
                    removed_lines: added.map(|_| 0),
                });
            }
            Some(old) if old != new => {
                let (added, removed) = match (text(old), text(new)) {
                    (Some(old), Some(new)) => {
                        let (added, removed) = line_delta(old, new);
                        (Some(added), Some(removed))
                    }
                    _ => (None, None),
                };
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Modified,
                    added_lines: added,
                    removed_lines: removed,
                });
            }
            Some(_) => {}
        }
    }
    for (path, old) in before {
        if !after.contains_key(path) {
            let removed = text(old).map(count_lines);
            changes.push(FileChange {
                path: path.clone(),
                kind: ChangeKind::Removed,
                added_lines: removed.map(|_| 0),
                removed_lines: removed,
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

/// The bytes as text, or `None` for binary content (not UTF-8, or NUL).
fn text(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn count_lines(text: &str) -> u64 {
    text.lines().count() as u64
}

/// Lines added and removed between two texts, counted as the difference
/// of their line multisets. This matches a line diff whenever lines are
/// edited, added or deleted in place; a line that only moved counts as
/// unchanged. It is linear in the input, so large bundles stay cheap.
fn line_delta(old: &str, new: &str) -> (u64, u64) {
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for line in old.lines() {
        *counts.entry(line).or_default() -= 1;
    }
    for line in new.lines() {
        *counts.entry(line).or_default() += 1;
    }
    let mut added = 0u64;
    let mut removed = 0u64;
    for count in counts.into_values() {
        if count > 0 {
            added += count as u64;
        } else {
            removed += count.unsigned_abs();
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(files: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
        files
            .iter()
            .map(|(path, bytes)| ((*path).to_owned(), bytes.to_vec()))
            .collect()
    }

    #[test]
    fn diff_reports_added_modified_and_removed_files_with_line_counts() {
        let before = snapshot(&[
            ("server.js", b"a\nb\nc\n"),
            ("old.js", b"x\ny\n"),
            ("same.txt", b"same\n"),
        ]);
        let after = snapshot(&[
            ("server.js", b"a\nB\nc\nd\n"),
            ("new.js", b"1\n2\n3\n"),
            ("same.txt", b"same\n"),
        ]);
        let changes = diff_snapshots(&before, &after);
        assert_eq!(
            changes,
            vec![
                FileChange {
                    path: "new.js".into(),
                    kind: ChangeKind::Added,
                    added_lines: Some(3),
                    removed_lines: Some(0),
                },
                FileChange {
                    path: "old.js".into(),
                    kind: ChangeKind::Removed,
                    added_lines: Some(0),
                    removed_lines: Some(2),
                },
                FileChange {
                    path: "server.js".into(),
                    kind: ChangeKind::Modified,
                    added_lines: Some(2),
                    removed_lines: Some(1),
                },
            ]
        );
    }

    #[test]
    fn binary_files_change_without_line_counts() {
        let before = snapshot(&[("icon.png", b"\x89PNG\0one")]);
        let after = snapshot(&[("icon.png", b"\x89PNG\0two")]);
        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].added_lines, None);
        assert_eq!(changes[0].removed_lines, None);
    }

    #[test]
    fn repeated_lines_are_counted_as_a_multiset() {
        assert_eq!(line_delta("a\na\nb\n", "a\nb\nb\nb\n"), (2, 1));
        assert_eq!(line_delta("", "one\n"), (1, 0));
        assert_eq!(line_delta("same\n", "same\n"), (0, 0));
    }

    #[test]
    fn log_round_trips_and_rejects_oversized_or_invalid_files() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let path = tmp.path().join(VERSIONS_FILE);
        assert!(read_log(&path).expect("missing log").versions.is_empty());

        let log = Log {
            versions: vec![Entry {
                revision_id: "a".repeat(32),
                source_digest: "b".repeat(64),
                installed_at_seconds: 1_700_000_000,
                _legacy_note: None,
            }],
        };
        write_log(&path, &log).expect("write");
        assert_eq!(read_log(&path).expect("read").versions, log.versions);

        // Older logs that carried a manual `note` field still parse cleanly.
        std::fs::write(
            &path,
            format!(
                r#"{{"versions":[{{"revisionId":"{}","sourceDigest":"{}","installedAtSeconds":1700000000,"note":"Legacy"}}]}}"#,
                "a".repeat(32),
                "b".repeat(64)
            ),
        )
        .expect("legacy");
        assert_eq!(read_log(&path).expect("read legacy").versions.len(), 1);

        std::fs::write(&path, b"{ not json").expect("corrupt");
        assert!(read_log(&path).is_err());
    }

    const TEST_MANIFEST: &str = concat!(
        "apiVersion: pockets.dev/v1alpha1\n",
        "kind: Pocket\n",
        "metadata:\n",
        "  id: demo-pocket\n",
        "  name: Demo Pocket\n",
        "  version: 0.1.0\n",
        "  description: A demo pocket\n",
        "runtime:\n",
        "  image: localhost/demo-pocket:0.1.0\n",
        "  architectures: [arm64, amd64]\n",
        "web:\n",
        "  port: 8080\n",
        "  healthcheck: /health\n",
        "storage:\n",
        "  name: demo-data\n",
        "  path: /data\n",
    );

    fn pocket_dir(dir: &Path, main: &str) {
        std::fs::create_dir_all(dir.join("app")).expect("dirs");
        std::fs::write(dir.join("pocket.yaml"), TEST_MANIFEST).expect("manifest");
        std::fs::write(dir.join("Containerfile"), "FROM scratch\n").expect("containerfile");
        std::fs::write(dir.join("app/main.go"), main).expect("main");
    }

    /// Installs the project's current working source the way an Update
    /// commits it: archive, pin, save, then record.
    fn install(store: &Store, id: &str, at: i64) -> Project {
        let previous = store.load(id).expect("load");
        let revision =
            crate::revisions::archive_current(store, id, "Build / Update").expect("archive");
        let mut next = previous.clone();
        next.installed = Some(InstalledRevision {
            image_id: format!("sha256:{}", "c".repeat(64)),
            manifest: previous.manifest.clone(),
            source_digest: previous.source_digest.clone(),
            revision_id: Some(revision.id),
            built_at_seconds: Some(at),
        });
        next.built_image_id = Some(format!("sha256:{}", "c".repeat(64)));
        next.phase = crate::projects::Phase::Installed;
        store.save(&next).expect("save");
        record_install(store, &next, previous.installed.as_ref()).expect("record");
        next
    }

    #[test]
    fn installs_are_listed_newest_first_with_changes_and_pending_source() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let first = tmp.path().join("first");
        pocket_dir(&first, "package one\n");
        let store = Store::new(tmp.path().join("store"));
        let project = source::import_project(&store, &first).expect("import");

        install(&store, &project.id, 100);
        // Reinstalling identical source is not a new version.
        install(&store, &project.id, 150);

        let second = tmp.path().join("second");
        pocket_dir(&second, "package one\nfunc two() {}\n");
        std::fs::write(second.join("app/extra.txt"), "hello\n").expect("extra");
        crate::revisions::publish_working(&store, &project.id, &second, "sync").expect("publish");
        install(&store, &project.id, 200);

        let third = tmp.path().join("third");
        pocket_dir(&third, "package three\n");
        crate::revisions::publish_working(&store, &project.id, &third, "sync").expect("publish");

        let report = list(&store, &project.id).expect("list");
        assert_eq!(report.source_control, SourceControl::Unlinked);
        assert_eq!(report.versions.len(), 2);

        let newest = &report.versions[0];
        assert!(newest.live);
        assert!(!newest.first);
        assert_eq!(newest.installed_at_seconds, 200);
        assert_eq!(newest.files, 4);
        assert_eq!(
            newest
                .changes
                .iter()
                .map(|change| (
                    change.path.as_str(),
                    change.kind,
                    change.added_lines,
                    change.removed_lines
                ))
                .collect::<Vec<_>>(),
            vec![
                ("app/extra.txt", ChangeKind::Added, Some(1), Some(0)),
                ("app/main.go", ChangeKind::Modified, Some(1), Some(0)),
            ]
        );

        let oldest = &report.versions[1];
        assert!(!oldest.live);
        assert!(oldest.first);
        assert_eq!(oldest.installed_at_seconds, 100);
        assert!(oldest.changes.is_empty());

        let pending = report.pending.expect("synced source is not live yet");
        assert_eq!(
            pending
                .changes
                .iter()
                .map(|change| (change.path.as_str(), change.kind))
                .collect::<Vec<_>>(),
            vec![
                ("app/extra.txt", ChangeKind::Removed),
                ("app/main.go", ChangeKind::Modified),
            ]
        );
    }

    #[test]
    fn a_pocket_installed_before_the_log_lists_its_current_install() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let first = tmp.path().join("first");
        pocket_dir(&first, "package one\n");
        let store = Store::new(tmp.path().join("store"));
        let project = source::import_project(&store, &first).expect("import");
        install(&store, &project.id, 100);
        std::fs::remove_file(log_path(&store, &project.id).expect("path")).expect("drop log");

        let report = list(&store, &project.id).expect("list");
        assert_eq!(report.versions.len(), 1);
        assert!(report.versions[0].live);
        assert_eq!(report.versions[0].installed_at_seconds, 100);
        assert!(report.pending.is_none());

        // The next Update keeps that earlier install in the log.
        let second = tmp.path().join("second");
        pocket_dir(&second, "package two\n");
        crate::revisions::publish_working(&store, &project.id, &second, "sync").expect("publish");
        install(&store, &project.id, 200);
        let report = list(&store, &project.id).expect("list");
        assert_eq!(
            report
                .versions
                .iter()
                .map(|version| version.installed_at_seconds)
                .collect::<Vec<_>>(),
            vec![200, 100]
        );
    }

    fn git(dir: &Path, args: &[&str]) -> String {
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

    /// A linked, git-tracked Pocket folder with two installed versions:
    /// v1 has `server.js` = "one"; v2 changes it and adds `extra.js`.
    fn linked_with_two_versions() -> (tempfile::TempDir, Store, PathBuf, Project, String, String) {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let root = tmp.path().join("tracker");
        std::fs::create_dir(&root).expect("root");
        std::fs::write(root.join("pocket.yaml"), TEST_MANIFEST).expect("manifest");
        std::fs::write(root.join("Containerfile"), "FROM scratch\n").expect("containerfile");
        std::fs::write(root.join("server.js"), "one\n").expect("server");
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "Toast"]);
        git(&root, &["config", "user.email", "toast@example.com"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "first"]);
        let store = Store::new(tmp.path().join("store"));
        let project = crate::development::link(&store, None, &root).expect("link");
        install(&store, &project.id, 100);

        std::fs::write(root.join("server.js"), "one\ntwo\n").expect("server");
        std::fs::write(root.join("extra.js"), "extra\n").expect("extra");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "second"]);
        crate::development::sync(&store, &project.id).expect("sync");
        install(&store, &project.id, 200);

        let report = list(&store, &project.id).expect("list");
        let v2 = report.versions[0].revision_id.clone();
        let v1 = report.versions[1].revision_id.clone();
        (tmp, store, root, project, v1, v2)
    }

    #[test]
    fn restore_writes_the_version_into_the_folder_as_a_commit_and_syncs_it() {
        let (_tmp, store, root, project, v1, _v2) = linked_with_two_versions();
        assert_eq!(
            list(&store, &project.id).expect("list").source_control,
            SourceControl::Git
        );

        let restored = restore_into_folder(&store, &project.id, &v1, "Restore: first version")
            .expect("restore");
        let commit = restored.commit.expect("a commit records the restore");
        assert_eq!(git(&root, &["rev-parse", "--short", "HEAD"]), commit);
        assert_eq!(
            git(&root, &["log", "-1", "--format=%s"]),
            "Restore: first version"
        );
        assert_eq!(git(&root, &["log", "-1", "--format=%an"]), "Toast");
        assert_eq!(
            std::fs::read_to_string(root.join("server.js")).unwrap(),
            "one\n"
        );
        assert!(!root.join("extra.js").exists());
        // Nothing is lost: the replaced version is one commit back.
        assert_eq!(git(&root, &["show", "HEAD~1:extra.js"]), "extra");
        assert_eq!(git(&root, &["status", "--porcelain"]), "");

        // The folder was synced: the preview source is v1 again, and the
        // live app (v2) is untouched until Update, so it is pending.
        let report = list(&store, &project.id).expect("list");
        assert!(report.versions[0].live);
        let pending = report.pending.expect("restored source is not live yet");
        assert_eq!(
            pending
                .changes
                .iter()
                .map(|change| (change.path.as_str(), change.kind))
                .collect::<Vec<_>>(),
            vec![
                ("extra.js", ChangeKind::Removed),
                ("server.js", ChangeKind::Modified)
            ]
        );
        assert_eq!(
            restored.project.source_digest,
            store.load(&project.id).unwrap().source_digest
        );

        // Restoring the version the folder already holds commits nothing.
        let again = restore_into_folder(&store, &project.id, &v1, "Restore: first version")
            .expect("no-op restore");
        assert_eq!(again.commit, None);
    }

    #[test]
    fn restore_refuses_uncommitted_or_untracked_work_and_untracked_folders() {
        let (_tmp, store, root, project, v1, v2) = linked_with_two_versions();

        std::fs::write(root.join("server.js"), "work in progress\n").expect("edit");
        let error = restore_into_folder(&store, &project.id, &v1, "Restore").unwrap_err();
        assert!(error.contains("uncommitted changes"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("server.js")).unwrap(),
            "work in progress\n",
            "nothing was written"
        );
        git(&root, &["checkout", "--", "server.js"]);

        restore_into_folder(&store, &project.id, &v1, "Restore v1").expect("restore v1");
        // An untracked file where the restore would write is never replaced.
        std::fs::write(root.join("extra.js"), "my own notes\n").expect("untracked");
        let error = restore_into_folder(&store, &project.id, &v2, "Restore v2").unwrap_err();
        assert!(
            error.contains("extra.js") && error.contains("isn't committed"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("extra.js")).unwrap(),
            "my own notes\n"
        );

        assert!(restore_into_folder(&store, &project.id, &"f".repeat(32), "Restore").is_err());
        assert!(restore_into_folder(&store, &project.id, &v2, "two\nlines").is_err());

        std::fs::remove_dir_all(root.join(".git")).expect("untrack");
        std::fs::remove_file(root.join("extra.js")).expect("tidy");
        let error = restore_into_folder(&store, &project.id, &v2, "Restore v2").unwrap_err();
        assert!(error.contains("isn't tracked with git"), "{error}");
        assert_eq!(
            list(&store, &project.id).expect("list").source_control,
            SourceControl::Untracked
        );
    }

    #[test]
    fn notes_are_the_changes_file_lines_each_version_adds() {
        let before = snapshot(&[(
            CHANGES_FILE,
            b"# Changes\n\n- Add the watchlist\n- Fix rounding\n",
        )]);
        let after = snapshot(&[(
            CHANGES_FILE,
            b"# Changes\n\nIntro text is ignored.\n- Add the watchlist\n- Fix rounding\n* Add price alerts\n-   \n- Fix rounding\n",
        )]);
        assert_eq!(
            new_notes(Some(&before), &after),
            vec!["Add price alerts".to_owned(), "Fix rounding".to_owned()]
        );
        // The first version's notes are everything in its file.
        assert_eq!(new_notes(None, &before).len(), 2);
        // Binary or missing files have no notes.
        assert!(new_notes(None, &snapshot(&[(CHANGES_FILE, b"\0")])).is_empty());
        assert!(new_notes(None, &snapshot(&[])).is_empty());
        // Overlong lines are cut, and only the newest MAX_NOTES are kept.
        let long = format!("- {}\n", "x".repeat(500));
        let many: String = (0..40).map(|i| format!("- note {i}\n")).collect();
        let notes = new_notes(
            None,
            &snapshot(&[(CHANGES_FILE, format!("{long}{many}").as_bytes())]),
        );
        assert_eq!(notes.len(), MAX_NOTES);
        assert_eq!(notes.last().map(String::as_str), Some("note 39"));
        let cut = new_notes(None, &snapshot(&[(CHANGES_FILE, long.as_bytes())]));
        assert_eq!(cut[0].chars().count(), MAX_NOTE_CHARS);
        assert!(cut[0].ends_with('…'));
    }

    #[test]
    fn versions_carry_agent_notes_and_owner_notes_without_listing_the_changes_file() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let first = tmp.path().join("first");
        pocket_dir(&first, "package one\n");
        std::fs::write(first.join(CHANGES_FILE), "- Start the app\n").expect("changes");
        let store = Store::new(tmp.path().join("store"));
        let project = source::import_project(&store, &first).expect("import");
        install(&store, &project.id, 100);

        let second = tmp.path().join("second");
        pocket_dir(&second, "package two\n");
        std::fs::write(
            second.join(CHANGES_FILE),
            "- Start the app\n- Add filters\n",
        )
        .expect("changes");
        crate::revisions::publish_working(&store, &project.id, &second, "sync").expect("publish");
        install(&store, &project.id, 200);

        let third = tmp.path().join("third");
        pocket_dir(&third, "package three\n");
        std::fs::write(
            third.join(CHANGES_FILE),
            "- Start the app\n- Add filters\n- Sort by price\n",
        )
        .expect("changes");
        crate::revisions::publish_working(&store, &project.id, &third, "sync").expect("publish");

        let report = list(&store, &project.id).expect("list");
        assert_eq!(report.versions[0].notes, vec!["Add filters".to_owned()]);
        assert_eq!(report.versions[1].notes, vec!["Start the app".to_owned()]);
        assert!(report.versions[0]
            .changes
            .iter()
            .all(|change| change.path != CHANGES_FILE));
        let pending = report.pending.expect("pending");
        assert_eq!(pending.notes, vec!["Sort by price".to_owned()]);
        assert_eq!(pending.changes.len(), 1, "only app/main.go");

        install(&store, &project.id, 300);
        let report = list(&store, &project.id).expect("list");
        assert_eq!(report.versions.len(), 3);
        assert_eq!(report.versions[0].notes, vec!["Sort by price".to_owned()]);
    }

    #[test]
    fn manifest_version_appears_only_when_non_default_or_changed() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let store = Store::new(tmp.path().join("store"));

        let first = tmp.path().join("first");
        pocket_dir(&first, "package one\n");
        let project = source::import_project(&store, &first).expect("import");
        install(&store, &project.id, 100);

        // Staying on default 0.1.0 leaves manifest_version unset.
        let second = tmp.path().join("second");
        pocket_dir(&second, "package two\n");
        crate::revisions::publish_working(&store, &project.id, &second, "sync").expect("publish");
        install(&store, &project.id, 200);

        // Bumping to 0.2.0 surfaces on pending, then on the installed build.
        let third = tmp.path().join("third");
        pocket_dir(&third, "package three\n");
        std::fs::write(
            third.join("pocket.yaml"),
            TEST_MANIFEST.replace("version: 0.1.0", "version: 0.2.0"),
        )
        .expect("manifest");
        crate::revisions::publish_working(&store, &project.id, &third, "sync").expect("publish");

        let before_install = list(&store, &project.id).expect("list");
        assert_eq!(before_install.versions[0].manifest_version, None);
        assert_eq!(before_install.versions[1].manifest_version, None);
        assert_eq!(
            before_install
                .pending
                .as_ref()
                .and_then(|p| p.manifest_version.as_deref()),
            Some("0.2.0")
        );

        install(&store, &project.id, 300);

        // A follow-up edit that keeps 0.2.0 does not repeat the badge.
        let fourth = tmp.path().join("fourth");
        pocket_dir(&fourth, "package four\n");
        std::fs::write(
            fourth.join("pocket.yaml"),
            TEST_MANIFEST.replace("version: 0.1.0", "version: v0.2.0"),
        )
        .expect("manifest");
        crate::revisions::publish_working(&store, &project.id, &fourth, "sync").expect("publish");
        install(&store, &project.id, 400);

        let report = list(&store, &project.id).expect("list");
        assert_eq!(
            report
                .versions
                .iter()
                .map(|v| v.manifest_version.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("0.2.0"), None, None]
        );

        // A Pocket whose first install starts at a non-default version shows it.
        let hub_dir = tmp.path().join("hub");
        pocket_dir(&hub_dir, "package hub\n");
        std::fs::write(
            hub_dir.join("pocket.yaml"),
            TEST_MANIFEST.replace("version: 0.1.0", "version: 0.4.0"),
        )
        .expect("manifest");
        let hub_project = source::import_project(&store, &hub_dir).expect("import");
        install(&store, &hub_project.id, 100);
        let hub_report = list(&store, &hub_project.id).expect("list");
        assert_eq!(
            hub_report.versions[0].manifest_version.as_deref(),
            Some("0.4.0")
        );
    }
}
