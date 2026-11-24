//! Immutable source-revision history for managed projects.
//!
//! History is **pure storage state**: every retained distinct source
//! state of a project can be listed and restored. Nothing here calls the
//! container engine, a build, or a provider, and no summary ever carries
//! a fabricated timestamp, image reference, or model data.
//!
//! Layout is project-local, inside the project's own store directory:
//!
//! ```text
//! <store>/<pid>/revisions/<rid>/revision.json   private revision metadata
//! <store>/<pid>/revisions/<rid>/source/...      the archived snapshot
//! ```
//!
//! A revision is immutable and committed atomically: bytes are staged in
//! an exclusive private directory, then the whole directory is renamed
//! into the namespace, so a visible revision is always a complete one.
//! What is archived is always the *verified* record: the current managed
//! snapshot is re-captured and re-checked against the project record with
//! the source module's own link-, size- and digest-guarded capture before
//! a single byte is copied, and a restore re-verifies the archived bytes
//! against the revision record before anything is swapped. A corrupt or
//! hostile revision is reported with an actionable error and never
//! silently skipped or followed — including a symbolic link at the
//! namespace, a revision directory, or its metadata file.
//!
//! Identical source digests are deduplicated: history holds distinct
//! source states, so a repeated save adds no revision, and the archived
//! baseline before the first chat edit is retained. History is never
//! pruned automatically; once [`MAX_REVISIONS`] is reached archiving
//! refuses *before* any working code is overwritten. A project that has
//! never archived anything simply lists an empty history.
//!
//! Restore is a **working-source-only** operation: it archives the
//! current source first, then installs the selected archived bytes under
//! the same project id, preserving the installed pin, built image id,
//! phase and preview marker. No container, data volume or image is ever
//! created, started, moved or removed here.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::projects::{Project, Store};
use crate::source;

/// Revision namespace directory inside each project directory.
const REVISIONS_DIR: &str = "revisions";
/// Private metadata file inside each revision directory.
const REVISION_META: &str = "revision.json";
/// Snapshot directory inside each revision directory.
const REVISION_SOURCE: &str = "source";

/// Most revisions one project may keep. Archiving refuses at the limit
/// instead of ever pruning history automatically.
const MAX_REVISIONS: usize = 1_000;

/// Every revision metadata read is bounded to this many bytes. A project
/// record with a maximal Containerfile and file list stays far below it,
/// so anything larger is corruption or a hostile file, not history.
const MAX_REVISION_META_BYTES: u64 = 1024 * 1024;

/// Label of the automatic archive a restore makes of the source it is
/// about to replace.
const RESTORE_LABEL: &str = "before restore";

/// Public, frontend-safe description of one archived source state. It
/// carries only what the revision genuinely records: a native id, the
/// real wall-clock second of the archive, the caller's label, and the
/// source snapshot's own digest, size and file count — never images,
/// providers, or models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionSummary {
    /// Native revision id: UUID v4 in simple form, 32 lowercase hex
    /// characters, generated here and never accepted from the frontend.
    pub id: String,
    /// Unix seconds at which the revision was archived.
    pub created_at_seconds: i64,
    /// Caller-supplied label identifying why the archive exists.
    pub label: String,
    /// Lowercase hex SHA-256 of the archived snapshot.
    pub source_digest: String,
    /// Total size of the archived files in bytes.
    pub total_bytes: u64,
    /// Number of archived files.
    pub files: usize,
}

/// Private on-disk record of one revision: the summary fields plus the
/// full [`Project`] snapshot as recorded at archive time, which is what
/// restore verifies the archived bytes against. Never serialized to the
/// frontend; only [`RevisionSummary`] leaves this module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RevisionRecord {
    id: String,
    created_at_seconds: i64,
    label: String,
    /// Strictly increasing archive order within the project. Wall-clock
    /// seconds cannot order archives made inside the same second, and
    /// ids are random, so [`list`] breaks same-second ties on this
    /// private sequence instead of fabricating an ordering the
    /// [`RevisionSummary`] must not expose.
    sequence: u64,
    project: Project,
}

impl RevisionRecord {
    fn summary(&self) -> RevisionSummary {
        RevisionSummary {
            id: self.id.clone(),
            created_at_seconds: self.created_at_seconds,
            label: self.label.clone(),
            source_digest: self.project.source_digest.clone(),
            total_bytes: self.project.total_bytes,
            files: self.project.files.len(),
        }
    }
}

/// All retained source states of `project_id`, newest first. A project
/// that never archived anything lists empty; anything in the namespace
/// that is not a valid revision directory is an actionable error, never
/// a skipped entry. Ids are native, timestamps are the real archive
/// seconds, and no field is fabricated.
pub(crate) fn list(store: &Store, project_id: &str) -> Result<Vec<RevisionSummary>, String> {
    // `load` validates the id before any join and refuses hostile ids,
    // corrupt records and symlinked project directories up front.
    let project = store.load(project_id)?;
    let revisions = match checked_namespace(store, &project.id)? {
        Some(dir) => dir,
        None => return Ok(Vec::new()),
    };
    let records = read_history(&revisions, &project.id)?;
    let sequences: std::collections::HashMap<String, u64> = records
        .iter()
        .map(|record| (record.id.clone(), record.sequence))
        .collect();
    let mut summaries: Vec<RevisionSummary> = records.iter().map(RevisionRecord::summary).collect();
    // Newest first; the private archive sequence orders same-second
    // archives truthfully, and the id only breaks exact ties.
    summaries.sort_by(|a, b| {
        let sequence_a = sequences.get(&a.id).copied().unwrap_or(0);
        let sequence_b = sequences.get(&b.id).copied().unwrap_or(0);
        b.created_at_seconds
            .cmp(&a.created_at_seconds)
            .then_with(|| sequence_b.cmp(&sequence_a))
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(summaries)
}

/// Archives the project's current managed source as a new revision,
/// unless an identical source digest is already retained — repeated
/// saves of unchanged bytes then return the existing revision instead of
/// writing a new one.
///
/// The archived bytes are exactly what the record proves: the managed
/// snapshot is re-captured through the source module's guarded capture
/// and verified against the record first, so a corrupt managed baseline
/// is reported untouched rather than archived or blessed. At the history
/// limit this refuses *before writing anything*: history is never pruned
/// automatically. A failure leaves the working source exactly as found.
pub(crate) fn archive_current(
    store: &Store,
    project_id: &str,
    label: &str,
) -> Result<RevisionSummary, String> {
    let project = store.load(project_id)?;
    let created_at_seconds = now_seconds()?;

    // Verify-first: only bytes that reproduce the record exactly are
    // ever archived, through the source module's link- and size-guarded
    // capture of the record's own file list.
    let managed = store.source_path(&project.id)?;
    let files = source::capture_record_files(&managed, &project.files)?;
    source::verify_against_record("the managed source", &files, &project)?;

    let revisions = ensure_namespace(store, &project.id)?;
    let history = read_history(&revisions, &project.id)?;
    if let Some(existing) = history
        .iter()
        .find(|record| record.project.source_digest == project.source_digest)
    {
        // Deduplicated: the current source state is already retained.
        return Ok(existing.summary());
    }
    if history.len() >= MAX_REVISIONS {
        return Err(format!(
            "project {} already keeps {} archived revisions, the maximum; move \
             revisions out of {} with a file manager to keep more history. Pockets \
             never prunes revision history automatically, and nothing was changed.",
            project.id,
            MAX_REVISIONS,
            revisions.display()
        ));
    }

    let project_dir = managed.parent().ok_or_else(|| {
        format!(
            "the project directory for {} could not be derived",
            project.id
        )
    })?;
    // Archive order within the project, so same-second archives still
    // list newest first even though ids are random and clocks only
    // resolve to seconds.
    let sequence = history
        .iter()
        .map(|record| record.sequence)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let record = RevisionRecord {
        id: Uuid::new_v4().simple().to_string(),
        created_at_seconds,
        label: label.to_owned(),
        sequence,
        project,
    };

    // Stage the whole revision privately beside the namespace (same
    // filesystem, so the commit below is a rename), then rename it in
    // exclusively: a visible revision is always a complete one.
    let staging = project_dir.join(format!("revision.staging-{}", Uuid::new_v4().simple()));
    create_private_dir(&staging)?;
    if let Err(err) = stage_and_commit(&staging, &revisions, &record, &files) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err);
    }
    Ok(record.summary())
}

/// Publishes the native workspace folder as the project's new working
/// source with history around it, in fixed order:
///
/// 1. Archive the current managed source as the baseline — before the
///    first successful edit overwrites it, and deduplicated for every
///    later call, so the state being replaced is always retained.
/// 2. Refresh the working source through the importer's approval path
///    ([`source::refresh_working_snapshot`]); installed lifecycle state
///    and the storage mount stay pinned.
/// 3. Archive the new working source.
///
/// Failures are honest and ordered by what they actually cost: a baseline
/// or refresh failure leaves the working source untouched; if only the
/// final history write fails, the working source is saved and said to be
/// saved — the operation is *not* claimed to have rolled back — and the
/// next edit archives the current source before overwriting it.
#[cfg(test)]
pub(crate) fn publish_working(
    store: &Store,
    project_id: &str,
    workspace: &Path,
    label: &str,
) -> Result<Project, String> {
    archive_current(store, project_id, label)?;
    let published = source::refresh_working_snapshot(store, project_id, workspace)?;
    if let Err(err) = archive_current(store, project_id, label) {
        return Err(format!(
            "the working source of project {} was saved, but the new revision could \
             not be archived: {err} The saved source is intact and authoritative; \
             the next edit archives it before overwriting.",
            published.id
        ));
    }
    Ok(published)
}

/// Archives the current source (deduplicated), then installs the
/// selected revision's verified archived bytes as the working source of
/// the **same** project, preserving phase, built image id, preview
/// marker and installed metadata. Restore touches working bytes only —
/// no container, data volume or image lifecycle.
///
/// Verification is strict and read-only before anything is written: the
/// revision directory, its metadata and its snapshot must all be real,
/// self-consistent and reproduce the archived digest, so a corrupt,
/// hostile or symlinked revision is refused with the working source and
/// record exactly as found.
pub(crate) fn restore_working(
    store: &Store,
    project_id: &str,
    revision_id: &str,
) -> Result<Project, String> {
    // The revision id is shape-checked before any join, and `load`
    // validates the project id before touching the store.
    validate_revision_id(revision_id)?;
    let project = store.load(project_id)?;
    let revisions = checked_namespace(store, &project.id)?.ok_or_else(|| {
        format!(
            "project {} has no archived revisions to restore from; nothing was changed",
            project.id
        )
    })?;
    let record = read_revision(&revisions, revision_id, &project.id)?;

    // Verify-first: the archived snapshot must reproduce its own record
    // through the guarded capture — including rejecting symlinked
    // recorded paths and the revision source root — before the current
    // source is archived to make room for it.
    let archived_dir = revisions.join(revision_id).join(REVISION_SOURCE);
    let archived = source::capture_record_files(&archived_dir, &record.project.files)?;
    source::verify_against_record("the archived source", &archived, &record.project)?;
    source::enforce_storage_pinned(&project, &record.project.manifest)?;

    // The source about to be replaced is retained first (deduplicated
    // when identical state is already archived), and only then do the
    // verified archived bytes become the working source.
    archive_current(store, project_id, RESTORE_LABEL)?;
    source::restore_working_snapshot(store, &project.id, &record.project, archived)
}

/// The verified archived files of one revision, read-only. The archived
/// snapshot must reproduce its own record through the guarded capture,
/// exactly as [`restore_working`] requires, so a corrupt or symlinked
/// revision is reported rather than compared.
pub(crate) fn revision_files(
    store: &Store,
    project_id: &str,
    revision_id: &str,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    validate_revision_id(revision_id)?;
    let project = store.load(project_id)?;
    let revisions = checked_namespace(store, &project.id)?
        .ok_or_else(|| format!("project {} has no archived revisions", project.id))?;
    let record = read_revision(&revisions, revision_id, &project.id)?;
    let archived_dir = revisions.join(revision_id).join(REVISION_SOURCE);
    let archived = source::capture_record_files(&archived_dir, &record.project.files)?;
    source::verify_against_record("the archived source", &archived, &record.project)?;
    Ok(archived)
}

/// The project-local revision namespace path, derived from
/// [`Store::source_path`] so the project id is validated before any
/// join and the path can never escape the store root.
fn revisions_dir(store: &Store, project_id: &str) -> Result<PathBuf, String> {
    let source_dir = store.source_path(project_id)?;
    let project_dir = source_dir
        .parent()
        .ok_or_else(|| format!("the project directory for {project_id} could not be derived"))?;
    Ok(project_dir.join(REVISIONS_DIR))
}

/// The revision namespace if it exists, after refusing anything at that
/// path that is not a real directory — a planted symlink at the
/// namespace itself is never followed. A missing namespace is simply a
/// project with no history yet.
fn checked_namespace(store: &Store, project_id: &str) -> Result<Option<PathBuf>, String> {
    let dir = revisions_dir(store, project_id)?;
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "the revision directory {} is a symbolic link; Pockets never follows \
             revision symlinks. Replace it with a real directory and retry.",
            dir.display()
        )),
        Ok(meta) if meta.is_dir() => Ok(Some(dir)),
        Ok(_) => Err(format!(
            "the revision entry {} is not a directory; move it out and retry.",
            dir.display()
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!(
            "the revision directory {} could not be inspected: {err}",
            dir.display()
        )),
    }
}

/// The revision namespace, created as a private directory on first use.
fn ensure_namespace(store: &Store, project_id: &str) -> Result<PathBuf, String> {
    if let Some(dir) = checked_namespace(store, project_id)? {
        return Ok(dir);
    }
    let dir = revisions_dir(store, project_id)?;
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Every revision directory under the namespace, validated as if each
/// were about to be restored: real directories with well-formed ids,
/// each holding a valid record for `project_id`. Nothing is skipped.
fn read_history(revisions: &Path, project_id: &str) -> Result<Vec<RevisionRecord>, String> {
    let entries = match std::fs::read_dir(revisions) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(format!(
                "the revision directory {} could not be read: {err}",
                revisions.display()
            ));
        }
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "the revision directory {} could not be read: {err}",
                revisions.display()
            )
        })?;
        // `to_string_lossy` keeps even non-UTF-8 names reportable; a
        // lossy name can never pass the id check, so it is an error.
        let name = entry.file_name().to_string_lossy().into_owned();
        if validate_revision_id(&name).is_err() {
            return Err(format!(
                "unexpected entry {name:?} in the revision directory {}; it may \
                 contain only revision directories, move it out and retry.",
                revisions.display()
            ));
        }
        // `DirEntry::file_type` never follows symlinks, so a symlink or
        // stray file is refused even under a perfectly valid revision
        // id, before anything under it is read.
        let file_type = entry.file_type().map_err(|err| {
            format!(
                "the revision directory {} could not be read: {err}",
                revisions.display()
            )
        })?;
        if !file_type.is_dir() {
            return Err(format!(
                "the revision entry {name:?} in {} is not a real directory; \
                 symlinks and stray files are never revisions, move it out and \
                 retry.",
                revisions.display()
            ));
        }
        records.push(read_revision(revisions, &name, project_id)?);
    }
    Ok(records)
}

/// Reads and validates one revision's private metadata: bounded, never
/// through a symlink, a well-formed record whose revision id matches its
/// directory and whose snapshot belongs to `project_id` and passes the
/// full [`Project::validate`]. Every deviation is an actionable error;
/// corrupt history is never silently skipped.
fn read_revision(
    revisions: &Path,
    revision_id: &str,
    project_id: &str,
) -> Result<RevisionRecord, String> {
    validate_revision_id(revision_id)?;
    let dir = revisions.join(revision_id);
    let dir_meta = std::fs::symlink_metadata(&dir).map_err(|err| {
        format!(
            "the revision {} could not be inspected: {err}",
            dir.display()
        )
    })?;
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return Err(format!(
            "the revision {} is a symbolic link or not a directory; Pockets never \
             follows revision symlinks. Restore a real directory and retry.",
            dir.display()
        ));
    }
    let path = dir.join(REVISION_META);
    let meta = std::fs::symlink_metadata(&path).map_err(|err| {
        format!(
            "the revision metadata at {} could not be inspected: {err}",
            path.display()
        )
    })?;
    if !meta.is_file() {
        return Err(corrupt_revision(
            &path,
            "it is not a regular file; Pockets never reads symlinks or special files",
        ));
    }
    let bytes = source::read_capped(&path, MAX_REVISION_META_BYTES)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| corrupt_revision(&path, "it is not valid UTF-8"))?;
    let record: RevisionRecord = serde_json::from_str(text).map_err(|err| {
        corrupt_revision(&path, &format!("it is not a valid revision record: {err}"))
    })?;
    // The directory name is the authoritative revision id, and the
    // snapshot may only ever describe this project: later operations
    // resolve paths by directory, so a mismatch would redirect them.
    if record.id != revision_id {
        return Err(corrupt_revision(
            &path,
            &format!(
                "its id {:?} does not match the revision directory it is stored in",
                record.id
            ),
        ));
    }
    if record.project.id != project_id {
        return Err(corrupt_revision(
            &path,
            &format!(
                "its snapshot belongs to project {:?}, not {project_id}",
                record.project.id
            ),
        ));
    }
    record
        .project
        .validate()
        .map_err(|err| corrupt_revision(&path, &err))?;
    Ok(record)
}

/// Writes the staged snapshot and metadata, then commits the whole
/// revision with one exclusive rename. A revision is immutable: an
/// existing target is refused, never overwritten.
fn stage_and_commit(
    staging: &Path,
    revisions: &Path,
    record: &RevisionRecord,
    files: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(record)
        .map_err(|err| format!("the revision record could not be serialized: {err}"))?;

    let source_dir = staging.join(REVISION_SOURCE);
    create_private_dir(&source_dir)?;
    // Only the verified capture of the managed bytes is ever written.
    source::write_snapshot(&source_dir, files)?;
    write_private_metadata(&staging.join(REVISION_META), json.as_bytes())?;

    let target = revisions.join(&record.id);
    match std::fs::symlink_metadata(&target) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "the revision target {} could not be inspected: {err}",
                target.display()
            ));
        }
        Ok(_) => {
            return Err(format!(
                "the revision target {} already exists; revisions are immutable \
                 and were not overwritten, retry the archive",
                target.display()
            ));
        }
    }
    std::fs::rename(staging, &target).map_err(|err| {
        format!(
            "the revision {} could not be committed to {}: {err}",
            record.id,
            target.display()
        )
    })
}

/// An exclusive private directory, mode 0700 on Unix: revision history
/// holds the project's private source and is never world- or
/// group-readable.
fn create_private_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    };
    #[cfg(not(unix))]
    let created = std::fs::DirBuilder::new().create(dir);
    created.map_err(|err| {
        format!(
            "the private directory {} could not be created: {err}",
            dir.display()
        )
    })
}

/// Writes the revision metadata exclusively, mode 0600 on Unix.
fn write_private_metadata(path: &Path, bytes: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    let written = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut file| file.write_all(bytes))
    };
    #[cfg(not(unix))]
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| file.write_all(bytes));
    written.map_err(|err| {
        format!(
            "the revision metadata at {} could not be written: {err}",
            path.display()
        )
    })
}

/// The real wall-clock second used as a revision's creation stamp. A
/// clock that cannot be read is an error: revisions never carry a
/// fabricated timestamp.
fn now_seconds() -> Result<i64, String> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| {
            format!(
                "the system clock is unusable ({err}); revisions cannot be \
                 timestamped truthfully, and Pockets never invents a timestamp"
            )
        })?
        .as_secs();
    i64::try_from(seconds)
        .map_err(|_| "the system clock is out of range; revisions cannot be timestamped".to_owned())
}

/// Actionable message for corrupt revision metadata. Nothing is ever
/// deleted or rewritten automatically: the user decides how to clear it.
fn corrupt_revision(path: &Path, detail: &str) -> String {
    format!(
        "the revision metadata at {} is invalid: {detail}. Repair that file or \
         move the revision aside with a file manager, then retry; Pockets never \
         deletes revision history automatically.",
        path.display()
    )
}

/// Native revision id shape: exactly 32 lowercase hex characters,
/// matching the simple UUID v4 form [`archive_current`] generates.
/// Validated before any path join so `..`, separators, or empty ids
/// cannot escape the revision namespace.
fn validate_revision_id(id: &str) -> Result<(), String> {
    let valid = id.len() == 32
        && id
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, 'a'..='f'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{id:?} is not a valid revision id (32 lowercase hexadecimal \
             characters are required)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::Phase;

    /// Minimal manifest that parses and supports both supported host
    /// architectures, matching the source module's test fixture.
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

    fn write_file(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        std::fs::write(path, content).expect("write file");
    }

    /// A three-file Pocket folder used as both an import source and a
    /// publish workspace: `name` renames it, `marker` rewrites the code.
    fn pocket_root(dir: &Path, marker: &str) {
        write_file(dir, "pocket.yaml", TEST_MANIFEST);
        write_file(dir, "Containerfile", "FROM scratch\n");
        write_file(dir, "app/main.go", marker);
    }

    /// A freshly imported project with its store, all rooted in one
    /// returned temp dir that outlives the fixtures.
    fn imported() -> (tempfile::TempDir, Store, Project) {
        let tmp = tempfile::TempDir::new().expect("temp home");
        let source_dir = tmp.path().join("source");
        pocket_root(&source_dir, "package one\n");
        let store = Store::new(tmp.path().join("store"));
        let project = source::import_project(&store, &source_dir).expect("import must succeed");
        (tmp, store, project)
    }

    /// A next workspace folder: same manifest, different code bytes.
    fn workspace(tmp: &tempfile::TempDir, label: &str, marker: &str) -> PathBuf {
        let dir = tmp.path().join(label);
        pocket_root(&dir, marker);
        dir
    }

    fn managed_file(store: &Store, project_id: &str, rel: &str) -> Vec<u8> {
        let managed = store.source_path(project_id).expect("managed source");
        std::fs::read(managed.join(rel)).expect("managed file")
    }

    #[test]
    fn publish_archives_baseline_and_new_source_under_one_project() {
        let (tmp, store, project) = imported();
        // Legacy-shaped records with no history list empty, not an error.
        assert!(list(&store, &project.id).expect("empty history").is_empty());

        let ws2 = workspace(&tmp, "ws2", "package two\n");
        let after_two =
            source_publish(&store, &project.id, &ws2, "edit two").expect("publish must succeed");
        assert_eq!(after_two.id, project.id, "the same record, never a copy");
        assert_eq!(
            managed_file(&store, &project.id, "app/main.go"),
            b"package two\n"
        );

        let history = list(&store, &project.id).expect("history");
        assert_eq!(history.len(), 2, "the baseline and the new source");
        assert_eq!(history[0].source_digest, after_two.source_digest);
        assert_eq!(history[1].source_digest, project.source_digest);
        for summary in &history {
            assert_eq!(summary.id.len(), 32, "native simple UUID ids");
            assert!(summary.created_at_seconds > 0, "real archive seconds");
            assert_eq!(summary.label, "edit two");
            assert_eq!(summary.files, 3);
            assert!(summary.total_bytes > 0);
        }
        assert_eq!(
            store.list().expect("store list").len(),
            1,
            "publishing never creates a project"
        );

        // A second edit archives the state it replaces, then its result.
        let ws3 = workspace(&tmp, "ws3", "package three\n");
        let after_three =
            source_publish(&store, &project.id, &ws3, "edit three").expect("second publish");
        let history = list(&store, &project.id).expect("history");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].source_digest, after_three.source_digest);
        assert_eq!(history[1].source_digest, after_two.source_digest);
        assert_eq!(history[2].source_digest, project.source_digest);
    }

    #[test]
    fn identical_source_deduplicates_and_adds_no_revision() {
        let (tmp, store, project) = imported();
        let ws = workspace(&tmp, "ws", "package two\n");
        let first = source_publish(&store, &project.id, &ws, "edit").expect("publish");
        assert_eq!(list(&store, &project.id).expect("history").len(), 2);

        // Publishing the unchanged workspace again changes nothing.
        let again = source_publish(&store, &project.id, &ws, "edit again").expect("repeat publish");
        assert_eq!(again, first);
        let history = list(&store, &project.id).expect("history");
        assert_eq!(history.len(), 2, "an identical source adds no revision");
        assert_eq!(
            history[1].label, "edit",
            "the original archive label survives"
        );

        // Archiving the current source directly deduplicates too.
        let summary = archive_current(&store, &project.id, "anything").expect("dedup archive");
        assert_eq!(summary.source_digest, again.source_digest);
        assert_eq!(list(&store, &project.id).expect("history").len(), 2);
        assert_eq!(project.source_digest, project.source_digest);
    }

    #[test]
    fn restore_brings_back_older_bytes_without_new_projects() {
        let (tmp, store, project) = imported();
        let ws2 = workspace(&tmp, "ws2", "package two\n");
        source_publish(&store, &project.id, &ws2, "edit two").expect("publish");
        let baseline = list(&store, &project.id)
            .expect("history")
            .into_iter()
            .find(|summary| summary.source_digest == project.source_digest)
            .expect("the baseline is retained");

        let restored = restore_working(&store, &project.id, &baseline.id).expect("restore");
        assert_eq!(restored.id, project.id, "the same project id");
        assert_eq!(restored.source_digest, project.source_digest);
        assert_eq!(restored.containerfile, project.containerfile);
        assert_eq!(
            managed_file(&store, &project.id, "app/main.go"),
            b"package one\n",
            "the archived bytes are the working bytes again"
        );
        assert_eq!(
            list(&store, &project.id).expect("history").len(),
            2,
            "the replaced source was already archived, so dedup kept history flat"
        );
        assert_eq!(
            store.list().expect("store list").len(),
            1,
            "restoring never creates a project"
        );

        // Restoring the state that is already current is a no-op.
        let again = restore_working(&store, &project.id, &baseline.id).expect("repeat restore");
        assert_eq!(again, restored);
        assert_eq!(list(&store, &project.id).expect("history").len(), 2);

        // No staging or temp leftovers anywhere in the project directory.
        let project_dir = store
            .source_path(&project.id)
            .expect("source path")
            .parent()
            .expect("project directory")
            .to_path_buf();
        let mut names: Vec<String> = std::fs::read_dir(&project_dir)
            .expect("read project directory")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "project.json".to_owned(),
                "revisions".to_owned(),
                "source".to_owned()
            ]
        );
    }

    #[test]
    #[cfg(unix)]
    fn corrupt_or_hostile_revisions_are_refused_and_working_survives() {
        let (tmp, store, project) = imported();
        let ws2 = workspace(&tmp, "ws2", "package two\n");
        source_publish(&store, &project.id, &ws2, "edit two").expect("publish");
        let revisions = revisions_dir(&store, &project.id).expect("revisions dir");
        let baseline = list(&store, &project.id)
            .expect("history")
            .into_iter()
            .find(|summary| summary.source_digest == project.source_digest)
            .expect("baseline revision");
        let working = store.load(&project.id).expect("working record");
        let intact = |store: &Store| {
            assert_eq!(store.load(&project.id).expect("working record"), working);
            assert_eq!(
                managed_file(store, &project.id, "app/main.go"),
                b"package two\n",
                "a refusal never touches the working source"
            );
        };

        // A hostile revision id never reaches a path join at all.
        for bad in ["", "..", "../elsewhere", "a/b", &"z".repeat(32)] {
            let err =
                restore_working(&store, &project.id, bad).expect_err("{bad} must never be joined");
            assert!(err.contains("valid revision id"), "got {err}");
        }
        intact(&store);

        // A stray entry in the namespace is corruption, never skipped.
        std::fs::write(revisions.join("notes.txt"), b"keep me").expect("stray entry");
        let err = list(&store, &project.id).expect_err("a stray entry must be reported");
        assert!(err.contains("unexpected entry"), "unexpected error: {err}");
        let err = restore_working(&store, &project.id, &baseline.id)
            .expect_err("restore must not guess around corruption");
        assert!(err.contains("unexpected entry"), "unexpected error: {err}");
        std::fs::remove_file(revisions.join("notes.txt")).expect("remove stray");

        // Metadata claiming a different revision id is corrupt.
        let meta = revisions.join(&baseline.id).join(REVISION_META);
        let raw = std::fs::read_to_string(&meta).expect("revision metadata");
        std::fs::write(&meta, raw.replace(&baseline.id, &"f".repeat(32))).expect("tamper");
        let err = restore_working(&store, &project.id, &baseline.id)
            .expect_err("a mismatched revision id must be refused");
        assert!(err.contains("does not match"), "unexpected error: {err}");
        intact(&store);
        std::fs::write(&meta, &raw).expect("repair");

        // A revision directory replaced by a symlink is refused before
        // it is ever followed.
        std::fs::remove_dir_all(revisions.join(&baseline.id)).expect("remove revision");
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), revisions.join(&baseline.id))
            .expect("symlink fixture");
        let err = restore_working(&store, &project.id, &baseline.id)
            .expect_err("a symlinked revision must be refused");
        assert!(err.contains("symbolic link"), "unexpected error: {err}");
        intact(&store);
    }

    #[test]
    fn the_history_limit_refuses_additional_history_before_any_overwrite() {
        let (_tmp, store, project) = imported();
        let revisions = revisions_dir(&store, &project.id).expect("revisions dir");
        create_private_dir(&revisions).expect("namespace");

        // Seed the namespace with exactly MAX_REVISIONS valid records for
        // a *different* digest, so dedup cannot mask the limit.
        let mut foreign = project.clone();
        foreign.source_digest = "d".repeat(64);
        let template = RevisionRecord {
            id: "a".repeat(32),
            created_at_seconds: 1,
            label: "seed".to_owned(),
            sequence: 1,
            project: foreign,
        };
        let json = serde_json::to_string(&template).expect("seed record");
        for index in 0..MAX_REVISIONS {
            let id = format!("{index:032x}");
            let dir = revisions.join(&id);
            create_private_dir(&dir).expect("seed revision directory");
            std::fs::write(dir.join(REVISION_META), json.replace(&"a".repeat(32), &id))
                .expect("seed metadata");
        }

        let err = archive_current(&store, &project.id, "one too many")
            .expect_err("the limit must refuse additional history");
        assert!(err.contains("maximum"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read_dir(&revisions)
                .expect("read namespace")
                .count(),
            MAX_REVISIONS,
            "the refusal writes nothing"
        );
        assert_eq!(
            managed_file(&store, &project.id, "app/main.go"),
            b"package one\n",
            "the working source is untouched"
        );

        // A repeated identical source still dedups at the limit: the
        // retained state exists, so nothing new is required. Rewrite
        // one seed record (at directory granularity) into the project's
        // *true* current record, so the archive verifies, dedups, and
        // never reaches the limit refusal.
        let live = store.load(&project.id).expect("working record");
        let live_id = format!("{:032x}", 0);
        let live_record = RevisionRecord {
            id: live_id.clone(),
            created_at_seconds: 2,
            label: "seed".to_owned(),
            sequence: 2,
            project: live,
        };
        std::fs::write(
            revisions.join(&live_id).join(REVISION_META),
            serde_json::to_string(&live_record).expect("live seed record"),
        )
        .expect("rewrite seed metadata");
        let summary = archive_current(&store, &project.id, "already retained")
            .expect("dedup must precede the limit");
        assert_eq!(summary.source_digest, project.source_digest);
        assert_eq!(summary.label, "seed", "the original archive is returned");
    }

    #[test]
    fn publish_and_restore_keep_installed_metadata_pinned() {
        use crate::projects::InstalledRevision;

        let (tmp, store, project) = imported();
        let image = format!("sha256:{}", "b".repeat(64));
        let mut installed = project.clone();
        installed.phase = Phase::Installed;
        installed.built_image_id = Some(image.clone());
        installed.installed = Some(InstalledRevision {
            image_id: image.clone(),
            manifest: project.manifest.clone(),
            source_digest: project.source_digest.clone(),
            revision_id: None,
            built_at_seconds: Some(1_700_000_000),
        });
        store
            .save(&installed)
            .expect("persist the installed fixture");

        // Editing the installed project's source keeps the same record
        // with the pin intact, with baseline and new state in history.
        let ws2 = workspace(&tmp, "ws2", "package two\n");
        let published = source_publish(&store, &project.id, &ws2, "edit two")
            .expect("an installed source still edits");
        assert_eq!(published.id, project.id);
        assert_eq!(published.phase, Phase::Installed);
        assert_eq!(published.built_image_id.as_deref(), Some(image.as_str()));
        assert_eq!(
            published.installed, installed.installed,
            "the installed pin is byte-identical after an edit"
        );
        assert_eq!(list(&store, &project.id).expect("history").len(), 2);

        // Moving the installed storage elsewhere is refused outright.
        let moved = tmp.path().join("ws-moved");
        write_file(
            &moved,
            "pocket.yaml",
            &TEST_MANIFEST.replace("path: /data", "path: /elsewhere"),
        );
        write_file(&moved, "Containerfile", "FROM scratch\n");
        write_file(&moved, "app/main.go", "package moved\n");
        let err = source_publish(&store, &project.id, &moved, "moved")
            .expect_err("installed storage may not move");
        assert!(err.contains("pinned"), "unexpected error: {err}");
        assert_eq!(
            store.load(&project.id).expect("record"),
            published,
            "a refused publish changes nothing"
        );
        assert_eq!(list(&store, &project.id).expect("history").len(), 2);

        // Restoring the installed baseline rewrites working bytes only:
        // the pin, image id and phase survive the restore untouched.
        let baseline = list(&store, &project.id)
            .expect("history")
            .into_iter()
            .find(|summary| summary.source_digest == project.source_digest)
            .expect("baseline revision");
        let restored = restore_working(&store, &project.id, &baseline.id).expect("restore");
        assert_eq!(restored.source_digest, project.source_digest);
        assert_eq!(restored.phase, Phase::Installed);
        assert_eq!(restored.built_image_id.as_deref(), Some(image.as_str()));
        assert_eq!(
            restored.installed, installed.installed,
            "the installed pin is untouched by a restore"
        );
        assert_eq!(
            managed_file(&store, &project.id, "app/main.go"),
            b"package one\n"
        );
        assert_eq!(store.list().expect("store list").len(), 1);
    }

    /// `publish_working` under its local test alias (keeps the fixture
    /// lines narrow).
    fn source_publish(
        store: &Store,
        project_id: &str,
        workspace_dir: &Path,
        label: &str,
    ) -> Result<Project, String> {
        publish_working(store, project_id, workspace_dir, label)
    }
}
