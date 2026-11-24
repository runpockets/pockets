//! Command boundary for the project list, the native import flow, and
//! the single-action build orchestration.
//!
//! These commands are the webview's creation-side boundary to the
//! file-backed [`projects::Store`] (the lifecycle commands only ever
//! toggle a record's preview flag); the frontend never supplies a path,
//! a tag, or build arguments anywhere here: the only build-related input
//! accepted from the webview is a project id that merely selects an
//! existing native record. Everything a build touches — manifest,
//! Containerfile, snapshot files, context directory, image tag, output
//! directory — is derived natively from that record and the store
//! layout. The store root is fixed natively to
//! `app_data_dir().join("projects")` and [`project_store`] is the single
//! place that resolves it, so every slice roots its store handle at
//! exactly the same directory.
//!
//! Every filesystem traversal or write runs on Tauri's blocking pool via
//! [`tauri::async_runtime::spawn_blocking`], never on an async command
//! thread, so a slow or huge store can never stall the event loop.
//!
//! Import is deliberately dialog-first: the native folder picker runs
//! before the store is even resolved, so cancelling performs no
//! filesystem work at all — in particular the app data directory is
//! never created by a cancelled import. The selected host path moves only
//! between native code ([`project_dialog::pick_source_folder`] and
//! [`source::import_project`]) and is never sent to the webview; only
//! the persisted record, whose only path-like fields are the snapshot's
//! normalized source-relative paths, crosses back.
//!
//! Builds are a single in-app action with no native confirmation dialog,
//! serialized by one global slot ([`BuildJobs`], reported as a busy error
//! rather than a hidden queue), and deliberately run *without* holding
//! the [`Controller`](crate::Controller) lifecycle lock: a long `podman
//! build` must never block Pocket status, start, or stop. The slot is
//! claimed *before* anything is allocated, so concurrent build requests
//! can never stack two builds; a busy request returns immediately and
//! changes no record and no progress state. Live progress is read from a
//! separate map that never touches the build lock, so status stays
//! responsive mid-build. A successful build commits the verified image
//! id and installs the record: it becomes `Installed` without launching
//! the app or requiring a preview, keeping its id, summary, and data.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::builder::{self, BuildProgress, BuildProgressHandle, BuildState};
use crate::projects::{self, Phase, Store};
use crate::source;
use crate::AppContext;

/// Native build-job state, injected via `tauri::State`.
///
/// The `build` mutex serializes whole builds so only one managed
/// `podman build` ever runs at a time. Commands `try_lock` it and report
/// a busy error rather than queueing: a queued command would sit behind
/// a minutes-long build with no observable state. The `progress` map is
/// guarded by a plain `std::sync::Mutex` held only long enough to insert
/// or clone a handle — reading progress never takes the build lock.
#[derive(Default)]
pub struct BuildJobs {
    build: tokio::sync::Mutex<()>,
    progress: std::sync::Mutex<HashMap<String, BuildProgressHandle>>,
}

impl BuildJobs {
    /// Claims the single build slot, or `None` when a build is already
    /// running. The returned guard releases the slot when the calling
    /// command ends. Also claimed by the AI conversation finalize step so
    /// a snapshot save never interleaves with a build.
    pub(crate) fn try_claim(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        self.build.try_lock().ok()
    }

    /// Registers a fresh job for `id`, replacing any earlier job's
    /// handle so a re-issued build can never inherit a stale log.
    fn begin(&self, id: &str) -> BuildProgressHandle {
        let handle = BuildProgressHandle::new();
        self.progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_owned(), handle.clone());
        handle
    }

    /// The remembered handle for `id`, if a build was ever started for
    /// it. Takes only the map lock, never the build lock.
    fn handle(&self, id: &str) -> Option<BuildProgressHandle> {
        self.progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }
}

fn blocking_failed(what: &str) -> String {
    format!(
        "Pockets could not finish {what}; the file operation was \
         interrupted. Try again; no project record was overwritten."
    )
}

/// Loads the record for `id` on the blocking pool. `load` validates the
/// id shape before touching the filesystem, so this is also the
/// frontend id-validation gate for both build commands.
async fn load_record(store: &Store, id: &str) -> Result<projects::Project, String> {
    let root = store.root().to_path_buf();
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || Store::new(root).load(&id))
        .await
        .map_err(|_| blocking_failed("loading the project record"))?
}

/// Every persisted project, sorted by native id.
pub async fn list_projects(ctx: &AppContext) -> Result<Vec<ProjectView>, String> {
    let store = ctx.store();
    tokio::task::spawn_blocking(move || {
        Ok(store
            .list()?
            .into_iter()
            .map(|project| {
                // A broken development link must not hide an installed app.
                // The development panel reports the link error separately.
                let development_linked =
                    crate::development::is_linked(&store, &project.id).unwrap_or(false);
                let unsynced_changes = if development_linked {
                    crate::development::has_unsynced_changes(&store, &project)
                } else {
                    false
                };
                ProjectView {
                    project,
                    development_linked,
                    unsynced_changes,
                }
            })
            .collect::<Vec<_>>())
    })
    .await
    .map_err(|_| blocking_failed("listing your Pockets"))?
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectView {
    #[serde(flatten)]
    pub project: projects::Project,
    pub development_linked: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unsynced_changes: bool,
}

/// Imports one Pocket folder and returns the persisted record.
pub async fn import_project(
    ctx: &AppContext,
    selected: PathBuf,
) -> Result<projects::Project, String> {
    let store = ctx.store();
    tokio::task::spawn_blocking(move || source::import_project(&store, &selected))
        .await
        .map_err(|_| blocking_failed("importing the selected folder"))?
}

/// Builds the image for project `id` and returns the persisted record,
/// now `phase: "installed"` and carrying its verified image id. A
/// successful build installs: it never launches the app and never
/// requires a preview, and the record keeps its native id, summary,
/// previewed flag, and data exactly as recorded. There is no native
/// confirmation dialog — the in-app Build press is the action itself.
///
/// The id is the only frontend input and only ever selects a persisted
/// record: the trusted record is loaded first (which also validates the
/// id), its manifest must support this host architecture, and a project
/// with an installed revision is updated only when source has changed. The global build slot is
/// then claimed — busy is an immediate error, never a hidden queue —
/// before anything is allocated. On claim a fresh progress handle is
/// registered and [`build_approved_project`] does the work. The
/// `Controller` lifecycle lock is deliberately not held: status, start,
/// and stop stay responsive while a build runs.
/// When a project has a linked development folder, capture its current
/// approved source before building so Build and start / Update always
/// packages what live Preview shows rather than a stale earlier snapshot.
fn sync_if_linked(store: &Store, id: &str) -> Result<projects::Project, String> {
    if crate::development::is_linked(store, id)? {
        crate::development::sync(store, id)
    } else {
        store.load(id)
    }
}

pub async fn build_project(ctx: &AppContext, id: String) -> Result<projects::Project, String> {
    let store = ctx.store();
    let _ = load_record(&store, &id).await?;
    let Some(_serializing) = ctx.build_jobs.try_claim() else {
        return Err(
            "Another Pocket is building right now. Wait for it to finish, \
             then start this build again."
                .to_owned(),
        );
    };
    let project = {
        let _lifecycle = ctx.controller.lifecycle_guard().await;
        let sync_store = store.clone();
        let sync_id = id.clone();
        tokio::task::spawn_blocking(move || sync_if_linked(&sync_store, &sync_id))
            .await
            .map_err(|_| blocking_failed("syncing your Pocket's source"))??
    };
    project.manifest.validate_host()?;
    let updating = project.installed.is_some();
    if updating {
        if !project.has_pending_changes() {
            return Err("This Pocket already has its current source installed.".into());
        }
    } else {
        guard_buildable(&project)?;
    }
    let bin = crate::require_podman()?;
    let progress = ctx.build_jobs.begin(&project.id);
    let result = if updating {
        async {
            let candidate = build_candidate_image(&store, &bin, &project.id, &progress).await?;
            let _lifecycle = ctx.controller.lifecycle_guard().await;
            let committed =
                crate::project_updates::apply_candidate(&store, &bin, candidate).await?;
            Ok(committed)
        }
        .await
    } else {
        build_approved_project(&store, &bin, &project.id, &progress).await
    };
    match result {
        Ok(committed) => Ok(committed),
        Err(err) => {
            progress.fail(&err);
            Err(err)
        }
    }
}

pub async fn project_build_status(ctx: &AppContext, id: String) -> Result<BuildProgress, String> {
    let store = ctx.store();
    let project = load_record(&store, &id).await?;
    if let Some(handle) = ctx.build_jobs.handle(&project.id) {
        return Ok(handle.snapshot());
    }
    if project.built_image_id.is_some() {
        return Ok(BuildProgress {
            state: BuildState::Succeeded,
            ..Default::default()
        });
    }
    Ok(BuildProgress::default())
}

/// Builds the image for draft `id` end to end and returns the persisted
/// record, installed with its verified image id. A successful build
/// installs the record (phase `Installed`) without launching the app or
/// requiring a preview. This is the service behind `build_project` — no
/// dialog, no job-map or lifecycle locks — so the follow-up opt-in live
/// Podman tests can drive it directly.
///
/// The whole flow is native and initial-installation only: the record is
/// freshly loaded and guarded ([`guard_buildable`]: draft, not yet built)
/// before anything is allocated, [`build_candidate_image`] then stages
/// and builds the working snapshot as a private candidate that changes
/// nothing persisted, and only afterwards does [`commit_built_image`]
/// atomically persist the verified `sha256:` id and the installed phase
/// onto a freshly loaded record. The commit refuses the image if the
/// record's source digest or manifest changed while it was building: the
/// fresh image describes exactly what was staged, not a working source
/// that drifted mid-build. Every filesystem step runs on the blocking
/// pool.
pub(crate) async fn build_approved_project(
    store: &Store,
    bin: &Path,
    id: &str,
    progress: &BuildProgressHandle,
) -> Result<projects::Project, String> {
    // `Store` owns only its root, so each blocking step rebuilds its own
    // handle from a cloned root instead of fighting the `'static` bound.
    let root = store.root().to_path_buf();

    // Fresh load and the install-only guard before any allocation: even
    // if the command's pre-claim check raced with something, an installed
    // or already-built record can never be rebuilt on this path.
    {
        let root = root.clone();
        let id = id.to_owned();
        let loaded = tokio::task::spawn_blocking(move || Store::new(root).load(&id))
            .await
            .map_err(|_| blocking_failed("loading the project record"))??;
        guard_buildable(&loaded)?;
    }

    // Stage and build the working snapshot privately: the candidate
    // helper allocates the scratch directory, verifies the staged source
    // against the record, and runs the pinned build without writing the
    // record, its installed metadata, or any live state. The candidate
    // pins exactly the digest and manifest that were staged.
    let candidate = build_candidate_image(store, bin, id, progress).await?;
    let staged = candidate.project;
    let image_id = candidate.image_id;

    // Commit only while the staged snapshot is still the working record:
    // a save or restore that changed the source digest or manifest
    // mid-build means the fresh image no longer describes the working
    // state, so it must not become the installed identity.
    let committed = {
        tokio::task::spawn_blocking(move || -> Result<projects::Project, String> {
            let store = Store::new(root);
            let current = store.load(&staged.id)?;
            if current.source_digest != staged.source_digest || current.manifest != staged.manifest
            {
                return Err(format!(
                    "project {} changed while its image was building; the new \
                     image was not installed. Build the current revision again.",
                    staged.id
                ));
            }
            commit_built_image(&store, &staged.id, image_id)
        })
        .await
        .map_err(|_| blocking_failed("recording the built image"))??
    };
    Ok(committed)
}

/// One freshly built image paired with the exact working snapshot it was
/// staged from, so a caller can never commit an image without also
/// proving what it was built from.
///
/// `project` is the working record as loaded when staging verified the
/// managed source: its `source_digest`, `manifest`, and file list pin
/// precisely what the image contains, so a later commit can compare the
/// live record against it and refuse when the working source drifted.
/// `image_id` is the verified `sha256:` id read back from the build's
/// iidfile. Nothing is persisted: a candidate exists only in memory
/// until the caller deliberately installs it.
pub(crate) struct BuiltCandidate {
    /// The working snapshot that was staged, digest-verified, and built.
    pub project: projects::Project,
    /// The verified image id (`sha256:<64 lowercase hex>`) of the build.
    pub image_id: String,
}

/// Builds a candidate image for the current working snapshot of `id`
/// without mutating anything installed.
///
/// Unlike [`build_approved_project`], this accepts both draft and
/// installed records and only builds: the working record is freshly
/// loaded, its manifest re-validated against the host architecture, a
/// fresh owner-only (0700) `<root>/<id>/builds/<uuid>` directory is
/// created exclusively, only the recorded, digest-verified file list is
/// staged from the managed source via [`source::prepare_build_context`],
/// and [`builder::build_image`] runs the pinned `podman build`, ending
/// only with a verified `sha256:` image id read back from its iidfile.
/// Nothing is saved and nothing live is touched: the record and its
/// installed metadata are never written, the managed source is only
/// read, and no container is started, stopped, replaced, or removed and
/// no volume is created or deleted. Installing a candidate is a separate
/// decision belonging to the caller — the initial install via
/// [`commit_built_image`] here, or the update path's own transaction.
/// The returned [`BuiltCandidate::project`] pins the staged digest and
/// manifest for the caller's compare at commit time. Every filesystem
/// step runs on the blocking pool.
pub(crate) async fn build_candidate_image(
    store: &Store,
    bin: &Path,
    id: &str,
    progress: &BuildProgressHandle,
) -> Result<BuiltCandidate, String> {
    // `Store` owns only its root, so each blocking step rebuilds its own
    // handle from a cloned root instead of fighting the `'static` bound.
    let root = store.root().to_path_buf();

    // Fresh working snapshot before anything is allocated, with the host
    // architecture re-checked: a record can predate a move to a
    // different machine. Draft and installed records are both valid
    // candidates here — deciding whether to build is the caller's.
    let record = {
        let root = root.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || Store::new(root).load(&id))
            .await
            .map_err(|_| blocking_failed("loading the project record"))??
    };
    record.manifest.validate_host()?;

    // Exclusive owner-only build directory, then the digest-verified
    // staging of exactly the recorded files, together on the pool. A
    // copy moves into this one task while the original snapshot stays
    // behind for the returned candidate: the caller must compare against
    // the exact record that was staged, never a later load.
    let staged = record.clone();
    let project_id = record.id.clone();
    let (build_dir, context) = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || -> Result<(PathBuf, PathBuf), String> {
            let store = Store::new(root);
            let build_dir = create_build_dir(&store, &staged.id)?;
            let context = source::prepare_build_context(&store, &staged, &build_dir)?;
            Ok((build_dir, context))
        })
        .await
        .map_err(|_| blocking_failed("staging the build context"))??
    };

    // The build itself streams progress into `progress` and ends only
    // with a verified `sha256:` image id.
    let image_id = builder::build_image(bin, &context, &build_dir, &project_id, progress).await?;

    Ok(BuiltCandidate {
        project: record,
        image_id,
    })
}

/// Atomically records the verified `image_id` on a freshly loaded copy
/// of `id` and installs it: a successful build sets `phase` to
/// [`Phase::Installed`] in the same atomic save. It never launches the
/// app and never requires a preview — `previewed` carries over exactly
/// as recorded (a draft was never previewed, and stays that way until a
/// real open). Only `built_image_id` and `phase` change: the id itself
/// is immutable and the recorded source fields (including
/// `source_digest`) are carried over verbatim, all of them re-validated
/// by [`Store::save`] before the atomic rename. The buildable guard runs
/// again on the fresh copy, so a record that raced into a different
/// state is never overwritten.
fn commit_built_image(
    store: &Store,
    id: &str,
    image_id: String,
) -> Result<projects::Project, String> {
    let mut record = store.load(id)?;
    guard_buildable(&record)?;
    record.built_image_id = Some(image_id);
    record.phase = Phase::Installed;
    store.save(&record)?;
    Ok(record)
}

/// Guards that `record` is still buildable: a draft with no built image.
/// Checked on every fresh load inside [`build_approved_project`] and
/// [`commit_built_image`], so a re-issued build command can never
/// rebuild or overwrite an installed or already-built project.
fn guard_buildable(record: &projects::Project) -> Result<(), String> {
    if record.phase != Phase::Draft {
        return Err(format!(
            "project {} is already installed; only draft projects can be built.",
            record.id
        ));
    }
    if record.built_image_id.is_some() {
        return Err(format!(
            "project {} already has a built image; each imported snapshot \
             is built once.",
            record.id
        ));
    }
    Ok(())
}

/// Allocates `<root>/<id>/builds/<uuid>` exclusively with mode 0700 and
/// returns it. The id is validated through the store's own
/// id-guarded pathing (`source_path` shares [`Store`]'s join guard), so
/// a hostile id can never move the builds directory outside the store.
/// 0700 is what the builder's output-directory check requires for the
/// image-id file, and the exclusive create means a fresh uuid can
/// never reuse or clobber an earlier build directory.
fn create_build_dir(store: &Store, id: &str) -> Result<PathBuf, String> {
    let project_dir = store
        .source_path(id)?
        .parent()
        .ok_or_else(|| format!("the project directory for {id} could not be derived"))?
        .to_path_buf();
    let builds = project_dir.join("builds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&builds)
            .map_err(|err| {
                format!(
                    "the builds directory {} could not be created: {err}",
                    builds.display()
                )
            })?;
        let dir = builds.join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|err| {
                format!(
                    "the build directory {} could not be created: {err}",
                    dir.display()
                )
            })?;
        Ok(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .create(&builds)
            .map_err(|err| {
                format!(
                    "the builds directory {} could not be created: {err}",
                    builds.display()
                )
            })?;
        let dir = builds.join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::DirBuilder::new().create(&dir).map_err(|err| {
            format!(
                "the build directory {} could not be created: {err}",
                dir.display()
            )
        })?;
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;
    use crate::projects::SourceSummary;

    fn test_summary() -> SourceSummary {
        SourceSummary {
            files: vec!["app/main.go".to_owned(), "go.mod".to_owned()],
            total_bytes: 2048,
            containerfile: "FROM scratch\n".to_owned(),
            source_digest: "a".repeat(64),
        }
    }

    /// The service guards run before anything Podman-facing: an already
    /// installed project and a malformed id are refused without
    /// allocating anything and without the shared progress ever leaving
    /// `Idle` — `builder::build_image` transitions to `Running` on any
    /// attempted build, and the deliberately bogus binary path would
    /// then have produced a `Failed` job. All other build coverage
    /// belongs to the final opt-in live phase.
    #[tokio::test]
    async fn build_service_refuses_installed_and_invalid_ids_before_building() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let store = Store::new(dir.path().join("projects"));
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let mut installed = store
            .create(manifest, test_summary())
            .expect("create must succeed");
        installed.phase = Phase::Installed;
        installed.built_image_id = Some(format!("sha256:{}", "b".repeat(64)));
        store.save(&installed).expect("mark installed");

        let bin = Path::new("/definitely/not/podman");
        let progress = BuildProgressHandle::new();

        let err = build_approved_project(&store, bin, &installed.id, &progress)
            .await
            .expect_err("an installed project must never be rebuilt");
        assert!(err.contains("already"), "unexpected error: {err}");

        let err = build_approved_project(&store, bin, "not-a-project-id", &progress)
            .await
            .expect_err("a malformed id must never be built");
        assert!(err.contains("valid project id"), "unexpected error: {err}");

        // Neither refusal allocated or ran anything: no builds directory
        // under the project, and the handle never left the Idle default.
        let project_dir = store.root().join(&installed.id);
        assert!(
            !project_dir.join("builds").exists(),
            "a refused build must not allocate a builds directory"
        );
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.state, BuildState::Idle);
        assert!(snapshot.error.is_none());
    }

    /// A successful build commits the verified image id and installs:
    /// the record must become `Installed` without launching anything or
    /// requiring a preview — it stays unpreviewed — while the native id
    /// and every recorded source field — `source_digest` included —
    /// carry over verbatim and durably. No Podman involved: this drives
    /// the commit step on its own.
    #[test]
    fn commit_built_image_installs_and_retains_identity() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let store = Store::new(dir.path().join("projects"));
        let manifest = manifest::bundled_manifest().expect("bundled manifest");
        let draft = store
            .create(manifest, test_summary())
            .expect("create must succeed");

        let image_id = format!("sha256:{}", "c".repeat(64));
        let committed = commit_built_image(&store, &draft.id, image_id.clone())
            .expect("committing a build result must succeed");

        assert_eq!(committed.id, draft.id, "the native id is immutable");
        assert_eq!(
            committed.source_digest, draft.source_digest,
            "the recorded snapshot must not change"
        );
        assert_eq!(
            committed.phase,
            Phase::Installed,
            "a successful build installs"
        );
        assert!(!committed.previewed, "building must not preview");
        assert_eq!(committed.built_image_id.as_deref(), Some(image_id.as_str()));

        // Durable across a reopen: a fresh store sees exactly the same
        // installed, still-unpreviewed built record. The store's own
        // normalization pins the authoritative `installed` revision from
        // the committed mirror, so the reopened copy carries it where
        // the just-saved in-memory clone left it unset.
        let reopened = Store::new(dir.path().join("projects"));
        let mut expected = committed.clone();
        expected.installed = Some(projects::InstalledRevision {
            image_id: image_id.clone(),
            manifest: draft.manifest.clone(),
            source_digest: draft.source_digest.clone(),
            revision_id: None,
            built_at_seconds: None,
        });
        assert_eq!(
            reopened.load(&draft.id).expect("load after commit"),
            expected
        );
    }

    /// The completion handoff (`ai::agent::handoff`) is exactly
    /// [`source::import_project`] — the same snapshot path a human folder
    /// pick uses — so it can only ever produce an ordinary draft:
    /// `Phase::Draft` with no built image. Explicit Build is the only
    /// promotion path: committing the verified image id (a build's final
    /// step) is what installs the record, nothing before it.
    #[test]
    fn completion_handoff_imports_a_draft_and_only_the_build_installs() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let store = Store::new(dir.path().join("projects"));

        // A minimal importable Pocket folder, the shape the agent leaves
        // its generated workspace in at completion (`pocket.yaml` and
        // `Containerfile` at the root).
        let source = tempfile::TempDir::new().expect("temp source");
        std::fs::write(
            source.path().join("pocket.yaml"),
            concat!(
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
            ),
        )
        .expect("write manifest");
        std::fs::write(source.path().join("Containerfile"), "FROM scratch\n")
            .expect("write Containerfile");

        let imported = crate::source::import_project(&store, source.path())
            .expect("the generated workspace must import");
        assert_eq!(
            imported.phase,
            Phase::Draft,
            "completion handoff ends at draft state"
        );
        assert_eq!(
            imported.built_image_id, None,
            "handoff never builds or installs"
        );

        // Only the explicit Build commits the image and installs.
        let image_id = format!("sha256:{}", "d".repeat(64));
        let installed = commit_built_image(&store, &imported.id, image_id.clone())
            .expect("committing a build result must succeed");
        assert_eq!(
            installed.phase,
            Phase::Installed,
            "explicit Build is the only promotion path"
        );
        assert_eq!(installed.built_image_id.as_deref(), Some(image_id.as_str()));
    }

    /// Writes an executable stand-in for `podman` that appends every
    /// invocation to `record` as one space-joined line and answers the
    /// pinned `podman build ... --iidfile <path>` by writing `image_id`
    /// to the iidfile. Only shell builtins and redirections: completing
    /// the fake build needs no external tool.
    fn fake_build_podman(dir: &Path, record: &Path, image_id: &str) -> PathBuf {
        let script = format!(
            r#"#!/bin/sh
{{ printf '%s ' "$@"; printf '\n'; }} >> '{}'
prev=''
iid=''
for arg in "$@"; do
    if [ "$prev" = '--iidfile' ]; then iid=$arg; fi
    prev=$arg
done
printf '%s\n' '{}' > "$iid"
exit 0
"#,
            record.display(),
            image_id
        );
        let bin = dir.join("fake-podman");
        std::fs::write(&bin, script).expect("write fake podman");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake podman");
        }
        bin
    }

    /// Every file path under `dir`, relative to it, sorted: a change to
    /// the managed source shows up as a difference between two listings.
    fn listed_files(dir: &Path) -> Vec<String> {
        let mut files = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in std::fs::read_dir(&current).expect("read source dir") {
                let path = entry.expect("source entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else if let Ok(relative) = path.strip_prefix(dir) {
                    files.push(relative.display().to_string());
                }
            }
        }
        files.sort();
        files
    }

    /// A candidate build for an installed pocket produces a new image
    /// while the installed record, its revision metadata, and the
    /// managed source stay exactly as they were, and the whole flow
    /// touches the engine exactly once: the stand-in records every call,
    /// so any run, start, stop, rm, or volume invocation would show up.
    /// No live Podman and no real deployment: acceptance here is that
    /// the candidate image exists while the installed state does not
    /// change.
    #[tokio::test]
    async fn candidate_build_of_an_installed_pocket_never_mutates_the_record() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let store = Store::new(dir.path().join("projects"));

        // A real managed source, so the digest-verified staging runs
        // against genuine bytes rather than an in-memory-only summary.
        let source = tempfile::TempDir::new().expect("temp source");
        std::fs::write(
            source.path().join("pocket.yaml"),
            concat!(
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
            ),
        )
        .expect("write manifest");
        std::fs::write(source.path().join("Containerfile"), "FROM scratch\n")
            .expect("write Containerfile");
        let draft = crate::source::import_project(&store, source.path())
            .expect("the demo folder must import");

        // Mark the pocket installed through the store so `installed` is
        // the normalized, authoritative revision, then take the baseline
        // a fresh load sees.
        let old_image = format!("sha256:{}", "b".repeat(64));
        let mut installed = draft.clone();
        installed.built_image_id = Some(old_image.clone());
        store.save(&installed).expect("mark installed");
        let before = store.load(&draft.id).expect("installed baseline");
        assert_eq!(before.phase, Phase::Installed);
        assert_eq!(before.built_image_id.as_deref(), Some(old_image.as_str()));
        let revision = before.installed.clone().expect("the revision is pinned");
        assert_eq!(revision.image_id, old_image);

        let source_dir = store.source_path(&draft.id).expect("managed source path");
        let files_before = listed_files(&source_dir);

        let new_image = format!("sha256:{}", "e".repeat(64));
        let invocations = dir.path().join("invocations.txt");
        let bin = fake_build_podman(dir.path(), &invocations, &new_image);
        let progress = BuildProgressHandle::new();

        let candidate = build_candidate_image(&store, &bin, &draft.id, &progress)
            .await
            .expect("an installed pocket must accept a candidate build");

        // The candidate carries the new image plus the exact working
        // snapshot for the caller's compare at commit — the installed
        // metadata inside the snapshot is still the old revision.
        assert_eq!(candidate.image_id, new_image);
        assert_eq!(
            candidate.project, before,
            "the candidate must pin the staged working snapshot"
        );
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.state, BuildState::Succeeded);
        assert!(snapshot.error.is_none());

        // The installed record is unchanged: no image was installed and
        // the revision was not replaced by the candidate.
        let after = store
            .load(&draft.id)
            .expect("reload after the candidate build");
        assert_eq!(after, before);
        assert_eq!(after.built_image_id.as_deref(), Some(old_image.as_str()));
        assert_eq!(after.installed.as_ref(), Some(&revision));

        // The managed source was only ever read: the same files remain.
        assert_eq!(
            listed_files(&source_dir),
            files_before,
            "a candidate build must not touch the managed source"
        );

        // The image came from a fresh private build directory staged
        // under the project, not from any installed state.
        let builds = store.root().join(&draft.id).join("builds");
        let mut entries = std::fs::read_dir(&builds)
            .expect("the candidate build must allocate a builds directory")
            .map(|entry| entry.expect("builds entry").path())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries.len(), 1, "exactly one build directory");
        let iid = std::fs::read_to_string(entries[0].join("image-id.txt"))
            .expect("the build must record its image id");
        assert_eq!(iid.trim(), new_image);

        // The engine saw exactly one invocation and it was the build:
        // no container run/start/stop/rm and no volume call ever reached
        // the binary.
        let lines: Vec<String> = std::fs::read_to_string(&invocations)
            .expect("invocation record")
            .lines()
            .map(|line| line.trim_end().to_owned())
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "only the build may invoke podman: {lines:?}"
        );
        assert!(
            lines[0].starts_with("build "),
            "the only call must be a build: {lines:?}"
        );
        assert!(
            lines[0].contains("--iidfile"),
            "the build must be iidfile-pinned: {lines:?}"
        );
    }

    #[test]
    fn sync_if_linked_captures_unsynced_folder_edits_before_building() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("linked-app");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("pocket.yaml"),
            include_str!("../../../examples/hello-pocket/pocket.yaml"),
        )
        .unwrap();
        std::fs::write(root.join("Containerfile"), "FROM scratch\n").unwrap();
        std::fs::write(root.join("server.js"), "console.log('starter');\n").unwrap();

        let store = Store::new(temp.path().join("store"));
        let linked = crate::development::link(&store, None, &root).unwrap();
        let initial_digest = linked.source_digest.clone();

        // Simulate editing the linked folder in an external coding tool
        // without clicking Sync source before Build and start.
        std::fs::write(root.join("server.js"), "console.log('real app');\n").unwrap();

        let synced = sync_if_linked(&store, &linked.id).expect("linked sync succeeds");
        assert_ne!(
            synced.source_digest, initial_digest,
            "live linked edits must be captured before building"
        );
        let stored_server =
            std::fs::read_to_string(store.source_path(&linked.id).unwrap().join("server.js"))
                .unwrap();
        assert_eq!(stored_server, "console.log('real app');\n");

        // Unlinked records keep their stored snapshot unchanged.
        crate::development::unlink(&store, &linked.id).unwrap();
        std::fs::write(root.join("server.js"), "console.log('after unlink');\n").unwrap();
        let unlinked = sync_if_linked(&store, &linked.id).expect("unlinked load succeeds");
        assert_eq!(unlinked.source_digest, synced.source_digest);
    }
}
