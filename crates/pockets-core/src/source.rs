//! Native snapshot import and build-context staging.
//!
//! Import copies a user-selected folder into a project's managed
//! `source/` snapshot inside the [`Store`]. The original host path is
//! never persisted: only the [`SourceSummary`] description lives in the
//! record, and every later read goes through the store-managed copy.
//!
//! Capture rules are fixed here, not configurable and not accepted from
//! the frontend:
//!
//! - The root must contain the regular files `pocket.yaml` and
//!   `Containerfile`; the manifest is parsed and host-validated from the
//!   exact captured bytes, and the Containerfile shown to the UI comes
//!   from the same bytes the digest covers.
//! - Any entry with a `.git`, `node_modules`, `venv`, `__pycache__`, `data`,
//!   `dist` or `target` component, hidden entries (except the root
//!   `.containerignore`, `.dockerignore` and `.gitignore`), credential extensions
//!   (`.pem`, `.key`, `.p12`, `.pfx`) and the names `credentials.json`, `id_rsa`
//!   and `id_ed25519` are excluded before traversal — never stat'ed,
//!   opened or copied, so user-local data can never enter a snapshot.
//! - Symlinks and special files are rejected, and on Unix included
//!   files with more than one hard link are rejected.
//! - Limits: at most 2000 regular files, 5 MiB each, 50 MiB total,
//!   relative depth 16, and root `pocket.yaml`/`Containerfile` at most
//!   128 KiB.
//! - Relative paths are normalized UTF-8 in the exact shape
//!   `projects::Project` records accept.
//!
//! The snapshot digest is a canonical SHA-256 over each sorted relative
//! path length (u64 BE), path bytes, file length (u64 BE), and file
//! bytes, so the digest commits to paths and content together.
//!
//! Import reads the whole snapshot into memory first, allocates the
//! record with [`Store::create`] only afterwards, and on a copy failure
//! removes exactly the newly allocated project directory: no runtime
//! data exists yet, and the store never keeps a half-imported project.
//!
//! [`prepare_build_context`] stages a build context for an existing
//! record by re-reading only `record.files` from the managed source,
//! verifying the recorded digest, total bytes, Containerfile and parsed
//! manifest, and copying exactly that approved list into a fresh
//! `context/` directory under the caller-provided output directory.
//! `selected_dir` and `output_dir` come from native code (the future
//! native picker and the build slice), never from the frontend.
//!
//! [`refresh_draft_snapshot`] re-captures an existing **draft** record's
//! managed source from a native workspace folder — the same importer
//! approval path as a first import, never a filesystem copy — and updates
//! the same record in place, staging and swapping the snapshot so a
//! handled failure never leaves the recorded digest and the managed tree
//! disagreeing. Installed or built records are refused without any file
//! effect.
//!
//! The revision-history entry points [`refresh_working_snapshot`] and
//! [`restore_working_snapshot`] (used only by [`crate::revisions`]) accept
//! a draft **or an installed** record: the working source updates in
//! place while the installed pin — phase, built image id, preview marker,
//! installed metadata and the pinned storage mount — survives every edit
//! or restore unchanged. They are working-source-only operations: no
//! build, container, data volume or image is ever touched.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::manifest::{parse_manifest, Manifest};
use crate::projects::{Phase, Project, SourceSummary, Store};

/// Root manifest file a Pocket folder must provide.
const MANIFEST_FILE: &str = "pocket.yaml";
/// Root Containerfile a Pocket folder must provide.
const CONTAINERFILE: &str = "Containerfile";
/// Directory `prepare_build_context` creates inside the output directory.
const CONTEXT_DIR: &str = "context";

/// At most this many regular files per snapshot.
const MAX_FILES: usize = 2_000;
/// At most this many bytes per snapshotted file.
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// At most this many bytes across a whole snapshot.
const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
/// Deepest allowed relative path (component count) in a snapshot.
const MAX_DEPTH: usize = 16;
/// Root manifest and Containerfile are bounded much tighter than data.
const MAX_ROOT_CONFIG_BYTES: u64 = 128 * 1024;

/// Directory or file components excluded (with everything under them)
/// before traversal, at any depth.
const EXCLUDED_COMPONENTS: [&str; 7] = [
    ".git",
    "node_modules",
    "venv",
    "__pycache__",
    "data",
    "dist",
    "target",
];
/// Hidden entries stay out of snapshots, except these root ignore files.
const ROOT_IGNORE_EXCEPTIONS: [&str; 3] = [".containerignore", ".dockerignore", ".gitignore"];
/// Credential file names excluded at any depth.
const CREDENTIAL_NAMES: [&str; 3] = ["credentials.json", "id_rsa", "id_ed25519"];
/// Credential extensions excluded at any depth (case-insensitive).
const CREDENTIAL_EXTENSIONS: [&str; 4] = ["pem", "key", "p12", "pfx"];

/// Imports a Pocket folder into `store`: captures the approved snapshot
/// in memory first, parses and host-validates the manifest from the
/// captured bytes, then allocates the record with [`Store::create`] and
/// populates its managed `source/` directory. A copy failure removes
/// exactly the newly allocated project directory, leaving the store
/// without a corrupt entry.
///
/// `selected_dir` must be a real directory supplied by native code (the
/// future native picker), never a frontend-provided path.
pub fn import_project(store: &Store, selected_dir: &Path) -> Result<Project, String> {
    // Read everything before allocating anything: failures while
    // capturing leave the store completely untouched.
    let (files, manifest, summary) = capture_approved(selected_dir, "the selected folder")?;
    let project = store.create(manifest, summary)?;

    let source_dir = store.source_path(&project.id)?;
    if let Err(err) = write_snapshot(&source_dir, &files) {
        // The project directory was allocated by the create just above
        // and nothing else exists for it yet, so removing exactly that
        // directory restores the store to its pre-import state.
        if let Some(project_dir) = source_dir.parent() {
            let _ = std::fs::remove_dir_all(project_dir);
        }
        return Err(err);
    }
    Ok(project)
}

/// The importer's full approval path, shared verbatim by
/// [`import_project`] and [`refresh_draft_snapshot`]: `root` must be a
/// real folder (never a symlink), the snapshot is captured under the
/// fixed exclusion and limit rules, and the manifest is parsed and
/// host-validated from the exact captured bytes, so the summary's
/// Containerfile and digest cover exactly what was approved. `what`
/// names the folder in user-facing errors.
pub(crate) fn capture_approved(
    root: &Path,
    what: &str,
) -> Result<(Vec<(String, Vec<u8>)>, Manifest, SourceSummary), String> {
    let root_meta = std::fs::symlink_metadata(root)
        .map_err(|err| format!("{what} {} could not be read: {err}", root.display()))?;
    if root_meta.file_type().is_symlink() || !root_meta.file_type().is_dir() {
        return Err(format!(
            "{} is not a folder; select the Pocket's real directory",
            root.display()
        ));
    }
    let files = capture_snapshot(root)?;

    let yaml = utf8_file(&files, MANIFEST_FILE)
        .ok_or_else(|| format!("{what} must contain a {MANIFEST_FILE} file at its root"))?;
    let manifest = parse_manifest(yaml)?;
    manifest.validate_host()?;
    let containerfile_bytes = utf8_file(&files, CONTAINERFILE)
        .map(str::to_owned)
        .ok_or_else(|| format!("{what} must contain a {CONTAINERFILE} file at its root"))?;
    if containerfile_bytes.is_empty() {
        return Err(format!("the root {CONTAINERFILE} must not be empty"));
    }

    let total_bytes = files
        .iter()
        .map(|(_, bytes)| bytes.len() as u64)
        .sum::<u64>();
    let summary = SourceSummary {
        files: files.iter().map(|(path, _)| path.clone()).collect(),
        total_bytes,
        // The UI-reviewed Containerfile is exactly the digested bytes.
        containerfile: containerfile_bytes,
        source_digest: snapshot_digest(&files),
    };
    Ok((files, manifest, summary))
}

/// Replaces the managed `source/` snapshot of the draft `project_id` with
/// a fresh capture of `workspace_dir`, reusing the importer's exact
/// capture and approval rules — never a filesystem copy — and updates the
/// **same** record in place: the id, manifest-independent identity, phase
/// and preview marker are preserved, the `editedFrom` lineage carries
/// unchanged (never re-derived from the workspace), and no duplicate
/// project is ever created. Only an unbuilt draft refreshes: a record that is installed,
/// or that carries a durable built image id, is refused before the
/// filesystem is touched. This function has no installed-lifecycle
/// authority — it never builds, installs, or changes phase.
///
/// Ordering and recovery for every managed-source replacement lives in
/// [`stage_swap_and_save`].
///
/// `workspace_dir` comes from native code (the agent workspace), never
/// from the frontend.
#[cfg(test)]
pub fn refresh_draft_snapshot(
    store: &Store,
    project_id: &str,
    workspace_dir: &Path,
) -> Result<Project, String> {
    // `load` refuses hostile ids, corrupt records and symlinked project
    // directories; its legacy projection means a record carrying a built
    // image id arrives as `Installed` even if its stored phase lies.
    let current = store.load(project_id)?;
    if current.phase != Phase::Draft || current.built_image_id.is_some() {
        return Err(format!(
            "project {} is installed or built and its source snapshot is fixed; \
             only an unbuilt draft can be refreshed from a workspace",
            current.id
        ));
    }
    apply_workspace_snapshot(store, &current, workspace_dir)
}

/// The revision-history refresh entry point, used by
/// [`crate::revisions::publish_working`]: re-captures the managed source
/// of `project_id` from a native workspace folder through the exact same
/// importer approval path as [`refresh_draft_snapshot`], but a draft **or
/// an installed** record is accepted. Installed lifecycle state is pinned,
/// never advanced: the id, phase, built image id, preview marker and
/// installed metadata all survive byte-identically, and so does any
/// `editedFrom` lineage: a working-source refresh neither invents nor
/// clears it. The storage
/// volume of an installed Pocket may not move
/// ([`enforce_storage_pinned`]). No duplicate project is ever created,
/// and a project that was deleted is never recreated: `load` refuses it
/// first. Builds nothing; touches no container, volume or image.
///
/// `workspace_dir` comes from native code (the agent workspace), never
/// from the frontend.
#[cfg(test)]
pub(crate) fn refresh_working_snapshot(
    store: &Store,
    project_id: &str,
    workspace_dir: &Path,
) -> Result<Project, String> {
    let current = store.load(project_id)?;
    apply_workspace_snapshot(store, &current, workspace_dir)
}

/// Installs an already-verified archived snapshot — `archived`, captured
/// and verified by [`crate::revisions::restore_working`] against
/// `snapshot` — as the managed working source of `project_id`, through
/// the same staged swap as [`refresh_working_snapshot`]. Only source
/// fields change: the id, phase, built image id, preview marker and
/// installed metadata of the current record survive unchanged, and the
/// pinned storage of an installed Pocket may not move. This is a
/// **working-source-only** operation: no container, data volume or image
/// is touched, because restoring bytes is not an installed-lifecycle
/// action. A project that no longer exists is refused by `load`, never
/// recreated.
pub(crate) fn restore_working_snapshot(
    store: &Store,
    project_id: &str,
    snapshot: &Project,
    archived: Vec<(String, Vec<u8>)>,
) -> Result<Project, String> {
    let current = store.load(project_id)?;
    enforce_storage_pinned(&current, &snapshot.manifest)?;
    let mut updated = current.clone();
    updated.manifest = snapshot.manifest.clone();
    updated.files = snapshot.files.clone();
    updated.total_bytes = snapshot.total_bytes;
    updated.containerfile = snapshot.containerfile.clone();
    updated.source_digest = snapshot.source_digest.clone();
    updated.validate()?;
    stage_swap_and_save(store, &current, &archived, &updated)
}

/// Shared workspace-capture path of both refresh entry points: the new
/// snapshot is captured whole in memory through the importer's exact
/// approval path before anything is written, so failures here leave the
/// store completely untouched.
#[cfg(test)]
fn apply_workspace_snapshot(
    store: &Store,
    current: &Project,
    workspace_dir: &Path,
) -> Result<Project, String> {
    let (files, manifest, summary) = capture_approved(workspace_dir, "the workspace folder")?;
    apply_captured_snapshot(store, current, files, manifest, summary)
}

/// Publishes bytes already captured through `capture_approved`. External
/// development uses this to validate identity and publish the exact same
/// capture, even when a harness keeps changing files on disk.
pub(crate) fn apply_captured_snapshot(
    store: &Store,
    current: &Project,
    files: Vec<(String, Vec<u8>)>,
    manifest: Manifest,
    summary: SourceSummary,
) -> Result<Project, String> {
    enforce_storage_pinned(current, &manifest)?;
    let updated = updated_source(current, &manifest, &summary)?;
    stage_swap_and_save(store, current, &files, &updated)
}

/// The candidate record: the same identity and lifecycle fields — phase,
/// built image id, preview marker and installed pin included — carrying
/// the new approved source summary and manifest. The whole record is
/// cloned and only source fields are replaced, so the `editedFrom`
/// lineage travels byte-identically through every refresh: a refresh
/// rewrites working bytes, it never invents, re-derives, or clears where
/// a draft was edited from.
fn updated_source(
    current: &Project,
    manifest: &Manifest,
    summary: &SourceSummary,
) -> Result<Project, String> {
    let mut updated = current.clone();
    updated.manifest = manifest.clone();
    updated.files = summary.files.clone();
    updated.total_bytes = summary.total_bytes;
    updated.containerfile = summary.containerfile.clone();
    updated.source_digest = summary.source_digest.clone();
    updated.validate()?;
    Ok(updated)
}

/// While a Pocket is installed its data volume is mounted from the
/// installed manifest: an edited source may change anything **except**
/// the storage name and path, or the running volume would silently be
/// remounted somewhere else. The installed metadata itself (digest,
/// manifest, image id) is pinned structurally — every refresh clones it
/// unchanged. An unbuilt draft has nothing mounted and stays freely
/// editable. Crate-shared with [`crate::revisions`], which checks the
/// same pin before archiving work around a restore.
pub(crate) fn enforce_storage_pinned(
    current: &Project,
    new_manifest: &Manifest,
) -> Result<(), String> {
    // A genuine draft has nothing mounted. Any other record is installed
    // or built: [`Store::load`] normalization pins legacy records into
    // `installed`, and [`Project::installed_manifest`] otherwise falls
    // back to the working manifest — exactly what the volume was mounted
    // from while no installed metadata existed.
    if current.installed.is_none()
        && current.built_image_id.is_none()
        && current.phase == Phase::Draft
    {
        return Ok(());
    }
    let pinned = current.installed_manifest();
    if new_manifest.storage.name != pinned.storage.name
        || new_manifest.storage.path != pinned.storage.path
    {
        return Err(format!(
            "project {} is installed: its storage volume {:?} mounted at {:?} is \
             pinned while installed, but the edited source changes storage to {:?} \
             at {:?}. Restore the installed storage name and path; Pockets never \
             remounts existing data somewhere else",
            current.id,
            pinned.storage.name,
            pinned.storage.path,
            new_manifest.storage.name,
            new_manifest.storage.path
        ));
    }
    Ok(())
}

/// The staged swap shared by every managed-source replacement. Ordering
/// and recovery: the previous managed snapshot is first verified against
/// the record, so a corrupt baseline is reported untouched rather than
/// swapped away; `files` are then staged in a fresh private directory
/// beside `source/` and re-verified against the candidate `updated`
/// record; the swap is two renames inside the project directory; and the
/// record is saved last, rolling the previous snapshot back in if the
/// save fails. Every handled failure either never touched the store or
/// restores the previous snapshot, so a returned `Err` never leaves the
/// recorded digest and the managed tree disagreeing. A process killed
/// mid-swap is not a handled error: every later snapshot check (build
/// staging, workspace seeding, or refresh) refuses the mismatched pair.
/// Recovery then requires restoring the previous snapshot from its
/// backup; a new refresh must not silently bless a corrupt baseline.
fn stage_swap_and_save(
    store: &Store,
    current: &Project,
    files: &[(String, Vec<u8>)],
    updated: &Project,
) -> Result<Project, String> {
    // The previous managed snapshot must still reproduce the record
    // before it can be swapped out: refresh never papers over corruption
    // of the authoritative bytes.
    let source_dir = store.source_path(&current.id)?;
    let previous = capture_record_files(&source_dir, &current.files)?;
    verify_against_record("the managed source", &previous, current)?;

    let project_dir = source_dir.parent().ok_or_else(|| {
        format!(
            "the project directory for {} could not be derived",
            current.id
        )
    })?;

    // Stage the whole new snapshot beside `source/` — same filesystem, so
    // the swap below is a rename — and re-verify it from disk exactly
    // like the build staging does, before anything existing is moved.
    let tag = Uuid::new_v4().simple().to_string();
    let staging = project_dir.join(format!("source.refresh-{tag}"));
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&staging)
    };
    created.map_err(|err| {
        format!(
            "the staging directory {} could not be created: {err}",
            staging.display()
        )
    })?;
    if let Err(err) = write_snapshot(&staging, files)
        .and_then(|()| capture_record_files(&staging, &updated.files))
        .and_then(|staged| verify_against_record("the staged snapshot", &staged, updated))
    {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err);
    }

    // Swap with two renames, keeping the old snapshot as a backup until
    // the record is durable. A failed swap renames the backup back; the
    // record has not been touched at this point.
    let backup = project_dir.join(format!("source.previous-{tag}"));
    if let Err(err) = std::fs::rename(&source_dir, &backup) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "the snapshot of project {} could not be set aside: {err}; nothing was \
             changed",
            current.id
        ));
    }
    if let Err(err) = std::fs::rename(&staging, &source_dir) {
        let restored = std::fs::rename(&backup, &source_dir);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(match restored {
            Ok(()) => format!(
                "the refreshed snapshot of project {} could not be installed: {err}; \
                 the previous snapshot is back in place and nothing was changed",
                current.id
            ),
            Err(restore_err) => format!(
                "the refreshed snapshot of project {} could not be installed: {err}; \
                 and the previous snapshot at {} could not be restored: {restore_err}. \
                 The record was not changed; repair the project directory with a file \
                 manager and retry",
                current.id,
                backup.display()
            ),
        });
    }

    // Persist last: if the record cannot be saved, roll the previous
    // snapshot back in so the stored record and the managed tree keep
    // agreeing with each other.
    if let Err(err) = store.save(&updated) {
        let abandoned = project_dir.join(format!("source.rejected-{tag}"));
        let restored = std::fs::rename(&source_dir, &abandoned)
            .and_then(|()| std::fs::rename(&backup, &source_dir));
        return Err(match restored {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(&abandoned);
                format!(
                    "the refreshed record for project {} could not be saved: {err}; \
                     the previous snapshot is back in place and nothing was changed",
                    current.id
                )
            }
            Err(restore_err) => format!(
                "the refreshed record for project {} could not be saved: {err}; and \
                 the previous snapshot at {} could not be restored: {restore_err}. \
                 Repair the project directory with a file manager and retry",
                current.id,
                backup.display()
            ),
        });
    }
    let _ = std::fs::remove_dir_all(&backup);
    Ok(updated.clone())
}

/// Stages a build context for `project` at `output_dir/context` and
/// returns that directory. Only `record.files` are re-read, exclusively
/// from the store-managed source copy; symlinks, hard links, path
/// escapes and size violations are rejected. Before the call succeeds
/// the staged bytes are verified to reproduce the recorded snapshot
/// digest, total bytes, Containerfile and parsed manifest exactly.
/// Extra files sitting in the managed source are ignored because only
/// the approved list is ever copied. A failure never leaves a staging
/// directory behind.
///
/// `output_dir` comes from native code (the build slice), never from
/// the frontend.
pub fn prepare_build_context(
    store: &Store,
    project: &Project,
    output_dir: &Path,
) -> Result<PathBuf, String> {
    project.validate()?;
    let source_dir = store.source_path(&project.id)?;

    // Managed source must still match the record before anything is
    // staged; failures here create nothing.
    let approved = capture_record_files(&source_dir, &project.files)?;
    verify_against_record("the managed source", &approved, project)?;

    let context = output_dir.join(CONTEXT_DIR);
    std::fs::create_dir_all(output_dir).map_err(|err| {
        format!(
            "the output directory {} could not be created: {err}",
            output_dir.display()
        )
    })?;
    // Exclusive: an existing context is never silently reused.
    if let Err(err) = std::fs::create_dir(&context) {
        if err.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(format!(
                "the staging directory {} already exists; choose an empty \
                 output directory and retry",
                context.display()
            ));
        }
        return Err(format!(
            "the staging directory {} could not be created: {err}",
            context.display()
        ));
    }

    // Only the verified bytes are written, then re-read from disk so
    // success means the context provably matches the record. The
    // directory was created exclusively above, so cleanup on failure
    // can only ever remove this staging attempt.
    if let Err(err) = write_snapshot(&context, &approved)
        .and_then(|()| capture_record_files(&context, &project.files))
        .and_then(|written| verify_against_record("the staged context", &written, project))
    {
        let _ = std::fs::remove_dir_all(&context);
        return Err(err);
    }
    Ok(context)
}

/// Re-reads exactly `files` (record-relative paths) from `dir`, one
/// regular non-link file each, sorted like a snapshot. `dir` itself must
/// be a real directory, every ancestor component of a recorded path
/// must be an ordinary directory, and any deviation — missing file,
/// symlink (even one pointing inside `dir`), hard link, size limit
/// breach — is an error; nothing is ever read from outside `dir`.
/// Native-only and crate-shared: [`crate::ai::workspace`] verifies a
/// managed snapshot with it before seeding an iteration workspace.
pub(crate) fn capture_record_files(
    dir: &Path,
    files: &[String],
) -> Result<Vec<(String, Vec<u8>)>, String> {
    // The base itself must be a real directory before it is
    // canonicalized: canonicalizing first would follow a planted
    // symlink at the root and approve whatever it points at.
    let root_meta = std::fs::symlink_metadata(dir)
        .map_err(|err| format!("{} could not be opened: {err}", dir.display()))?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err(format!(
            "{} is a symbolic link or not a directory; the snapshot root \
             must be a real directory",
            dir.display()
        ));
    }
    let root = dir
        .canonicalize()
        .map_err(|err| format!("{} could not be opened: {err}", dir.display()))?;
    let mut captured: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;
    for rel in files {
        validate_relative("record file", rel)?;
        let joined = root.join(rel);
        // Every ancestor component must be an ordinary directory. A
        // symbolic link anywhere on the path is refused even when it
        // targets bytes inside the root: the snapshot contract records
        // ordinary files under ordinary directories only, so the
        // recorded path always identifies exactly one file.
        let segments = rel.split('/').collect::<Vec<_>>();
        let mut ancestor = root.clone();
        let mut ancestor_rel = String::new();
        for segment in &segments[..segments.len() - 1] {
            if !ancestor_rel.is_empty() {
                ancestor_rel.push('/');
            }
            ancestor_rel.push_str(segment);
            ancestor.push(segment);
            let meta = std::fs::symlink_metadata(&ancestor)
                .map_err(|err| format!("{ancestor_rel} could not be inspected: {err}"))?;
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "{ancestor_rel} is a symbolic link; recorded paths must \
                     live under ordinary directories"
                ));
            }
            if !meta.is_dir() {
                return Err(format!("{ancestor_rel} is not a directory"));
            }
        }
        // Final component: lstat refuses a file that is itself a link;
        // the ancestor walk above makes the join containment-complete.
        let meta = std::fs::symlink_metadata(&joined)
            .map_err(|err| format!("{rel} could not be read: {err}"))?;
        if meta.file_type().is_symlink() {
            return Err(format!("{rel} is a symbolic link, not a regular file"));
        }
        if !meta.file_type().is_file() {
            return Err(format!("{rel} is not a regular file"));
        }
        reject_hardlink(&meta, rel)?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(format!(
                "{rel} is {} bytes, above the {MAX_FILE_BYTES} byte limit",
                meta.len()
            ));
        }
        let bytes = read_capped(&joined, MAX_FILE_BYTES)
            .map_err(|err| format!("{rel} could not be read: {err}"))?;
        total = total
            .checked_add(bytes.len() as u64)
            .filter(|sum| *sum <= MAX_TOTAL_BYTES)
            .ok_or_else(|| format!("the recorded files exceed {MAX_TOTAL_BYTES} bytes"))?;
        captured.push((rel.clone(), bytes));
    }
    captured.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(captured)
}

/// Checks that captured `files` reproduce `project`'s recorded total
/// bytes, snapshot digest, Containerfile and parsed manifest exactly.
/// The host architecture is deliberately not re-checked: records must
/// stay readable across machines, and the manifest already passed that
/// check at import time. Crate-shared with [`crate::ai::workspace`],
/// which verifies both the managed source and its seeded copy with it.
pub(crate) fn verify_against_record(
    what: &str,
    files: &[(String, Vec<u8>)],
    project: &Project,
) -> Result<(), String> {
    let total = files
        .iter()
        .map(|(_, bytes)| bytes.len() as u64)
        .sum::<u64>();
    if total != project.total_bytes {
        return Err(format!(
            "{what} of project {} totals {total} bytes but the record says {}",
            project.id, project.total_bytes
        ));
    }
    let digest = snapshot_digest(files);
    if digest != project.source_digest {
        return Err(format!(
            "{what} of project {} no longer matches its recorded snapshot \
             digest; the stored files were modified",
            project.id
        ));
    }
    let containerfile = utf8_file(files, CONTAINERFILE).ok_or_else(|| {
        format!(
            "{what} of project {} has no readable {CONTAINERFILE}",
            project.id
        )
    })?;
    if containerfile != project.containerfile {
        return Err(format!(
            "{what} of project {} has a different {CONTAINERFILE} than the record",
            project.id
        ));
    }
    let yaml = utf8_file(files, MANIFEST_FILE).ok_or_else(|| {
        format!(
            "{what} of project {} has no readable {MANIFEST_FILE}",
            project.id
        )
    })?;
    let parsed = parse_manifest(yaml).map_err(|err| {
        format!(
            "{what} of project {} carries an invalid manifest: {err}",
            project.id
        )
    })?;
    if parsed != project.manifest {
        return Err(format!(
            "{what} of project {} carries a different manifest than the record",
            project.id
        ));
    }
    Ok(())
}
/// Captures the approved snapshot of `root` as sorted
/// (relative path, bytes) pairs. Exclusions are decided before any
/// stat or open of the excluded contents.
fn capture_snapshot(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    capture_dir(root, "", 0, &mut files, &mut total)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Cheap host-side change detection. Apply the exact snapshot exclusions
/// before statting entries; inspect metadata rather than reading source on
/// every watcher tick. Full capture/validation still precedes execution.
pub(crate) fn source_signature(root: &Path) -> Result<String, String> {
    fn walk(
        root: &Path,
        rel: &str,
        depth: usize,
        entries: &mut Vec<(String, std::fs::Metadata)>,
        total: &mut u64,
        directories: &mut usize,
    ) -> Result<(), String> {
        if depth > MAX_DEPTH || *directories > MAX_FILES {
            return Err("Source tree exceeds its directory limits.".into());
        }
        for item in std::fs::read_dir(root.join(rel)).map_err(|e| e.to_string())? {
            let item = item.map_err(|e| e.to_string())?;
            let file_name = item.file_name();
            if is_excluded(&file_name.to_string_lossy(), rel.is_empty()) {
                continue;
            }
            let name = file_name
                .into_string()
                .map_err(|_| "Source paths must be UTF-8.")?;
            validate_relative("source entry", &name)?;
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let meta = std::fs::symlink_metadata(root.join(&path)).map_err(|e| e.to_string())?;
            if meta.is_dir() {
                *directories += 1;
                walk(root, &path, depth + 1, entries, total, directories)?;
            } else if meta.is_file() {
                reject_hardlink(&meta, &path)?;
                *total = total
                    .checked_add(meta.len())
                    .ok_or("Source size overflow.")?;
                if entries.len() >= MAX_FILES {
                    return Err(format!(
                        "This folder holds more than {MAX_FILES} files (excluding .git, \
                         node_modules, venv, __pycache__, dist, target, and data)."
                    ));
                }
                if meta.len() > MAX_FILE_BYTES {
                    return Err(format!(
                        "{path} is larger than 5 MiB ({:.1} MiB). Move large files or binaries \
                         out of source (or into data/).",
                        meta.len() as f64 / (1024.0 * 1024.0)
                    ));
                }
                if *total > MAX_TOTAL_BYTES {
                    return Err(format!(
                        "Total source size exceeds 50 MiB (reached at {path})."
                    ));
                }
                entries.push((path, meta));
            } else {
                return Err(format!("{path} is not a regular source file or directory."));
            }
        }
        Ok(())
    }
    if !std::fs::symlink_metadata(root)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("Source root must be a real directory.".into());
    }
    let mut entries = Vec::new();
    walk(root, "", 0, &mut entries, &mut 0, &mut 0)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hash = Sha256::new();
    for (path, meta) in entries {
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update(meta.len().to_le_bytes());
        hash.update(
            meta.modified()
                .map_err(|e| e.to_string())?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos()
                .to_le_bytes(),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            hash.update(meta.ino().to_le_bytes());
            hash.update(meta.dev().to_le_bytes());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// One directory of the recursive walk. `rel` is the already-captured
/// relative path of `dir` (`""` for the root) and `depth` its component
/// count; entries found here sit at `depth + 1`.
fn capture_dir(
    root: &Path,
    rel: &str,
    depth: usize,
    files: &mut Vec<(String, Vec<u8>)>,
    total: &mut u64,
) -> Result<(), String> {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let entries = std::fs::read_dir(&dir)
        .map_err(|err| format!("{} could not be read: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("{} could not be read: {err}", dir.display()))?;
        // Exclusions first: excluded entries are never stat'ed, opened
        // or copied, at any depth.
        let lossy = entry.file_name().to_string_lossy().into_owned();
        if is_excluded(&lossy, rel.is_empty()) {
            continue;
        }
        let name = entry.file_name().into_string().map_err(|_| {
            format!(
                "{} holds the name {lossy:?}, which is not valid UTF-8; \
                 rename it and retry",
                dir.display()
            )
        })?;
        validate_relative("entry name", &name).map_err(|_| {
            format!(
                "the entry {name:?} in {} cannot become a snapshot path; \
                 rename it and retry",
                dir.display()
            )
        })?;
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let child_depth = depth + 1;
        if child_depth > MAX_DEPTH {
            return Err(format!(
                "{child_rel} is more than {MAX_DEPTH} directories deep; \
                 flatten the folder and retry"
            ));
        }
        let path = root.join(&child_rel);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|err| format!("{child_rel} could not be inspected: {err}"))?;
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            return Err(format!(
                "{child_rel} is a symbolic link; replace it with a regular \
                 file or remove it, then retry"
            ));
        }
        if file_type.is_dir() {
            capture_dir(root, &child_rel, child_depth, files, total)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(format!(
                "{child_rel} is neither a regular file nor a directory; \
                 remove it and retry"
            ));
        }
        reject_hardlink(&meta, &child_rel)?;
        if files.len() >= MAX_FILES {
            return Err(format!(
                "the selected folder holds more than {MAX_FILES} files; \
                 remove files and retry"
            ));
        }
        let cap = if child_depth == 1 && (name == MANIFEST_FILE || name == CONTAINERFILE) {
            MAX_ROOT_CONFIG_BYTES
        } else {
            MAX_FILE_BYTES
        };
        if meta.len() > cap {
            return Err(format!(
                "{child_rel} is {} bytes, above the {cap} byte limit",
                meta.len()
            ));
        }
        let bytes = read_capped(&path, cap)
            .map_err(|err| format!("{child_rel} could not be read: {err}"))?;
        *total = total
            .checked_add(bytes.len() as u64)
            .filter(|sum| *sum <= MAX_TOTAL_BYTES)
            .ok_or_else(|| {
                format!("the selected folder exceeds the {MAX_TOTAL_BYTES} byte import limit")
            })?;
        files.push((child_rel, bytes));
    }
    Ok(())
}

/// Whether `name` is excluded before traversal: an excluded component,
/// a hidden entry (except the three root ignore files), a credential
/// name, or a credential extension (case-insensitive).
fn is_excluded(name: &str, at_root: bool) -> bool {
    if EXCLUDED_COMPONENTS.contains(&name) {
        return true;
    }
    if name.starts_with('.') {
        return !(at_root && ROOT_IGNORE_EXCEPTIONS.contains(&name));
    }
    if CREDENTIAL_NAMES.contains(&name) {
        return true;
    }
    match name.rsplit_once('.') {
        Some((_, extension)) => {
            CREDENTIAL_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        }
        None => false,
    }
}

/// The exact normalized relative-path shape [`crate::projects::Project`]
/// records require, checked here before anything is ever captured.
fn validate_relative(field: &str, value: &str) -> Result<(), String> {
    let error = || {
        format!(
            "{field} must be a normalized source-relative path without \
             '.', '..', absolute or drive prefixes, found {value:?}"
        )
    };
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains(':')
        || value.contains('\0')
    {
        return Err(error());
    }
    for segment in value.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(error());
        }
    }
    Ok(())
}

/// Included files must be unique inode references: a link count above 1 means
/// another path shares the bytes, so the import refuses the folder.
fn reject_hardlink(meta: &std::fs::Metadata, rel: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if meta.nlink() > 1 {
        return Err(format!(
            "{rel} is a hard link to another file; break the link and retry"
        ));
    }
    Ok(())
}

/// Reads at most `cap + 1` bytes so a lying metadata length or a file
/// growing mid-read can never exhaust memory; exceeding `cap` is an
/// error. Native-only and crate-shared: [`crate::revisions`] bounds every
/// revision metadata read with it.
pub(crate) fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|err| format!("{} could not be read: {err}", path.display()))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("{} could not be read: {err}", path.display()))?;
    if bytes.len() as u64 > cap {
        return Err(format!("{} is above the {cap} byte limit", path.display()));
    }
    Ok(bytes)
}

/// The captured bytes of `want` decoded as UTF-8, or `None` when the
/// file is absent or not valid UTF-8.
fn utf8_file<'f>(files: &'f [(String, Vec<u8>)], want: &str) -> Option<&'f str> {
    files
        .iter()
        .find(|(path, _)| path == want)
        .and_then(|(_, bytes)| std::str::from_utf8(bytes).ok())
}

/// Canonical snapshot digest: SHA-256 over each sorted relative path
/// length (u64 BE), path bytes, file length (u64 BE), and file bytes.
/// Paths and content are committed together, in sorted order.
fn snapshot_digest(files: &[(String, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    for (path, bytes) in files {
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Writes captured files under `target`, creating parent directories.
/// Native-only and crate-shared: [`crate::revisions`] writes only
/// byte-for-byte verified captures with it, into its private staging
/// directory.
pub(crate) fn write_snapshot(target: &Path, files: &[(String, Vec<u8>)]) -> Result<(), String> {
    for (rel, bytes) in files {
        let path = target.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("{} could not be created: {err}", parent.display()))?;
        }
        std::fs::write(&path, bytes).map_err(|err| format!("{rel} could not be written: {err}"))?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal manifest that parses and supports both supported host
    /// architectures.
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

    /// A Pocket folder carrying everything the capture rules must keep
    /// out: hidden entries, credential files, and user/build/VCS
    /// directories, next to the three files that must be captured.
    fn sample_source() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("temp source");
        let root = dir.path();
        write_file(root, MANIFEST_FILE, TEST_MANIFEST);
        write_file(root, CONTAINERFILE, "FROM scratch\n");
        write_file(root, ".gitignore", "build/\n");
        write_file(root, "app/main.go", "package main\n");
        // Must never reach the snapshot:
        write_file(root, ".env", "TOKEN=secret\n");
        write_file(root, "server.pem", "PRIVATE KEY\n");
        write_file(root, "keys/api.key", "key bytes\n");
        write_file(root, "bundle.P12", "pkcs12 bytes\n");
        write_file(root, "credentials.json", "{\"api\": \"secret\"}\n");
        write_file(root, "secrets/id_ed25519", "private key\n");
        write_file(root, "data/users.db", "user rows\n");
        write_file(root, "repo/.git/config", "[core]\n");
        write_file(root, "web/node_modules/left-pad/index.js", "// vendored\n");
        write_file(root, "web/dist/app.js", "// bundled\n");
        write_file(root, "cache/target/artifact", "// built\n");
        dir
    }

    #[test]
    fn import_excludes_secrets_and_survives_original_edits() {
        let source = sample_source();
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let store = Store::new(workspace.path().join("store"));

        let project = import_project(&store, source.path()).expect("import must succeed");
        assert_eq!(
            project.files,
            vec![
                ".gitignore".to_owned(),
                CONTAINERFILE.to_owned(),
                "app/main.go".to_owned(),
                MANIFEST_FILE.to_owned(),
            ]
        );
        assert_eq!(project.containerfile, "FROM scratch\n");
        assert_eq!(
            project.total_bytes,
            b"build/\n".len() as u64
                + b"FROM scratch\n".len() as u64
                + b"package main\n".len() as u64
                + TEST_MANIFEST.len() as u64
        );
        assert!(store.list().expect("list").contains(&project));

        let managed = store.source_path(&project.id).expect("managed source");
        assert_eq!(
            std::fs::read(managed.join("app/main.go")).expect("managed copy"),
            b"package main\n"
        );
        for excluded in [
            ".env",
            "server.pem",
            "keys/api.key",
            "bundle.P12",
            "credentials.json",
            "secrets/id_ed25519",
            "data/users.db",
            "repo/.git/config",
            "web/node_modules/left-pad/index.js",
            "web/dist/app.js",
            "cache/target/artifact",
        ] {
            assert!(
                !managed.join(excluded).exists(),
                "{excluded} must not be copied"
            );
        }

        // Editing the original folder afterwards must not disturb
        // anything: the managed snapshot and a fresh build context keep
        // exactly the bytes captured at import time.
        write_file(source.path(), "app/main.go", "package edited\n");
        write_file(source.path(), "late.txt", "added after import\n");
        let out = tempfile::TempDir::new().expect("temp output");
        let out_dir = out.path().join("build");
        let context =
            prepare_build_context(&store, &project, &out_dir).expect("staging must succeed");
        assert_eq!(context, out_dir.join(CONTEXT_DIR));
        assert_eq!(
            std::fs::read(context.join("app/main.go")).expect("staged copy"),
            b"package main\n",
            "the context must hold the imported bytes, not the later edit"
        );
        assert!(!context.join("late.txt").exists());
    }

    #[test]
    #[cfg(unix)]
    fn included_symlink_is_rejected_and_nothing_is_allocated() {
        let source = sample_source();
        std::os::unix::fs::symlink(
            source.path().join("app/main.go"),
            source.path().join("link.go"),
        )
        .expect("create symlink");
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let root = workspace.path().join("store");
        let store = Store::new(root.clone());

        let err = import_project(&store, source.path()).expect_err("symlink must be rejected");
        assert!(err.contains("symbolic link"), "unexpected error: {err}");
        // Capture happens before allocation, so the store never learns
        // of this import attempt at all.
        assert!(store.list().expect("empty list").is_empty());
        assert!(!root.exists(), "no store directory may be allocated");
    }

    #[test]
    #[cfg(unix)]
    fn staging_refuses_a_modified_managed_source() {
        let source = sample_source();
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let store = Store::new(workspace.path().join("store"));
        let project = import_project(&store, source.path()).expect("import must succeed");

        // Same-size tampering so the recorded digest is what rejects it.
        let managed = store.source_path(&project.id).expect("managed source");
        std::fs::write(managed.join("app/main.go"), b"packagx main\n").expect("tamper");

        let out = tempfile::TempDir::new().expect("temp output");
        let out_dir = out.path().join("build");
        let err = prepare_build_context(&store, &project, &out_dir)
            .expect_err("a modified managed source must be refused");
        assert!(err.contains("digest"), "unexpected error: {err}");
        assert!(
            !out_dir.join(CONTEXT_DIR).exists(),
            "a refusal must not stage anything"
        );

        // A managed `source/` replaced by a symbolic link is refused
        // outright: the root is lstat'ed as a real directory before it
        // is canonicalized, so a planted link can never be followed and
        // approved as the snapshot root, whatever it points at.
        std::fs::remove_dir_all(&managed).expect("remove managed source");
        std::os::unix::fs::symlink(workspace.path().join("elsewhere"), &managed)
            .expect("plant root symlink");
        let err = prepare_build_context(&store, &project, &out_dir)
            .expect_err("a symlinked managed source must be refused");
        assert!(
            err.contains("symbolic link") && err.contains("real directory"),
            "unexpected error: {err}"
        );
        assert!(
            !out_dir.join(CONTEXT_DIR).exists(),
            "a refusal must not stage anything"
        );
    }

    /// The names inside a project directory after a clean operation: the
    /// record and the snapshot, with no staging or backup leftovers.
    fn project_dir_entries(store: &Store, id: &str) -> Vec<String> {
        let project_dir = store
            .source_path(id)
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
        names
    }

    /// The renamed manifest a next-iteration workspace carries.
    fn iteration_manifest() -> String {
        TEST_MANIFEST.replace("name: Demo Pocket", "name: Demo Pocket v2")
    }

    /// A next-iteration workspace folder: the manifest renamed, the
    /// Containerfile extended, `app/main.go` rewritten, and the old
    /// `.gitignore` dropped.
    fn iteration_source() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("temp iteration");
        let root = dir.path();
        write_file(root, MANIFEST_FILE, &iteration_manifest());
        write_file(root, CONTAINERFILE, "FROM scratch\nRUN echo once\n");
        write_file(root, "app/main.go", "package iteration\n");
        dir
    }

    #[test]
    fn refresh_updates_the_same_draft_in_place() {
        let source = sample_source();
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let store = Store::new(workspace.path().join("store"));
        let project = import_project(&store, source.path()).expect("import must succeed");

        // A previewed draft: identity and the preview marker must survive
        // every refresh unchanged.
        let mut previewed = project.clone();
        previewed.previewed = true;
        store.save(&previewed).expect("persist preview marker");

        let iteration = iteration_source();
        let refreshed = refresh_draft_snapshot(&store, &project.id, iteration.path())
            .expect("refresh of an unbuilt draft must succeed");

        assert_eq!(
            refreshed.id, project.id,
            "the same record is updated, never duplicated"
        );
        assert!(refreshed.previewed, "the preview marker survives");
        assert_eq!(refreshed.phase, Phase::Draft);
        assert_eq!(refreshed.built_image_id, None);
        assert_eq!(refreshed.manifest.metadata.name, "Demo Pocket v2");
        assert_eq!(
            refreshed.files,
            vec![
                CONTAINERFILE.to_owned(),
                "app/main.go".to_owned(),
                MANIFEST_FILE.to_owned(),
            ],
            "the recorded files are exactly the new workspace"
        );
        assert_eq!(refreshed.containerfile, "FROM scratch\nRUN echo once\n");
        assert_eq!(
            refreshed.total_bytes,
            b"FROM scratch\nRUN echo once\n".len() as u64
                + b"package iteration\n".len() as u64
                + iteration_manifest().len() as u64,
            "the dropped .gitignore is out of the total"
        );

        let stored = store.load(&project.id).expect("load after refresh");
        assert_eq!(stored, refreshed, "the record on disk is the update");
        assert_eq!(store.list().expect("list").len(), 1, "no duplicate");

        // The managed snapshot holds the new bytes and none of the old.
        let managed = store.source_path(&project.id).expect("managed source");
        assert_eq!(
            std::fs::read(managed.join("app/main.go")).expect("refreshed copy"),
            b"package iteration\n"
        );
        assert!(!managed.join(".gitignore").exists());
        assert_eq!(
            project_dir_entries(&store, &project.id),
            vec!["project.json".to_owned(), "source".to_owned()],
            "a successful refresh leaves no staging or backup directories"
        );

        // A repeated refresh of the unchanged workspace is idempotent.
        let again = refresh_draft_snapshot(&store, &project.id, iteration.path())
            .expect("a repeat refresh must also succeed");
        assert_eq!(again, refreshed);
        assert_eq!(store.list().expect("list").len(), 1);

        // The refreshed draft still stages a build context from its new
        // bytes, proving record and managed tree agree.
        let out = tempfile::TempDir::new().expect("temp output");
        let context = prepare_build_context(&store, &stored, &out.path().join("build"))
            .expect("staging after a refresh must succeed");
        assert_eq!(
            std::fs::read(context.join("app/main.go")).expect("staged copy"),
            b"package iteration\n"
        );
    }

    #[test]
    fn refresh_refuses_installed_or_built_projects_without_touching_anything() {
        let source = sample_source();
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let root = workspace.path().join("store");
        let store = Store::new(root.clone());
        let project = import_project(&store, source.path()).expect("import must succeed");
        let record = root.join(&project.id).join("project.json");
        let managed = store.source_path(&project.id).expect("managed source");

        let mut installed = project.clone();
        installed.phase = Phase::Installed;
        installed.built_image_id = Some(format!("sha256:{}", "b".repeat(64)));
        store.save(&installed).expect("install the record");
        let raw = std::fs::read_to_string(&record).expect("installed record");

        // The refusal precedes even locating the workspace folder, so a
        // path that does not exist still reports the lifecycle refusal.
        let err = refresh_draft_snapshot(&store, &project.id, &workspace.path().join("missing"))
            .expect_err("an installed project must never refresh");
        assert!(
            err.contains("installed or built"),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&record).expect("record after refusal"),
            raw,
            "the refusal must not rewrite the record"
        );
        assert_eq!(
            std::fs::read(managed.join("app/main.go")).expect("managed bytes"),
            b"package main\n",
            "the refusal must not touch the snapshot"
        );

        // Legacy shape: a record that still claims `draft` on disk but
        // carries a durable built image id loads as installed and is
        // refused by the same guard.
        let mut legacy = installed.clone();
        legacy.phase = Phase::Draft;
        store
            .save(&legacy)
            .expect("persist legacy draft-with-image");
        let err = refresh_draft_snapshot(&store, &project.id, iteration_source().path())
            .expect_err("a built image id fixes the snapshot regardless of stored phase");
        assert!(
            err.contains("installed or built"),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::read(managed.join("app/main.go")).expect("managed bytes"),
            b"package main\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn refresh_failures_never_damage_the_authoritative_snapshot() {
        let source = sample_source();
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let root = workspace.path().join("store");
        let store = Store::new(root.clone());
        let project = import_project(&store, source.path()).expect("import must succeed");
        let record = root.join(&project.id).join("project.json");
        let raw = std::fs::read_to_string(&record).expect("record");
        let managed = store.source_path(&project.id).expect("managed source");

        // An unsafe file in the workspace folder aborts the capture —
        // the exact exclusion/rejection rules of the importer apply.
        let unsafe_dir = tempfile::TempDir::new().expect("temp unsafe");
        write_file(unsafe_dir.path(), MANIFEST_FILE, TEST_MANIFEST);
        write_file(unsafe_dir.path(), CONTAINERFILE, "FROM scratch\n");
        std::os::unix::fs::symlink(
            workspace.path().join("elsewhere.secret"),
            unsafe_dir.path().join("escape.go"),
        )
        .expect("symlink fixture");
        let err = refresh_draft_snapshot(&store, &project.id, unsafe_dir.path())
            .expect_err("a symlinked workspace file must be refused");
        assert!(err.contains("symbolic link"), "unexpected error: {err}");

        // A folder without a manifest is refused too.
        let empty = tempfile::TempDir::new().expect("temp empty");
        let err = refresh_draft_snapshot(&store, &project.id, empty.path())
            .expect_err("a workspace without a manifest must be refused");
        assert!(err.contains(MANIFEST_FILE), "unexpected error: {err}");

        // After both refusals the store is byte-for-byte untouched.
        assert_eq!(store.load(&project.id).expect("load"), project);
        assert_eq!(
            std::fs::read_to_string(&record).expect("record after refusals"),
            raw
        );
        assert_eq!(
            std::fs::read(managed.join("app/main.go")).expect("snapshot bytes"),
            b"package main\n"
        );
        assert_eq!(
            project_dir_entries(&store, &project.id),
            vec!["project.json".to_owned(), "source".to_owned()],
            "a refused refresh leaves nothing behind"
        );

        // A managed snapshot tampered against its recorded digest is a
        // corrupt baseline: refresh reports it and changes nothing, so
        // the record and the (tampered) files are exactly as found.
        std::fs::write(managed.join("app/main.go"), b"packagx main\n").expect("tamper");
        let iteration = iteration_source();
        let err = refresh_draft_snapshot(&store, &project.id, iteration.path())
            .expect_err("a corrupt baseline must be refused");
        assert!(err.contains("digest"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read(managed.join("app/main.go")).expect("tampered bytes stay"),
            b"packagx main\n",
            "refresh never silently rewrites corruption"
        );
        assert_eq!(
            std::fs::read_to_string(&record).expect("record after corrupt baseline"),
            raw,
            "the record is never rewritten over a corrupt baseline"
        );
        assert_eq!(
            project_dir_entries(&store, &project.id),
            vec!["project.json".to_owned(), "source".to_owned()],
            "a refused refresh leaves nothing behind"
        );
    }

    /// A record pinned to a built image, with the metadata worker's
    /// installed pin recording the manifest and digest it was installed
    /// from.
    fn install_fixture(store: &Store, project: &Project) -> Project {
        use crate::projects::InstalledRevision;

        let image = format!("sha256:{}", "b".repeat(64));
        let mut installed = project.clone();
        installed.phase = Phase::Installed;
        installed.built_image_id = Some(image.clone());
        installed.previewed = true;
        installed.installed = Some(InstalledRevision {
            image_id: image,
            manifest: project.manifest.clone(),
            source_digest: project.source_digest.clone(),
            revision_id: None,
            built_at_seconds: Some(1_700_000_000),
        });
        store
            .save(&installed)
            .expect("persist the installed fixture");
        // The legacy projection reports the fixture as installed, exactly
        // as every later read path will.
        store.load(&project.id).expect("installed fixture loads")
    }

    #[test]
    fn refresh_working_updates_installed_in_place_with_pinned_state() {
        let source = sample_source();
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let store = Store::new(workspace.path().join("store"));
        let project = import_project(&store, source.path()).expect("import must succeed");
        let installed = install_fixture(&store, &project);

        // A next-iteration workspace renames the display name only: the
        // storage contract is untouched, so the installed pin allows it.
        let iteration = iteration_source();
        let refreshed = refresh_working_snapshot(&store, &project.id, iteration.path())
            .expect("an installed project's working source refreshes in place");

        assert_eq!(refreshed.id, project.id, "the same record, never a copy");
        assert_eq!(refreshed.phase, Phase::Installed);
        assert_eq!(
            refreshed.built_image_id, installed.built_image_id,
            "the built image id is pinned through an edit"
        );
        assert_eq!(
            refreshed.installed, installed.installed,
            "the installed pin is byte-identical after an edit"
        );
        assert!(refreshed.previewed, "the preview marker survives");
        assert_eq!(refreshed.manifest.metadata.name, "Demo Pocket v2");
        assert_eq!(
            store.list().expect("list").len(),
            1,
            "no project was created"
        );

        let managed = store.source_path(&project.id).expect("managed source");
        assert_eq!(
            std::fs::read(managed.join("app/main.go")).expect("refreshed copy"),
            b"package iteration\n",
            "the working source carries the new bytes"
        );
        assert_eq!(
            project_dir_entries(&store, &project.id),
            vec!["project.json".to_owned(), "source".to_owned()],
            "a successful installed refresh leaves no staging or backup directories"
        );
    }

    #[test]
    fn refresh_working_refuses_to_move_installed_storage() {
        let source = sample_source();
        let workspace = tempfile::TempDir::new().expect("temp workspace");
        let store = Store::new(workspace.path().join("store"));
        let project = import_project(&store, source.path()).expect("import must succeed");
        let installed = install_fixture(&store, &project);
        let managed = store.source_path(&project.id).expect("managed source");

        // The same source, but the manifest moves the volume elsewhere.
        let moved = tempfile::TempDir::new().expect("temp moved");
        write_file(
            moved.path(),
            MANIFEST_FILE,
            &TEST_MANIFEST.replace("path: /data", "path: /data2"),
        );
        write_file(moved.path(), CONTAINERFILE, "FROM scratch\n");
        write_file(moved.path(), "app/main.go", "package moved\n");

        let err = refresh_working_snapshot(&store, &project.id, moved.path())
            .expect_err("moving an installed Pocket's storage must be refused");
        assert!(err.contains("pinned"), "unexpected error: {err}");
        assert_eq!(
            store.load(&project.id).expect("record after refusal"),
            installed,
            "the refusal must not rewrite the record"
        );
        assert_eq!(
            std::fs::read(managed.join("app/main.go")).expect("snapshot bytes"),
            b"package main\n",
            "the refusal must not touch the snapshot"
        );
        assert_eq!(
            project_dir_entries(&store, &project.id),
            vec!["project.json".to_owned(), "source".to_owned()],
            "a refused refresh leaves nothing behind"
        );

        // An unbuilt draft has no mounted volume: the identical storage
        // change stays freely editable for it.
        let draft_home = tempfile::TempDir::new().expect("temp draft home");
        let draft_store = Store::new(draft_home.path().join("store"));
        let draft = import_project(&draft_store, source.path()).expect("draft import");
        let refreshed = refresh_working_snapshot(&draft_store, &draft.id, moved.path())
            .expect("a draft may change its storage");
        assert_eq!(refreshed.manifest.storage.path, "/data2");
    }
}
