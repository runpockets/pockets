//! Native project record storage for imported Pockets.
//!
//! Each project owns an exclusive directory under the store root (supplied
//! by the app as `app_data_dir().join("projects")` in a follow-up slice):
//!
//! ```text
//! <root>/<id>/project.json   the [`Project`] record
//! <root>/<id>/source/        reserved for the imported source snapshot
//! ```
//!
//! Records are written atomically (temp file plus rename) and are the only
//! native state kept per project. The original host source path is
//! deliberately never persisted — only the [`SourceSummary`] snapshot
//! description. Every id-crossing method validates the id before joining it
//! onto the root, so a hostile id can never escape the store. Corrupt or
//! unexpected entries surface as actionable errors; they are never silently
//! skipped. Nothing is ever deleted except by [`Store::remove`], the
//! explicit hard delete of one validated project directory: it removes only
//! a real directory inside the store root, never a symlink, a stray file,
//! or anything belonging to another project.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::manifest::Manifest;

/// File name of the record inside each project directory.
const RECORD_FILE: &str = "project.json";

/// Reserved directory inside each project directory, holding the imported
/// source snapshot. Created empty by [`Store::create`].
const SOURCE_DIR: &str = "source";

/// Lifecycle phase of a project record. `Draft` means the record exists
/// with its source summary but no revision has been installed; `Installed`
/// means an [`InstalledRevision`] is registered for lifecycle control.
/// Older records can persist a built image id with `Draft`; [`Store::load`]
/// and [`Store::save`] normalize those onto `Installed` with a pinned
/// revision (see [`Project::normalize_identity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Draft,
    Installed,
}

/// Immutable summary of the imported source snapshot, captured at import
/// time. The original host source path is intentionally not part of it (or
/// of any record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceSummary {
    /// Source-relative paths of every snapshotted file.
    pub files: Vec<String>,
    /// Total size of the snapshotted files in bytes.
    pub total_bytes: u64,
    /// Containerfile content, kept verbatim for UI review.
    pub containerfile: String,
    /// Lowercase hex SHA-256 of the snapshot: exactly 64 `[0-9a-f]`
    /// characters.
    pub source_digest: String,
}

impl SourceSummary {
    /// Validates the digest shape, Containerfile presence, and that every
    /// file path is a safe normalized source-relative path.
    pub fn validate(&self) -> Result<(), String> {
        validate_summary(&self.files, &self.containerfile, &self.source_digest)
    }
}

/// The single revision currently installed for a Pocket: the exact image,
/// manifest, and source digest the running pocket was built from. This is
/// the authoritative installed identity; the working fields on [`Project`]
/// describe what the source is *now*, which may already have drifted (chat
/// edits) or been reverted (history restore). Built-at and revision id are
/// recorded when the builder knows them and stay `None` when it does not —
/// they are never fabricated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstalledRevision {
    /// `sha256:<64 hex>` id of the installed image.
    pub image_id: String,
    /// The manifest the installed image was built from.
    pub manifest: Manifest,
    /// Lowercase hex SHA-256 of the source snapshot the image was built
    /// from. [`Project::has_pending_changes`] compares this against the
    /// working source digest.
    pub source_digest: String,
    /// Identifier of the history revision this install came from, when the
    /// builder recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    /// Unix seconds of the build, when known. `None` means unknown — legacy
    /// normalizations never guess a timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at_seconds: Option<i64>,
}

impl InstalledRevision {
    /// Validates the installed identity: image id shape, manifest value
    /// constraints, source digest shape, and a present revision id being
    /// nonempty. Host architecture is not checked here, mirroring
    /// [`Project::validate`].
    pub fn validate(&self) -> Result<(), String> {
        validate_image_id("installed.imageId", &self.image_id)?;
        self.manifest.validate()?;
        validate_digest_hex("installed.sourceDigest", &self.source_digest)?;
        if let Some(revision_id) = &self.revision_id {
            if revision_id.is_empty() {
                return Err("installed.revisionId must not be empty when present".to_owned());
            }
        }
        Ok(())
    }
}

/// The stored record for one Pocket project: the working source summary
/// flattened at the top level, plus the single [`InstalledRevision`] that is
/// currently installed (if any) and the lifecycle phase derived from it.
///
/// `manifest`, `files`, `total_bytes`, `containerfile`, and `source_digest`
/// describe the *working* source — what a future build would install. The
/// installed identity lives in `installed`; `built_image_id` is only a
/// wire-compatible mirror of `installed.image_id` and is never an
/// independent authority after [`Store::load`] or [`Store::save`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Project {
    /// Native identifier: UUID v4 in simple form, 32 lowercase hex
    /// characters. Generated only here, never accepted from the frontend.
    pub id: String,
    /// The validated manifest of the working source.
    pub manifest: Manifest,
    pub phase: Phase,
    /// Wire-compatible mirror of `installed.image_id`: `sha256:<64 hex>`
    /// once an image is installed, otherwise `None`. Older readers keep
    /// using this field; new code treats `installed` as authoritative.
    pub built_image_id: Option<String>,
    /// The single revision currently installed, or `None` while the pocket
    /// has never been built. Absent from the wire format for drafts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<InstalledRevision>,
    /// Whether the project has been opened in the browser at least once.
    pub previewed: bool,
    /// Source-relative paths of the snapshotted working files.
    pub files: Vec<String>,
    /// Total size of the snapshotted working files in bytes.
    pub total_bytes: u64,
    /// Containerfile content of the working source, for UI review.
    pub containerfile: String,
    /// Lowercase hex SHA-256 of the working source snapshot.
    pub source_digest: String,
    /// The project this record was edited from: a draft imported as a copy
    /// of an installed (or built) project names that original here, so the
    /// UI can say where the draft came from. `None` means no lineage was
    /// ever set — a fresh import or generation does not know a parent, and
    /// no refresh path ever re-derives or clears it. It names the parent by
    /// id only (32 lowercase hex characters like [`Project::id`]); whether
    /// that project still exists is never assumed or required here. Absent
    /// from the wire format while `None`, so records written before the
    /// field existed load unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_from: Option<String>,
}

impl Project {
    /// Creates a fresh draft record: validates the manifest (including
    /// host-architecture support) and the summary, then generates a new id
    /// natively with no built image and no preview.
    pub fn new(manifest: Manifest, summary: SourceSummary) -> Result<Project, String> {
        manifest.validate()?;
        manifest.validate_host()?;
        summary.validate()?;
        Ok(Project {
            id: new_id(),
            manifest,
            phase: Phase::Draft,
            built_image_id: None,
            installed: None,
            previewed: false,
            files: summary.files,
            total_bytes: summary.total_bytes,
            containerfile: summary.containerfile,
            source_digest: summary.source_digest,
            // A fresh import knows no lineage at construction: the importer
            // is the shared approval path for plain folder picks and
            // generated workspaces alike, and it never learns a parent. The
            // caller who *does* know one (an installed-edit copy) sets the
            // field explicitly after the import.
            edited_from: None,
        })
    }

    /// The source snapshot summary carried by this record. Test-only
    /// accessor: production code reads the record fields directly.
    #[cfg(test)]
    pub fn summary(&self) -> SourceSummary {
        SourceSummary {
            files: self.files.clone(),
            total_bytes: self.total_bytes,
            containerfile: self.containerfile.clone(),
            source_digest: self.source_digest.clone(),
        }
    }

    /// The manifest of the installed revision. A never-built draft has no
    /// installed identity yet, so it falls back to the working manifest —
    /// what a first build would install.
    pub fn installed_manifest(&self) -> &Manifest {
        self.installed
            .as_ref()
            .map(|revision| &revision.manifest)
            .unwrap_or(&self.manifest)
    }

    /// Whether the working source differs from the installed revision,
    /// compared by source digest. A never-built draft is always pending:
    /// there is no installed revision for the working source to match.
    pub fn has_pending_changes(&self) -> bool {
        match &self.installed {
            Some(revision) => revision.source_digest != self.source_digest,
            None => true,
        }
    }

    /// Brings the installed identity into its canonical form, after
    /// validation:
    ///
    /// - `installed` present (the authority): `built_image_id` mirrors its
    ///   image id and the phase is `Installed`. A conflicting, different
    ///   `built_image_id` never reaches this point — `validate` refuses the
    ///   ambiguity instead of silently resolving it.
    /// - Legacy shape, no `installed` but a durable `built_image_id`: pins
    ///   an [`InstalledRevision`] from the record's own manifest and source
    ///   digest, with `revision_id` and `built_at_seconds` left `None`
    ///   (unknown, never fabricated), and projects the phase onto
    ///   `Installed`.
    /// - Nothing installed and no built image: a genuine draft; the stored
    ///   phase is kept as-is.
    ///
    /// [`Store::load`] applies this in memory without rewriting the record,
    /// and [`Store::save`] writes the normalized form, so the pin exists
    /// before any later edit can confuse it with working state.
    fn normalize_identity(&mut self) {
        match (&self.installed, &self.built_image_id) {
            (None, Some(image_id)) => {
                self.installed = Some(InstalledRevision {
                    image_id: image_id.clone(),
                    manifest: self.manifest.clone(),
                    source_digest: self.source_digest.clone(),
                    revision_id: None,
                    built_at_seconds: None,
                });
                self.phase = Phase::Installed;
            }
            (Some(revision), _) => {
                self.built_image_id = Some(revision.image_id.clone());
                self.phase = Phase::Installed;
            }
            (None, None) => {}
        }
    }

    /// Validates the whole record: id shape, manifest value constraints,
    /// the built image id when present, the installed revision when
    /// present (including that it does not contradict the built image
    /// mirror), the `editedFrom` parent id when present, and the working
    /// summary fields. The host architecture is only checked when a record
    /// is created, so records remain readable even if the store is moved
    /// between machines.
    pub fn validate(&self) -> Result<(), String> {
        validate_project_id(&self.id)?;
        self.manifest.validate()?;
        if let Some(image_id) = &self.built_image_id {
            validate_image_id("builtImageId", image_id)?;
        }
        if let Some(parent) = &self.edited_from {
            // The parent is named by project id, so it carries exactly the
            // [`validate_project_id`] shape rule — under its own field name
            // in the error. Existence of the named project is never
            // required: lineage is durable memory, not a live reference.
            if !is_project_id(parent) {
                return Err(format!(
                    "editedFrom {parent:?} is not a valid project id (32 lowercase \
                     hexadecimal characters are required)"
                ));
            }
        }
        if let Some(revision) = &self.installed {
            revision.validate()?;
            // `installed` is authoritative, but two different Some values
            // are an ambiguity, not a mirror: refuse rather than pick one.
            if let Some(image_id) = &self.built_image_id {
                if image_id != &revision.image_id {
                    return Err(format!(
                        "builtImageId {image_id:?} conflicts with installed.imageId \
                         {:?}; the installed revision is authoritative, repair the \
                         record so builtImageId mirrors installed.imageId",
                        revision.image_id
                    ));
                }
            }
        }
        validate_summary(&self.files, &self.containerfile, &self.source_digest)
    }
}

/// File-backed project store. The root is supplied by the app, not derived
/// here, so the store stays testable and independent of Tauri path APIs.
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens the store rooted at `root` (the app data `projects` directory).
    /// The root need not exist yet: a missing root is an empty store, and
    /// [`Store::create`] creates it on demand.
    pub fn new(root: PathBuf) -> Store {
        Store { root }
    }

    /// The store root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Native-only: the reserved source directory for `id`. Must never be
    /// exposed to the frontend; only this crate resolves ids to paths.
    pub fn source_path(&self, id: &str) -> Result<PathBuf, String> {
        Ok(self.project_dir(id)?.join(SOURCE_DIR))
    }

    /// Allocates a new project: builds a draft record with a fresh native
    /// id, then exclusively creates `<root>/<id>` (an existing directory is
    /// never overwritten), reserves its `source/` directory, and atomically
    /// writes the record.
    pub fn create(&self, manifest: Manifest, summary: SourceSummary) -> Result<Project, String> {
        let mut project = Project::new(manifest, summary)?;
        std::fs::create_dir_all(&self.root).map_err(|err| {
            format!(
                "the project store at {} could not be created: {err}",
                self.root.display()
            )
        })?;
        // A collision would require a UUID v4 collision; regenerating once
        // is the entire recovery story, and the exclusive mkdir guarantees
        // no existing project is ever overwritten either way.
        for attempt in 0..2 {
            match std::fs::create_dir(self.project_dir(&project.id)?) {
                Ok(()) => break,
                Err(err) if attempt == 0 && err.kind() == std::io::ErrorKind::AlreadyExists => {
                    project.id = new_id();
                }
                Err(err) => {
                    return Err(format!(
                        "the project directory for {} could not be created: {err}",
                        project.id
                    ));
                }
            }
        }
        std::fs::create_dir(self.source_path(&project.id)?).map_err(|err| {
            format!(
                "the source directory for {} could not be reserved: {err}",
                project.id
            )
        })?;
        self.save(&project)?;
        Ok(project)
    }

    /// Loads one record by id. The project directory must be a real
    /// directory (never a symlink or stray file) and the record must be a
    /// regular file (never a symlink or special file), checked with lstat
    /// before anything is read — commands load by id directly, so this
    /// guard cannot rely on [`Store::list`] having run. Deserialization
    /// rejects unknown fields, the parsed record is re-validated, and the
    /// record's own id must equal the directory it is stored in, so a
    /// hand-edited or corrupt record produces an actionable error instead
    /// of a partial value. Once validated, the record's installed identity
    /// is normalized in memory with [`Project::normalize_identity`] — a
    /// legacy built-image-id-without-pin gains its pinned
    /// [`InstalledRevision`], and `builtImageId` becomes a mirror — without
    /// the load ever rewriting the file; the next durable commit of the
    /// record persists the normalized form on its own.
    pub fn load(&self, id: &str) -> Result<Project, String> {
        let dir = self.project_dir(id)?;
        let path = dir.join(RECORD_FILE);
        let unreadable = |err: std::io::Error| {
            format!(
                "project {id} could not be read from {}: {err}",
                path.display()
            )
        };
        // lstat never follows the final symlink: a substituted project
        // directory is refused before its record is even located.
        let dir_meta = std::fs::symlink_metadata(&dir).map_err(|err| unreadable(err))?;
        if !dir_meta.is_dir() {
            return Err(format!(
                "the project entry {} is not a real directory; symlinks and \
                 stray files cannot be projects, replace it with a real \
                 project directory",
                dir.display()
            ));
        }
        // Likewise the record itself must be a plain regular file.
        let record_meta = std::fs::symlink_metadata(&path).map_err(|err| unreadable(err))?;
        if !record_meta.is_file() {
            return Err(corrupt_record(
                &path,
                "it is not a regular file; Pockets never reads symlinks or special files",
            ));
        }
        let text = std::fs::read_to_string(&path).map_err(|err| unreadable(err))?;
        let mut project: Project = serde_json::from_str(&text).map_err(|err| {
            corrupt_record(&path, &format!("it is not a valid project record: {err}"))
        })?;
        project
            .validate()
            .map_err(|err| corrupt_record(&path, &err))?;
        // The directory name is the authoritative id. A record claiming a
        // different (even well-formed) id must never load: later operations
        // resolve paths by directory, so a mismatch would redirect them.
        if project.id != id {
            return Err(corrupt_record(
                &path,
                &format!(
                    "its id {:?} does not match the project directory it is stored in",
                    project.id
                ),
            ));
        }
        // Identity normalization (read-only): a record written before the
        // installed-revision pin existed carries a durable `builtImageId`
        // while its `installed` field is absent — the image id is the
        // ground truth for what exists in the container store, so the load
        // pins an `InstalledRevision` from the record's own working fields
        // and projects it onto `Installed`. Unknown build metadata stays
        // unknown: the pin carries no revision id and no fabricated
        // timestamp. Because the pin is derived only from durable fields,
        // the projection is stable across restarts without a migration:
        // the load never rewrites the file, and the next durable commit of
        // the record persists the pin on its own. A record with no built
        // image keeps its stored phase, so a genuine draft stays a draft.
        project.normalize_identity();
        Ok(project)
    }

    /// Atomically writes `project` over its record path (temp file plus
    /// rename inside the project directory), after full validation. The
    /// project directory must already exist; `save` never creates
    /// projects. The installed identity is normalized with
    /// [`Project::normalize_identity`] before serialization — a caller
    /// holding a legacy in-memory record (a built image id without a pin)
    /// persists the pin on this save, so later edits to the working fields
    /// can never blur the installed identity. The passed `project` is not
    /// modified; the normalized form is what reaches disk and every later
    /// load.
    pub fn save(&self, project: &Project) -> Result<(), String> {
        project.validate()?;
        let dir = self.project_dir(&project.id)?;
        if !dir.is_dir() {
            return Err(format!(
                "the project directory {} does not exist; only records of \
                 existing projects can be saved",
                dir.display()
            ));
        }
        let path = dir.join(RECORD_FILE);
        let mut record = project.clone();
        record.normalize_identity();
        let json = serde_json::to_string_pretty(&record)
            .map_err(|err| format!("project {} could not be serialized: {err}", project.id))?;
        // Unique temp name so concurrent or crashed saves can never
        // clobber a live record; the rename within the directory is atomic.
        let tmp = dir.join(format!("{}.{}.tmp", RECORD_FILE, new_id()));
        if let Err(err) = std::fs::write(&tmp, &json) {
            return Err(format!(
                "the record for project {} could not be staged at {}: {err}",
                project.id,
                tmp.display()
            ));
        }
        if let Err(err) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "the record for project {} could not be committed to {}: {err}",
                project.id,
                path.display()
            ));
        }
        Ok(())
    }

    /// All records under the root, sorted by id for a stable order. A
    /// missing root is an empty store. Anything under the root that is not
    /// exactly a valid project *directory* — including a symlink whose
    /// name looks like a valid id — is corruption and fails with an
    /// actionable error rather than being silently skipped or followed. Each
    /// record is read through [`Store::load`], so the installed identity
    /// normalization applies here too.
    pub fn list(&self) -> Result<Vec<Project>, String> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let unreadable = || {
            format!(
                "the project store at {} could not be read",
                self.root.display()
            )
        };
        let entries =
            std::fs::read_dir(&self.root).map_err(|err| format!("{}: {err}", unreadable()))?;
        let mut projects = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|err| format!("{}: {err}", unreadable()))?;
            // `to_string_lossy` keeps even non-UTF-8 names reportable; a
            // lossy name can never pass the id check, so it is an error.
            let id = entry.file_name().to_string_lossy().into_owned();
            if validate_project_id(&id).is_err() {
                return Err(format!(
                    "unexpected entry {id:?} in the project store at {}; it may \
                     contain only project directories, move it out and retry",
                    self.root.display()
                ));
            }
            // `DirEntry::file_type` never follows symlinks, so a symlink
            // (or stray file) is refused even when its name is a perfectly
            // valid id, before any record under it is read.
            let file_type = entry
                .file_type()
                .map_err(|err| format!("{}: {err}", unreadable()))?;
            if !file_type.is_dir() {
                return Err(format!(
                    "the entry {id:?} in the project store at {} is not a real \
                     directory; symlinks and stray files cannot be projects, move \
                     it out and retry",
                    self.root.display()
                ));
            }
            projects.push(self.load(&id)?);
        }
        projects.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(projects)
    }

    /// Hard-deletes one project: removes `<root>/<id>` whole, the record
    /// and the imported `source/` snapshot inside it included.
    ///
    /// The id goes through the same [`validate_project_id`] join guard as
    /// every other id-crossing method, so a hostile id fails before the
    /// filesystem is touched and no path outside the store root can ever be
    /// removed. The directory is then re-checked with lstat — the same guard
    /// [`Store::load`] uses — so a symlink or stray file wearing a valid id
    /// is refused instead of followed or deleted: only a real directory
    /// inside the store is ever unlinked, and only this project's.
    ///
    /// A directory that does not exist is success, which makes the removal
    /// idempotent and a retried delete a no-op. A removal that fails
    /// part-way reports the error and leaves whatever remains in place for
    /// the retry; nothing is renamed, rewritten, or cleaned up here.
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let dir = self.project_dir(id)?;
        let metadata = match std::fs::symlink_metadata(&dir) {
            Ok(metadata) => metadata,
            // Already gone: exactly the state a finished removal leaves,
            // so a retry of a completed delete succeeds without doing
            // anything at all.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(format!(
                    "the project directory {} could not be inspected for removal: {err}",
                    dir.display()
                ));
            }
        };
        // lstat never follows the final symlink, and a plain file is not a
        // project either: both are refused rather than removed, so a
        // hostile or stray entry can never turn a deletion into user-file
        // loss outside (or beside) the store.
        if !metadata.is_dir() {
            return Err(format!(
                "the project entry {} is not a real directory; symlinks and \
                 stray files cannot be projects, so nothing was removed. Move \
                 it aside with a file manager and retry",
                dir.display()
            ));
        }
        std::fs::remove_dir_all(&dir).map_err(|err| {
            format!(
                "the project directory {} could not be removed: {err}. Clear what \
                 is holding it and delete the project again; a retry is safe.",
                dir.display()
            )
        })
    }

    /// Directory holding one project's record and reserved source
    /// directory. The id is validated before the join.
    fn project_dir(&self, id: &str) -> Result<PathBuf, String> {
        validate_project_id(id)?;
        Ok(self.root.join(id))
    }

    #[cfg(test)]
    fn record_path(&self, id: &str) -> Result<PathBuf, String> {
        Ok(self.project_dir(id)?.join(RECORD_FILE))
    }
}

/// A freshly generated native project id: UUID v4 simple form, 32
/// lowercase hex characters.
fn new_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Actionable message for a corrupt record. Nothing is ever deleted or
/// rewritten automatically: the user decides how to clear it.
fn corrupt_record(path: &Path, detail: &str) -> String {
    format!(
        "the project record at {} is invalid: {detail}. Repair that file or move \
         it aside with a file manager, then retry; Pockets never deletes \
         project data automatically.",
        path.display()
    )
}

/// Project id shape: exactly 32 lowercase hex characters, matching the
/// simple UUID v4 form [`new_id`] generates. Shared by the id join guard
/// and the `editedFrom` parent check — one shape rule, one definition.
fn is_project_id(value: &str) -> bool {
    value.len() == 32
        && value
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, 'a'..='f'))
}

/// Native project id shape: exactly 32 lowercase hex characters, matching
/// the simple UUID v4 form [`new_id`] generates. Validated before any path
/// join so `..`, separators, or empty ids cannot escape the store root.
/// Crate-visible so the sibling-file surfaces ([`crate::thumb`]) join ids
/// onto the store root behind the same one shape rule.
pub(crate) fn validate_project_id(id: &str) -> Result<(), String> {
    if is_project_id(id) {
        Ok(())
    } else {
        Err(format!(
            "{id:?} is not a valid project id (32 lowercase hexadecimal \
             characters are required)"
        ))
    }
}

/// SHA-256 digest shape: exactly 64 lowercase hex characters.
fn validate_digest_hex(field: &str, value: &str) -> Result<(), String> {
    let valid = value.len() == 64
        && value
            .chars()
            .all(|ch| ch.is_ascii_digit() || matches!(ch, 'a'..='f'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{field} must be 64 lowercase hexadecimal characters, found {value:?}"
        ))
    }
}

/// Image reference: a `sha256:` prefix followed by a 64-character
/// lowercase hex image digest. `field` names the record field in errors.
fn validate_image_id(field: &str, value: &str) -> Result<(), String> {
    match value.strip_prefix("sha256:") {
        Some(digest) => validate_digest_hex(&format!("{field} digest"), digest),
        None => Err(format!(
            "{field} must be a \"sha256:\" image digest, found {value:?}"
        )),
    }
}

/// Shared summary validation for [`SourceSummary`] and [`Project`].
fn validate_summary(
    files: &[String],
    containerfile: &str,
    source_digest: &str,
) -> Result<(), String> {
    validate_digest_hex("sourceDigest", source_digest)?;
    if containerfile.is_empty() {
        return Err("containerfile must not be empty".to_owned());
    }
    for file in files {
        validate_relative_file("files entry", file)?;
    }
    Ok(())
}

/// Source-relative file path shape: forward-slash separated, normalized
/// (no empty, `.` or `..` segments), with no leading slash, no backslashes,
/// no colons (drive syntax or legacy separators), and no NUL bytes.
fn validate_relative_file(field: &str, value: &str) -> Result<(), String> {
    let path_error = || {
        format!(
            "{field} must be a normalized source-relative path without '.', \
             '..', absolute or drive prefixes, found {value:?}"
        )
    };
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains(':')
        || value.contains('\0')
    {
        return Err(path_error());
    }
    for segment in value.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(path_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;

    fn test_summary() -> SourceSummary {
        SourceSummary {
            files: vec!["app/main.go".to_owned(), "go.mod".to_owned()],
            total_bytes: 2048,
            containerfile: "FROM scratch\n".to_owned(),
            source_digest: "a".repeat(64),
        }
    }

    /// A canonical installed revision for `project`, pinning its current
    /// working manifest and source digest under `image_id`.
    fn pin_of(project: &Project, image_id: &str) -> InstalledRevision {
        InstalledRevision {
            image_id: image_id.to_owned(),
            manifest: project.manifest.clone(),
            source_digest: project.source_digest.clone(),
            revision_id: None,
            built_at_seconds: None,
        }
    }

    #[test]
    fn record_round_trips_across_restart() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root.clone());
        let manifest = manifest::bundled_manifest().expect("bundled manifest");

        // A store that was never written reads as empty.
        assert!(store.list().expect("empty list").is_empty());

        let mut project = store
            .create(manifest.clone(), test_summary())
            .expect("create must succeed");
        assert_eq!(project.phase, Phase::Draft);
        assert_eq!(project.built_image_id, None);
        assert_eq!(project.installed, None);
        assert!(!project.previewed);
        validate_project_id(&project.id).expect("generated id must be valid");
        assert!(store.source_path(&project.id).unwrap().is_dir());

        // Simulate a completed install and preview, then an app restart: a
        // brand-new Store over the same root must observe exactly what the
        // previous process saved, including phase, digests, and the full
        // installed revision with its optional metadata.
        let image_id = format!("sha256:{}", "b".repeat(64));
        project.phase = Phase::Installed;
        project.built_image_id = Some(image_id.clone());
        project.installed = Some(InstalledRevision {
            built_at_seconds: Some(1_700_000_000),
            revision_id: Some("rev-7".to_owned()),
            ..pin_of(&project, &image_id)
        });
        project.previewed = true;
        store.save(&project).expect("save must succeed");

        let reopened = Store::new(root);
        let id = project.id.clone();
        assert_eq!(reopened.load(&id).expect("load"), project);
        let summary = reopened.load(&id).unwrap().summary();
        assert_eq!(summary.source_digest, "a".repeat(64));
        assert_eq!(summary.total_bytes, 2048);
        assert_eq!(reopened.list().expect("list"), vec![project]);
    }

    /// A legacy record — a persisted `draft` with a durable `builtImageId`
    /// and no `installed` field — loads as a pinned `Installed` revision
    /// with unknown build metadata, without the read ever rewriting the
    /// file. The next durable save persists the pin.
    #[test]
    fn legacy_built_draft_loads_as_installed() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root.clone());
        let manifest = manifest::bundled_manifest().expect("bundled manifest");

        // Build the legacy wire shape by hand: records written before the
        // pin existed persisted a durable image id while still claiming
        // `draft`, with no `installed` field at all. `Store::save` now
        // normalizes, so the fixture is serialized straight to the record
        // path. Marker values make every other field observable below.
        let mut legacy = store
            .create(manifest.clone(), test_summary())
            .expect("create must succeed");
        let image_id = format!("sha256:{}", "b".repeat(64));
        legacy.phase = Phase::Draft;
        legacy.built_image_id = Some(image_id.clone());
        legacy.previewed = true;
        legacy.files = vec!["marker/main.go".to_owned()];
        legacy.total_bytes = 4096;
        legacy.containerfile = "FROM marker\n".to_owned();
        legacy.source_digest = "c".repeat(64);
        let record = store.record_path(&legacy.id).expect("record path");
        std::fs::write(
            &record,
            serde_json::to_string_pretty(&legacy).expect("legacy serialization"),
        )
        .expect("legacy record must persist");
        let before = std::fs::read_to_string(&record).expect("stored record");
        assert!(
            before.contains("\"phase\": \"draft\""),
            "the fixture must stay a persisted draft on disk: {before}"
        );
        assert!(
            !before.contains("\"installed\""),
            "the fixture must predate the installed pin: {before}"
        );

        // Both read paths report the durable image id as an installed,
        // pinned revision, and the projection is stable across a restart
        // with a brand-new Store.
        let mut expected = legacy.clone();
        expected.phase = Phase::Installed;
        expected.installed = Some(pin_of(&legacy, &image_id));
        let reopened = Store::new(root.clone());
        let loaded = reopened.load(&legacy.id).expect("legacy draft must load");
        assert_eq!(loaded, expected);
        assert_eq!(reopened.list().expect("list"), vec![expected]);

        // Nothing but the identity is projected: the id is never
        // regenerated and the image, preview and source fields come back
        // as stored.
        assert_eq!(loaded.phase, Phase::Installed);
        assert_eq!(loaded.id, legacy.id, "the id must never be regenerated");
        assert_eq!(loaded.built_image_id.as_deref(), Some(image_id.as_str()));
        assert!(loaded.previewed, "the preview marker must be preserved");
        assert_eq!(loaded.summary(), legacy.summary());

        // The pin takes the record's stored manifest and source digest as
        // the installed identity, and unknown build metadata stays unknown
        // — no revision id and never a fabricated timestamp.
        let pin = loaded.installed.clone().expect("load must pin the image");
        assert_eq!(pin.image_id, image_id);
        assert_eq!(pin.manifest, legacy.manifest);
        assert_eq!(pin.source_digest, legacy.source_digest);
        assert_eq!(pin.revision_id, None, "an unknown revision stays None");
        assert_eq!(
            pin.built_at_seconds, None,
            "an unknown build time is never fabricated"
        );
        assert_eq!(loaded.installed_manifest(), &legacy.manifest);
        assert!(
            !loaded.has_pending_changes(),
            "a freshly pinned record matches its own working source"
        );

        // Reading never rewrites the record: the raw JSON is byte-identical
        // to what was on disk before the load and the list.
        assert_eq!(
            std::fs::read_to_string(&record).expect("record after reads"),
            before,
            "load and list must not rewrite the legacy record"
        );

        // The next durable commit persists the normalized identity: the
        // pin and the installed phase land on disk, still with no invented
        // metadata, and a fresh load agrees.
        reopened.save(&loaded).expect("save must persist the pin");
        let after = std::fs::read_to_string(&record).expect("record after save");
        assert!(after.contains("\"installed\""), "got {after}");
        assert!(after.contains("\"phase\": \"installed\""), "got {after}");
        assert!(
            !after.contains("builtAtSeconds") && !after.contains("revisionId"),
            "unknown metadata must stay absent from the wire: {after}"
        );
        assert_eq!(reopened.load(&legacy.id).expect("pinned load"), loaded);

        // A record with no built image keeps its stored phase: a genuine
        // draft is still a draft after the same restart.
        let untouched = reopened
            .create(manifest, test_summary())
            .expect("second create");
        let draft = reopened.load(&untouched.id).expect("draft load");
        assert_eq!(
            draft.phase,
            Phase::Draft,
            "an unbuilt draft must not be projected onto installed"
        );
        assert_eq!(draft.installed, None);
    }

    /// A manually set up legacy in-memory record (built image id, no pin)
    /// gains its pin on the next `save`, before any later edit: subsequent
    /// working-manifest and working-digest changes can never blur the
    /// installed identity or break the wire mirror.
    #[test]
    fn save_pins_legacy_in_memory_record_before_later_edits() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root);
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let mut project = store
            .create(manifest, test_summary())
            .expect("create must succeed");

        // Exactly how older build code left the record in memory: a
        // durable image id and installed phase, but no revision pin.
        let image_id = format!("sha256:{}", "b".repeat(64));
        project.built_image_id = Some(image_id.clone());
        project.phase = Phase::Installed;
        store
            .save(&project)
            .expect("save must pin the legacy record");
        assert_eq!(
            project.installed, None,
            "save normalizes what it writes, it never mutates the caller's record"
        );

        // The written record already carries the pin at the pre-edit
        // identity, with unknown metadata left unknown.
        let loaded = store.load(&project.id).expect("pinned load");
        let pin = loaded.installed.clone().expect("save must pin");
        assert_eq!(pin.image_id, image_id);
        assert_eq!(pin.manifest, project.manifest);
        assert_eq!(pin.source_digest, project.source_digest);
        assert_eq!(pin.revision_id, None);
        assert_eq!(pin.built_at_seconds, None);
        assert_eq!(loaded.built_image_id.as_deref(), Some(image_id.as_str()));
        assert!(!loaded.has_pending_changes());

        // Later chat-style edits to the working source — manifest and
        // digest — persist, while the installed revision and the mirror
        // survive a save/load cycle untouched.
        let original_name = project.manifest.metadata.name.clone();
        let mut edited = loaded;
        edited.manifest.metadata.name = "Edited working name".to_owned();
        edited.files = vec!["app/edited.go".to_owned()];
        edited.source_digest = "d".repeat(64);
        store.save(&edited).expect("edited save must succeed");
        let reread = store.load(&project.id).expect("edited load");
        assert_eq!(reread.installed, Some(pin), "the pin is not re-derived");
        assert_eq!(reread.built_image_id.as_deref(), Some(image_id.as_str()));
        assert_eq!(reread.manifest.metadata.name, "Edited working name");
        assert_eq!(reread.source_digest, "d".repeat(64));
        assert_eq!(reread.installed_manifest().metadata.name, original_name);
        assert!(
            reread.has_pending_changes(),
            "a diverged working source is pending against the pin"
        );

        // Bringing the working digest back in line clears the pending flag
        // without touching the installed revision.
        let mut synced = reread.clone();
        synced.source_digest = reread.installed.as_ref().unwrap().source_digest.clone();
        store.save(&synced).expect("synced save must succeed");
        let final_record = store.load(&project.id).expect("synced load");
        assert!(!final_record.has_pending_changes());
        assert_eq!(final_record.installed, reread.installed);
    }

    /// `installed` is the authority: saving it with a missing mirror (or a
    /// stale draft phase) fills in `builtImageId` and the phase, while a
    /// different `builtImageId` is refused as an ambiguity, never silently
    /// resolved in either direction.
    #[test]
    fn installed_is_authoritative_and_conflicts_are_refused() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root);
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let mut project = store
            .create(manifest, test_summary())
            .expect("create must succeed");
        let image_id = format!("sha256:{}", "b".repeat(64));

        // A caller that only sets `installed` gets the mirror and the
        // phase on disk even though memory claimed `Draft` with no id.
        project.installed = Some(pin_of(&project, &image_id));
        store.save(&project).expect("installed-only save");
        let loaded = store.load(&project.id).expect("load after install");
        assert_eq!(loaded.phase, Phase::Installed);
        assert_eq!(loaded.built_image_id.as_deref(), Some(image_id.as_str()));

        // Two different Some values are ambiguous: `save` refuses before
        // anything reaches the filesystem.
        let mut conflicted = loaded.clone();
        conflicted.built_image_id = Some(format!("sha256:{}", "e".repeat(64)));
        let err = store
            .save(&conflicted)
            .expect_err("a conflicting mirror must be refused");
        assert!(
            err.contains("conflicts with installed.imageId"),
            "got {err}"
        );
        assert_eq!(
            store
                .load(&project.id)
                .expect("refused save changed nothing"),
            loaded
        );
    }

    /// Corrupt installed metadata — malformed image ids, digests,
    /// manifests, revision ids, unknown fields, or a contradicting mirror —
    /// fails with an actionable error on both read paths instead of being
    /// skipped, defaulted, or migrated away.
    #[test]
    fn invalid_installed_metadata_is_refused_not_migrated() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root);
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let mut project = store
            .create(manifest, test_summary())
            .expect("create must succeed");
        let image_id = format!("sha256:{}", "b".repeat(64));
        project.phase = Phase::Installed;
        project.built_image_id = Some(image_id.clone());
        project.installed = Some(InstalledRevision {
            revision_id: Some("rev-1".to_owned()),
            built_at_seconds: Some(1_700_000_000),
            ..pin_of(&project, &image_id)
        });
        store.save(&project).expect("install must persist");

        let record = store.record_path(&project.id).expect("record path");
        let good: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&record).expect("stored record"))
                .expect("stored record is JSON");
        assert_eq!(store.load(&project.id).expect("baseline load"), project);

        let cases: Vec<(&str, fn(&mut serde_json::Value))> = vec![
            (
                "installed.imageId digest must be 64 lowercase hexadecimal",
                |v| v["installed"]["imageId"] = serde_json::json!("sha256:deadbeef"),
            ),
            (
                "installed.imageId must be a \"sha256:\" image digest",
                |v| v["installed"]["imageId"] = serde_json::json!("deadbeef"),
            ),
            (
                "installed.sourceDigest must be 64 lowercase hexadecimal",
                |v| v["installed"]["sourceDigest"] = serde_json::json!("not-a-digest"),
            ),
            ("installed.revisionId must not be empty", |v| {
                v["installed"]["revisionId"] = serde_json::json!("")
            }),
            ("web.port must be between 1 and 65535", |v| {
                v["installed"]["manifest"]["web"]["port"] = serde_json::json!(0)
            }),
            ("unknown field `builtAt`", |v| {
                v["installed"]["builtAt"] = serde_json::json!(1_700_000_000)
            }),
            ("conflicts with installed.imageId", |v| {
                v["builtImageId"] = serde_json::json!(format!("sha256:{}", "9".repeat(64)))
            }),
        ];
        for (expected_message, tamper) in cases {
            let mut bad = good.clone();
            tamper(&mut bad);
            std::fs::write(&record, bad.to_string()).expect("tampered record");
            let err = store
                .load(&project.id)
                .expect_err("tampered installed metadata must not load");
            assert!(
                err.contains(expected_message),
                "expected {expected_message:?}, got {err}"
            );
            assert!(
                store.list().is_err(),
                "list must report the corrupt record, never skip it"
            );
        }

        // Repairing the record restores both read paths, byte-for-byte.
        std::fs::write(&record, good.to_string()).expect("repaired record");
        assert_eq!(store.load(&project.id).expect("repaired load"), project);
        assert_eq!(store.list().expect("repaired list"), vec![project]);
    }

    /// Wire compatibility for drafts: an uninstalled record omits
    /// `installed` entirely, loads the field as `None` by default, and the
    /// helpers report a draft as always pending with no installed manifest
    /// of its own.
    #[test]
    fn draft_records_omit_installed_and_default_to_none() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root);
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let project = store
            .create(manifest, test_summary())
            .expect("create must succeed");

        let raw = std::fs::read_to_string(store.record_path(&project.id).expect("record path"))
            .expect("stored record");
        assert!(
            !raw.contains("installed"),
            "a draft must not serialize the installed field: {raw}"
        );

        let loaded = store.load(&project.id).expect("draft load");
        assert_eq!(loaded, project);
        assert_eq!(loaded.installed, None);
        assert_eq!(loaded.built_image_id, None);
        assert_eq!(loaded.phase, Phase::Draft);
        assert_eq!(
            loaded.installed_manifest(),
            &loaded.manifest,
            "a draft has nothing but its working manifest"
        );
        assert!(
            loaded.has_pending_changes(),
            "a never-built draft always has a first revision to install"
        );
    }

    /// `editedFrom` is a schema addition, not a migration: a `None`
    /// lineage is absent from the wire (so legacy records written without
    /// the key keep loading), a `Some` lineage round-trips through save
    /// and load spelled exactly `editedFrom`, and a malformed parent is
    /// refused on validate, on save, and on both read paths.
    #[test]
    fn edited_from_round_trips_and_defaults_to_none() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root);
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let project = store
            .create(manifest, test_summary())
            .expect("create must succeed");
        let record = store.record_path(&project.id).expect("record path");

        // A fresh draft knows no parent, in memory or on the wire: the
        // key is absent entirely, exactly like a pre-field legacy record,
        // which is why those legacy records load unchanged.
        assert_eq!(project.edited_from, None);
        let raw = std::fs::read_to_string(&record).expect("stored record");
        assert!(
            !raw.contains("editedFrom"),
            "a parentless record must not serialize the lineage key: {raw}"
        );
        assert_eq!(
            store.load(&project.id).expect("legacy load").edited_from,
            None,
            "a record without the key loads as None, never a guessed parent"
        );

        // Some round-trips through a durable save with the exact wire
        // spelling.
        let parent = "f".repeat(32);
        let mut edited = project.clone();
        edited.edited_from = Some(parent.clone());
        store.save(&edited).expect("save with lineage");
        let raw = std::fs::read_to_string(&record).expect("record after save");
        assert!(
            raw.contains(&format!("\"editedFrom\": \"{parent}\"")),
            "the wire name must be editedFrom with the parent id: {raw}"
        );
        assert_eq!(store.load(&project.id).expect("lineage load"), edited);
        let with_lineage: serde_json::Value =
            serde_json::from_str(&raw).expect("lineage record is JSON");

        // And deleting the key from that same record — the byte shape a
        // pre-field writer produced — still loads, as None.
        let mut keyless = with_lineage.clone();
        keyless
            .as_object_mut()
            .expect("record object")
            .remove("editedFrom");
        std::fs::write(&record, keyless.to_string()).expect("legacy rewrite");
        assert_eq!(store.load(&project.id).expect("keyless load"), project);

        // Malformed parents fail validate before the filesystem, and fail
        // save without touching the stored record.
        let good_record = std::fs::read_to_string(&record).expect("baseline bytes");
        for bad in [
            "",
            &"F".repeat(32),
            &"a".repeat(31),
            &"a".repeat(33),
            &"z".repeat(32),
        ] {
            let mut broken = project.clone();
            broken.edited_from = Some(bad.to_owned());
            let err = broken
                .validate()
                .expect_err("a malformed editedFrom must not validate");
            assert!(err.contains("editedFrom"), "got {err}");
            let err = store
                .save(&broken)
                .expect_err("a malformed editedFrom must not persist");
            assert!(err.contains("editedFrom"), "got {err}");
            assert_eq!(
                std::fs::read_to_string(&record).expect("bytes after refusal"),
                good_record,
                "a refused save may not rewrite the record"
            );
        }

        // A record tampered on disk is refused by both read paths, and
        // repairing the value restores them.
        for bad in ["", &"A".repeat(32), &"a".repeat(64)] {
            let mut tampered = with_lineage.clone();
            tampered["editedFrom"] = serde_json::json!(bad);
            std::fs::write(&record, tampered.to_string()).expect("tampered record");
            let err = store
                .load(&project.id)
                .expect_err("a malformed stored editedFrom must not load");
            assert!(err.contains("editedFrom"), "got {err}");
            assert!(
                store.list().is_err(),
                "list must report the corrupt lineage, never skip it"
            );
        }
        std::fs::write(&record, with_lineage.to_string()).expect("repaired record");
        assert_eq!(store.load(&project.id).expect("repaired load"), edited);
    }

    /// Removal owns exactly one path: `<root>/<id>`, everything inside it
    /// included. It is idempotent, and it never reaches a sibling.
    #[test]
    fn remove_deletes_the_whole_project_directory_and_nothing_else() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root.clone());
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let doomed = store
            .create(manifest.clone(), test_summary())
            .expect("first create");
        let sibling = store
            .create(manifest, test_summary())
            .expect("second create");

        // The doomed project owns both halves of its directory: the record
        // and a populated source snapshot.
        let doomed_dir = root.join(&doomed.id);
        std::fs::write(
            store
                .source_path(&doomed.id)
                .expect("source path")
                .join("main.go"),
            b"package main",
        )
        .expect("source snapshot fixture");
        assert!(store
            .record_path(&doomed.id)
            .expect("record path")
            .is_file());

        store.remove(&doomed.id).expect("removal must succeed");
        assert!(
            !doomed_dir.exists(),
            "the record and its source snapshot must go together"
        );

        // A retry of a completed removal is success: an absent directory is
        // precisely the state removal leaves behind.
        store
            .remove(&doomed.id)
            .expect("removing an absent project is idempotent");

        // The sibling survives on disk, through `load`, and in the listing.
        assert!(store
            .record_path(&sibling.id)
            .expect("sibling record path")
            .is_file());
        assert_eq!(store.load(&sibling.id).expect("sibling"), sibling);
        assert_eq!(store.list().expect("list after removal"), vec![sibling]);
    }

    /// A hostile id fails the same join guard as every other id-crossing
    /// method before the filesystem is touched, and an entry that is not a
    /// real directory inside the root is refused rather than deleted.
    #[test]
    fn remove_rejects_hostile_ids_and_entries_that_are_not_projects() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root.clone());
        std::fs::create_dir_all(&root).expect("store root");
        let canary = dir.path().join("canary.txt");
        std::fs::write(&canary, b"keep me").expect("canary file");

        for id in [
            "",
            "..",
            "../outside",
            "a/b",
            ".hidden",
            &"A".repeat(32), // uppercase, not lowercase hex
            &"z".repeat(32), // lowercase but not hex
            &"a".repeat(31),
            &"a".repeat(33),
        ] {
            let err = store.remove(id).expect_err("{id} must never be removed");
            assert!(err.contains("valid project id"), "{id:?} got {err}");
        }
        assert!(canary.exists(), "a refused id must never reach the root");

        // A stray file wearing a perfectly valid id is not a project: it is
        // reported and left exactly where it is, never unlinked.
        let stray_id = "e".repeat(32);
        let stray = root.join(&stray_id);
        std::fs::write(&stray, b"not a project").expect("stray file");
        let err = store
            .remove(&stray_id)
            .expect_err("a stray file is not removable as a project");
        assert!(err.contains("not a real directory"), "got {err}");
        assert!(stray.is_file(), "a stray file must be left in place");

        // The same guard covers a symlink pointing out of the store: lstat
        // never follows it, so nothing outside the root can be destroyed
        // through a project id.
        #[cfg(unix)]
        {
            let outside = dir.path().join("outside");
            std::fs::create_dir(&outside).expect("outside dir");
            let link_id = "d".repeat(32);
            std::os::unix::fs::symlink(&outside, root.join(&link_id)).expect("symlink fixture");
            let err = store
                .remove(&link_id)
                .expect_err("a symlinked project directory must be refused");
            assert!(err.contains("not a real directory"), "got {err}");
            assert!(outside.is_dir(), "the linked directory must survive");
        }
    }

    #[test]
    fn invalid_ids_and_path_escapes_are_rejected() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let store = Store::new(dir.path().join("projects"));

        let bad_ids = [
            "",
            "..",
            "../outside",
            "a/b",
            ".hidden",
            "Aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", // uppercase, not lowercase hex
            &"z".repeat(32),                    // lowercase but not hex
            &"a".repeat(31),
            &"a".repeat(33),
        ];
        for id in bad_ids {
            assert!(store.load(id).is_err(), "{id:?} must not load");
            assert!(
                store.source_path(id).is_err(),
                "{id:?} must never join a path"
            );
        }

        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let escaping = SourceSummary {
            files: vec!["../escape".to_owned()],
            ..test_summary()
        };
        assert!(Project::new(manifest.clone(), escaping).is_err());
        let absolute = SourceSummary {
            files: vec!["/etc/passwd".to_owned()],
            ..test_summary()
        };
        assert!(Project::new(manifest.clone(), absolute).is_err());
        let bad_digest = SourceSummary {
            source_digest: "not-a-digest".to_owned(),
            ..test_summary()
        };
        assert!(Project::new(manifest, bad_digest).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn tampered_ids_and_symlinked_dirs_are_refused() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root.clone());
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let first = store
            .create(manifest.clone(), test_summary())
            .expect("first create");
        let second = store
            .create(manifest, test_summary())
            .expect("second create");

        // Tamper: rewrite the stored record to claim the other project's
        // id. The forged id is well-formed, so only the directory match
        // check can catch it.
        let record = store.record_path(&second.id).expect("record path");
        let mut tampered: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&record).expect("stored record"))
                .expect("stored record is JSON");
        tampered["id"] = serde_json::Value::String(first.id.clone());
        std::fs::write(&record, tampered.to_string()).expect("tamper must apply");

        assert!(
            store.load(&second.id).is_err(),
            "a record whose id disagrees with its directory must not load"
        );
        assert!(
            store.list().is_err(),
            "list must report the tampered record, never skip it"
        );
        assert_eq!(
            store.load(&first.id).expect("untouched record"),
            first,
            "the untampered project must still load"
        );

        // Repair to a clean baseline so only the symlink can fail list.
        tampered["id"] = serde_json::Value::String(second.id.clone());
        std::fs::write(&record, tampered.to_string()).expect("repair must apply");
        assert_eq!(store.list().expect("list after repair").len(), 2);

        // A symlink outside the store carries a record whose id matches
        // the link name, so following it would pass every other check;
        // list must refuse it without following, even with a valid id name.
        let alias_id = "f".repeat(32);
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).expect("outside dir");
        let alias_json = std::fs::read_to_string(&record)
            .expect("record")
            .replace(&second.id, &alias_id);
        std::fs::write(outside.join(RECORD_FILE), alias_json).expect("alias record");
        std::os::unix::fs::symlink(&outside, root.join(&alias_id)).expect("symlink fixture");
        assert!(
            store.list().is_err(),
            "a symlinked directory must be refused, not followed"
        );
        // Commands load by id directly, so load must refuse the same
        // symlink on its own (the alias record's id matches the link name,
        // so following it would otherwise load successfully).
        assert!(
            store.load(&alias_id).is_err(),
            "load must refuse a symlinked project directory"
        );
    }
}
