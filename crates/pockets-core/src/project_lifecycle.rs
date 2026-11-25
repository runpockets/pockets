//! Native lifecycle service for managed projects: status, start, stop,
//! preview marking, and hard deletion, all keyed by project id alone.
//!
//! The [`tauri::command`]s here take exactly one frontend argument —
//! the project `id` — and hold the shared [`crate::Controller`]
//! lifecycle lock for their whole call, so project and bundled-Pocket
//! engine operations never interleave. Everything beyond the id (store
//! root, Podman binary, container name, volume, image, port, and URL)
//! is resolved natively; no path, image, URL, flag, or argument list
//! ever crosses from the webview.
//!
//! [`open_project`] is the only command that touches the browser, and
//! it persists the [`mark_previewed`] marker only after the native open
//! actually succeeded. A successful build installs a project on its own,
//! so there is no separate keep step: preview marking is metadata only,
//! never a gate on installation. No build is ever run here; this module
//! only drives containers that are already built.
//!
//! Every operation resolves state from the authoritative [`Store`] record
//! by id — never from a frontend-supplied name, image, path, or argument
//! list — and pins the [`Target`] to the record's immutable built image
//! id, which overrides any mutable manifest tag. The record's id is the
//! only identity source: `mark_previewed` mutates at most `previewed`,
//! never the id, phase, source digest, or the manifest-derived volume.
//!
//! Unbuilt records never reach the engine: [`status`] reports a stopped
//! view with a build-first message without any Podman call, [`start`] and
//! [`stop`] fail with an actionable build-first error, and [`delete`] skips
//! the engine outright — a draft never created a container or a volume.
//! The single-file record read runs inline; the store's atomic save remains
//! the only record write path, and [`delete`] is the only path that ever
//! removes a record directory.
//!
//! [`delete`] is the one destructive command here, and its order is the
//! entire safety story: the owned container, then the data volume, then local
//! runtime settings, then the record directory. Engine teardown runs before the record goes,
//! so a failed teardown leaves the authoritative record intact and the
//! delete retryable, and because every step treats what is already gone as
//! success, a retry repeats only the work that has not happened yet — even
//! a record directory that outlived a completed teardown is finished by the
//! next attempt. The built image is deliberately never removed: it is
//! content-addressed and may be shared by other pockets built from
//! identical sources, so the leak is accepted instead of guessing at who
//! else depends on the digest (see [`crate::pocket`]).

use std::path::Path;

use crate::health;
use crate::local_sites::{self, Site};
use crate::pocket::{self, PocketState, PocketStatus, Target};
use crate::projects::{Project, Store};
use crate::tailscale;
use crate::{require_podman, AppContext, PocketView};

/// Resolves the managed Pocket's persistent site address when ready, updating
/// `view.status.url` and returning the verified loopback port + stable slug.
fn apply_managed_site(
    view: &mut PocketView,
    store: &Store,
    project: &Project,
) -> Result<(Option<u16>, String), String> {
    if view.ready {
        let address = Site::Managed(project).running_address(store, view.status.url.as_deref())?;
        let port = address.port;
        let slug = address
            .slug()
            .map(str::to_owned)
            .unwrap_or_else(|| local_sites::slug_for_project(store, project));
        view.status.url = Some(address.url());
        Ok((Some(port), slug))
    } else {
        view.status.url = None;
        Ok((None, local_sites::slug_for_project(store, project)))
    }
}

pub async fn project_status(ctx: &AppContext, id: String) -> Result<PocketView, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    let bin = require_podman()?;
    let project = store.load(&id)?;
    let mut view = status_for_project(&bin, &project).await?;
    let (port, slug) = apply_managed_site(&mut view, &store, &project)?;
    view.tailscale = Some(
        tailscale::reconcile_project_share(&store, &ctx.tailscale_portal, &project.id, &slug, port)
            .await?,
    );
    Ok(view)
}

pub async fn set_project_tailscale(
    ctx: &AppContext,
    id: String,
    enabled: bool,
) -> Result<PocketView, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    let bin = require_podman()?;
    let project = store.load(&id)?;
    require_built(&project)?;
    let mut view = status_for_project(&bin, &project).await?;
    let (port, slug) = apply_managed_site(&mut view, &store, &project)?;
    let share = tailscale::set_enabled(
        &store,
        &ctx.tailscale_portal,
        &project.id,
        &slug,
        enabled,
        port,
    )
    .await?;
    view.tailscale = Some(share);
    Ok(view)
}

pub async fn start_project(ctx: &AppContext, id: String) -> Result<PocketView, String> {
    let store = ctx.store();
    let bin = require_podman()?;
    let mut serializing = ctx.controller.lifecycle_guard().await;
    let project = store.load(&id)?;
    let image_id = require_built(&project)?;
    let target = managed_target(&project, image_id)?;
    let current = pocket::inspect_target(&bin, &target).await?;
    let view = if current.state == PocketState::Running {
        finish_start(&bin, &project, &target, current).await?
    } else {
        // If Podman was reinstalled, reset, or pruned, the pinned built image
        // may be missing. Detect that up front (before secret-masked `podman run`)
        // and rebuild transparently from the installed snapshot when there are
        // no unapproved working edits.
        let image_exists = crate::runtime::run(&bin, &["image", "exists", image_id])
            .await
            .is_ok();
        if !image_exists {
            if project.has_pending_changes() {
                return Err(
                    "This Pocket's built image is missing from Podman (for example after \
                     reinstalling or resetting Podman). Click Update to rebuild it from your \
                     current source."
                        .to_owned(),
                );
            }
            drop(serializing);
            let Some(_build_guard) = ctx.build_jobs.try_claim() else {
                return Err(
                    "This Pocket's image needs to be rebuilt, and another Pocket is building right now. \
                     Wait for that build to finish and click Open again."
                        .to_owned(),
                );
            };
            let progress = ctx.build_jobs.begin(&project.id);
            let rebuilt = async {
                let candidate =
                    crate::creation::build_candidate_image(&store, &bin, &project.id, &progress)
                        .await?;
                let _rebuild_lock = ctx.controller.lifecycle_guard().await;
                crate::project_updates::apply_candidate(&store, &bin, candidate).await
            }
            .await;
            if let Err(err) = rebuilt {
                progress.fail(&err);
                return Err(format!(
                    "This Pocket's built image was missing from Podman and could not be rebuilt automatically: {err}"
                ));
            }
        } else {
            // Keychain / vault reads happen outside the lifecycle lock.
            drop(serializing);
        }
        let values = crate::runtime_settings::launch_values(
            &store,
            &id,
            crate::runtime_settings::Environment::Installed,
        )
        .await?;
        serializing = ctx.controller.lifecycle_guard().await;
        start(&store, &bin, &id, &values).await?
    };
    let project = store.load(&id)?;
    let mut view = view;
    let (port, slug) = apply_managed_site(&mut view, &store, &project)?;
    view.tailscale = Some(match port {
        Some(port) => {
            tailscale::restore_on_start(&store, &ctx.tailscale_portal, &project.id, &slug, port)
                .await
        }
        None => {
            tailscale::reconcile_project_share(
                &store,
                &ctx.tailscale_portal,
                &project.id,
                &slug,
                None,
            )
            .await?
        }
    });
    drop(serializing);
    Ok(view)
}

pub async fn stop_project(ctx: &AppContext, id: String) -> Result<PocketView, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    let bin = require_podman()?;
    let mut view = stop(&store, &bin, &id).await?;
    view.tailscale = Some(tailscale::pause_on_stop(&store, &ctx.tailscale_portal, &id).await);
    Ok(view)
}

/// Resolves and validates a running Pocket's browser URL, opens it in the host
/// browser when requested (`open_host_browser`), marks the record previewed,
/// and returns the verified URL. When `prefer_remote` is true (dashboard
/// accessed from a remote host, e.g. over Tailscale), ensures a private
/// Tailscale Serve route is active and returns its HTTPS URL instead of
/// `*.localhost`.
pub async fn open_project(
    ctx: &AppContext,
    id: String,
    open_host_browser: bool,
    prefer_remote: bool,
) -> Result<String, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    let bin = require_podman()?;
    let project = store.load(&id)?;
    let view = status_for_project(&bin, &project).await?;
    if !view.ready {
        return Err(
            "The Pocket is not reachable yet. Start it and wait until it reports ready.".to_owned(),
        );
    }
    let address = Site::Managed(&project).running_address(&store, view.status.url.as_deref())?;
    let url = if prefer_remote {
        let slug = address
            .slug()
            .map(str::to_owned)
            .unwrap_or_else(|| crate::local_sites::slug_for_project(&store, &project));
        let share = tailscale::reconcile_project_share(
            &store,
            &ctx.tailscale_portal,
            &project.id,
            &slug,
            Some(address.port),
        )
        .await?;
        if share.active {
            share.url.unwrap_or_else(|| address.url())
        } else {
            let enabled = tailscale::set_enabled(
                &store,
                &ctx.tailscale_portal,
                &project.id,
                &slug,
                true,
                Some(address.port),
            )
            .await
            .map_err(|err| {
                format!(
                    "Could not open this Pocket remotely over Tailscale ({err}). \
                     On the host it is running at {} (forward port {} over SSH if not using Tailscale).",
                    address.url(),
                    address.port
                )
            })?;
            enabled.url.ok_or_else(|| {
                enabled.message.unwrap_or_else(|| {
                    format!(
                        "Tailscale route for this Pocket is not active yet. Forward port {} over SSH or enable Tailscale.",
                        address.port
                    )
                })
            })?
        }
    } else {
        address.url()
    };
    if open_host_browser {
        crate::open_in_browser(&url).await?;
    }
    mark_previewed(&store, &id)?;
    Ok(url)
}

pub async fn delete_project(ctx: &AppContext, id: String) -> Result<(), String> {
    let _settings = ctx.runtime_settings.0.lock().await;
    let _serializing = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    crate::project_preview::stop_owned(ctx, &id).await?;
    let bin = if store.load(&id)?.built_image_id.is_some() {
        Some(require_podman()?)
    } else {
        None
    };
    let cleanup_store = store.clone();
    delete(&store, bin.as_deref(), &id, async {
        if let Some(bin) = &bin {
            let scope = crate::runtime_env::Scope::new(
                &id,
                crate::runtime_settings::Environment::Installed,
            )?;
            crate::runtime_env::clear(bin, &scope).await?;
        }
        tailscale::cleanup_on_delete(&cleanup_store, &ctx.tailscale_portal, &id).await;
        crate::runtime_settings::forget_project(&cleanup_store, id.clone()).await
    })
    .await
}

/// Exports one built project's Pockets-owned Podman data volume to `destination`
/// and returns the suggested human-readable archive filename.
pub async fn export_project_data_to_path(
    ctx: &AppContext,
    id: String,
    destination: &Path,
) -> Result<String, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    let bin = require_podman()?;
    let project = store.load(&id)?;
    let target = managed_target(&project, require_built(&project)?)?;
    if !pocket::has_owned_volume(&bin, &target).await? {
        return Err(
            "This Pocket has no saved data volume yet. Start it at least once before exporting a backup."
                .to_owned(),
        );
    }
    let default_name = backup_file_name(&project.manifest.metadata.name);
    pocket::export_volume(&bin, &target, destination).await?;
    Ok(default_name)
}

/// Result of restoring a Pocket's Podman data volume from a backup archive.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreDataResult {
    pub file_name: String,
    pub status: PocketView,
}

/// Restores one built project's Podman data volume from a `.tar` archive at `source`.
pub async fn restore_project_data_from_path(
    ctx: &AppContext,
    id: String,
    source: &Path,
    display_name: Option<String>,
) -> Result<RestoreDataResult, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    let bin = require_podman()?;
    let project = store.load(&id)?;
    let target = managed_target(&project, require_built(&project)?)?;
    pocket::restore_volume(&bin, &target, source).await?;
    let mut status = status_for_project(&bin, &project).await?;
    status.tailscale = Some(tailscale::pause_on_stop(&store, &ctx.tailscale_portal, &id).await);
    let file_name = display_name.unwrap_or_else(|| {
        source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| source.display().to_string())
    });
    Ok(RestoreDataResult { file_name, status })
}

pub async fn reset_project_data(ctx: &AppContext, id: String) -> Result<PocketView, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    let bin = require_podman()?;
    let mut view = reset_data(&store, &bin, &id).await?;
    view.tailscale = Some(tailscale::pause_on_stop(&store, &ctx.tailscale_portal, &id).await);
    Ok(view)
}

pub(crate) async fn reset_data(store: &Store, bin: &Path, id: &str) -> Result<PocketView, String> {
    let project = store.load(id)?;
    let target = managed_target(&project, require_built(&project)?)?;
    pocket::reset_volume(bin, &target).await?;
    status_for_project(bin, &project).await
}

fn backup_file_name(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !slug.is_empty() && !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let base = if slug.is_empty() { "pocket" } else { slug };
    format!("{base}-data-backup.tar")
}

/// Reports the project's lifecycle state reconciled from Podman, plus a
/// loopback health probe when running.
///
/// An unbuilt record is reported as stopped with a build-first message
/// and no engine call at all: there is no container, image, or volume to
/// look up yet. A built record is inspected as its managed target, and
/// the health endpoint from the *record's own* manifest is probed only
/// while running with a validated loopback mapping — a stopped (or
/// unmapped) Pocket is never probed and never ready.
#[cfg(test)]
pub(crate) async fn status(store: &Store, bin: &Path, id: &str) -> Result<PocketView, String> {
    let project = store.load(id)?;
    status_for_project(bin, &project).await
}

async fn status_for_project(bin: &Path, project: &Project) -> Result<PocketView, String> {
    let Some(image_id) = project.built_image_id.as_deref() else {
        return Ok(PocketView {
            status: PocketStatus {
                state: PocketState::Stopped,
                url: None,
                message: build_first_message(project),
            },
            ready: false,
            tailscale: None,
        });
    };
    let target = managed_target(project, image_id)?;
    view(bin, &target, project).await
}

/// Creates the project's container and starts it detached, waits for its
/// health endpoint, then returns the reconciled status.
///
/// The record must be built: an unbuilt record fails with an actionable
/// build-first error before any engine call. Start runs against the
/// managed target pinned to the record's immutable image id; readiness
/// is waited on with the record manifest's healthcheck path. A failed
/// readiness wait leaves the container running intact for inspection,
/// exactly like the bundled Pocket — it is never rolled back here.
pub(crate) async fn start(
    store: &Store,
    bin: &Path,
    id: &str,
    values: &crate::runtime_env::Values,
) -> Result<PocketView, String> {
    let project = store.load(id)?;
    let image_id = require_built(&project)?;
    let target = managed_target(&project, image_id)?;

    let status = pocket::start_target(bin, &target, values, store, Site::Managed(&project)).await?;
    finish_start(bin, &project, &target, status).await
}

async fn finish_start(
    bin: &Path,
    project: &Project,
    target: &Target,
    status: PocketStatus,
) -> Result<PocketView, String> {
    let url = match &status.url {
        Some(url) if status.state == PocketState::Running => url.clone(),
        _ => {
            return Err(format!("The Pocket did not start. {}", status.message));
        }
    };

    // Readiness failure is reported as-is; the container stays untouched.
    health::wait_ready(&url, &project.installed_manifest().web.healthcheck).await?;
    view(bin, target, project).await
}

/// Stops the project's owned container and returns its reconciled
/// stopped status, never health-probed, so `ready` is always `false`.
///
/// An unbuilt record fails with an actionable build-first error before
/// any engine call. The stop lookup inside [`pocket::stop_target`]
/// enforces the target's name and ownership label but not the
/// safe-mapping rule, so a running container with an absent or unsafe
/// mapping can still be stopped; container and data volume are left
/// intact by design.
pub(crate) async fn stop(store: &Store, bin: &Path, id: &str) -> Result<PocketView, String> {
    let project = store.load(id)?;
    let image_id = require_built(&project)?;
    let target = managed_target(&project, image_id)?;
    let status = pocket::stop_target(bin, &target).await?;
    Ok(PocketView {
        status,
        ready: false,
        tailscale: None,
    })
}

/// Removes the owned container, data volume, credentials, and record directory
/// with its source snapshot. The built image and linked folder are retained.
///
/// A built record pins the target exactly like start and stop do, and its
/// container is removed before its volume — a running container holds the
/// volume, and [`pocket::remove_target`] force-removes either state. An
/// unbuilt record skips both engine steps entirely: a draft allocated
/// nothing, so there is nothing to look up, and no Podman state or binary
/// takes part in its deletion at all.
///
/// Nothing here is rolled back. Every step is idempotent — an absent
/// container, volume, or directory is success — and the record is only
/// removed once the engine said it is done, so any failure leaves an
/// authoritative record behind and an ordinary retry of this same call
/// repeats exactly the steps that have not happened yet. The image stays:
/// it is content-addressed and may be shared by other pockets built from
/// identical sources.
async fn delete(
    store: &Store,
    bin: Option<&Path>,
    id: &str,
    clear_runtime_settings: impl std::future::Future<Output = Result<(), String>>,
) -> Result<(), String> {
    let project = store.load(id)?;
    if let Some(image_id) = project.built_image_id.as_deref() {
        // The command hands no binary for a record that was unbuilt when it
        // checked; if the record became built in the meantime, resolve the
        // engine here rather than skip teardown — no binary, no delete.
        let found;
        let bin = match bin {
            Some(bin) => bin,
            None => {
                found = require_podman()?;
                found.as_path()
            }
        };
        let target = managed_target(&project, image_id)?;
        pocket::remove_target(bin, &target).await?;
        pocket::remove_volume(bin, &target).await?;
    }
    clear_runtime_settings.await?;
    store.remove(id)
}

/// Synchronous record mutation: marks the built project as previewed and
/// atomically saves it, returning the persisted record.
///
/// Call this **only** after the native browser open for this project has
/// actually succeeded — it is the fact of a real preview, not a request
/// to open one; [`open_project`] is its only caller. An unbuilt record
/// is refused; a project is previewed at most once semantically, so
/// re-marking is idempotent. Only the `previewed` marker changes.
pub(crate) fn mark_previewed(store: &Store, id: &str) -> Result<Project, String> {
    let mut project = store.load(id)?;
    require_built(&project)?;
    project.previewed = true;
    store.save(&project)?;
    Ok(project)
}

/// The record's built image id, or an actionable build-first error. The
/// engine is never consulted for an unbuilt record.
fn require_built(project: &Project) -> Result<&str, String> {
    project
        .built_image_id
        .as_deref()
        .ok_or_else(|| build_first_message(project))
}

/// Build-first wording shared by the unbuilt status view and the
/// actionable start/stop refusal.
fn build_first_message(project: &Project) -> String {
    format!(
        "Project {} has not been built yet. Build it first; Pockets only \
         starts and stops built Pockets.",
        project.id
    )
}

/// Builds the project's lifecycle target: identity derives from the
/// record's validated id, the image is pinned to the record's immutable
/// built digest, and the port and mount path come from its pinned installed
/// manifest, never pending working source. Arbitrary arguments are impossible.
fn managed_target(project: &Project, image_id: &str) -> Result<Target, String> {
    Target::managed(project.installed_manifest(), &project.id, image_id)
}

/// Reconciles the target's status from Podman and probes the record
/// manifest's health endpoint only while running with a validated
/// loopback mapping, mirroring [`crate::read_status`] for one project.
async fn view(bin: &Path, target: &Target, project: &Project) -> Result<PocketView, String> {
    let status = pocket::inspect_target(bin, target).await?;
    let ready = match &status.url {
        Some(url) if status.state == PocketState::Running => {
            health::is_ready(url, &project.installed_manifest().web.healthcheck).await?
        }
        // Stopped (or no safe mapping): never ready, and never probed.
        _ => false,
    };
    Ok(PocketView {
        status,
        ready,
        tailscale: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;
    use crate::projects::{Phase, SourceSummary};

    fn test_summary() -> SourceSummary {
        SourceSummary {
            files: vec!["app/main.go".to_owned(), "go.mod".to_owned()],
            total_bytes: 2048,
            containerfile: "FROM scratch\n".to_owned(),
            source_digest: "a".repeat(64),
        }
    }

    /// A fresh draft record in `store`, not yet built.
    fn draft(store: &Store) -> Project {
        store
            .create(
                manifest::bundled_manifest().expect("bundled manifest"),
                test_summary(),
            )
            .expect("draft create")
    }

    /// Marks `project` as built with an immutable digest and the
    /// `Installed` phase a successful build now commits, then persists it.
    fn mark_built(store: &Store, project: &mut Project) {
        project.built_image_id = Some(format!("sha256:{}", "b".repeat(64)));
        project.phase = Phase::Installed;
        store.save(project).expect("mark built");
        *project = store.load(&project.id).expect("reload installed pin");
    }

    /// Podman's report of the project container's ID: lowercase hex, the only
    /// form [`crate::pocket::is_safe_container_id`] accepts.
    const CONTAINER_ID: &str = "6e1f2c3d4b5a69788796a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4";

    /// One `podman ps --format json` row for the project's container name.
    fn ps_row(id: &str) -> String {
        format!(r#"[{{"Id":"{CONTAINER_ID}","Names":["pockets-project-{id}"]}}]"#)
    }

    /// One `podman inspect` element for the project's container name. `owned`
    /// decides whether it carries the `dev.pockets.managed=project-<id>`
    /// label. Removal never uses a URL, so the port map is null.
    fn container_json(id: &str, owned: bool) -> String {
        let config = if owned {
            let key = pocket::OWNERSHIP_LABEL;
            format!(r#""Config":{{"Labels":{{"{key}":"project-{id}"}}}}"#)
        } else {
            r#""Config":{}"#.to_owned()
        };
        format!(
            r#"[{{"Id":"{CONTAINER_ID}","Name":"pockets-project-{id}","State":{{"Status":"running","Running":true}},{config},"NetworkSettings":{{"Ports":null}}}}]"#
        )
    }

    /// One `podman volume ls --format json` row for the project's data volume.
    fn volume_ls_row(id: &str) -> String {
        format!(r#"[{{"Name":"pockets-project-{id}-data"}}]"#)
    }

    /// One `podman volume inspect` result carrying this project's ownership
    /// label on its data volume.
    fn volume_inspect_json(id: &str) -> String {
        let key = pocket::OWNERSHIP_LABEL;
        format!(r#"[{{"Labels":{{"{key}":"project-{id}"}}}}]"#)
    }

    /// Writes an executable stand-in for `podman` that appends every
    /// invocation to `record` as one space-joined line, prints whatever `body`
    /// prints for the subcommand in `$1`, and exits 0. Only shell builtins are
    /// used: the script must not need any variable or external tool.
    fn fake_podman(dir: &Path, body: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let record = dir.join("invocations.txt");
        let script = format!(
            r#"#!/bin/sh
{{ printf '%s ' "$@"; printf '\n'; }} >> '{}'
{body}
exit 0
"#,
            record.display()
        );
        let bin = dir.join("fake-podman");
        std::fs::write(&bin, script).expect("write fake podman");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake podman");
        }
        (bin, record)
    }

    /// The recorded invocations in order, with the recorder's trailing
    /// separator trimmed so a full argument list compares verbatim.
    fn invocations(record: &Path) -> Vec<String> {
        let text = std::fs::read_to_string(record).expect("invocation record");
        text.lines()
            .map(|line| line.trim_end().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn pending_working_manifest_cannot_change_installed_status_or_healthcheck() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::new(dir.path().join("projects"));
        let mut project = draft(&store);
        mark_built(&store, &mut project);
        let installed = project.installed_manifest().clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let expected_path = installed.web.healthcheck.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let count = stream.read(&mut request).await.unwrap();
            let correct = String::from_utf8_lossy(&request[..count])
                .starts_with(&format!("GET {expected_path} "));
            let status = if correct { "200 OK" } else { "404 Not Found" };
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            correct
        });
        project.manifest.web.port = if installed.web.port == 9090 {
            8080
        } else {
            9090
        };
        project.manifest.web.healthcheck = "/pending-working-health".to_owned();
        project.source_digest = "c".repeat(64);
        store.save(&project).unwrap();
        let mut inspected: serde_json::Value =
            serde_json::from_str(&container_json(&project.id, true)).unwrap();
        inspected[0]["NetworkSettings"]["Ports"] = serde_json::json!({
            (format!("{}/tcp", installed.web.port)): [
                {"HostIp": "127.0.0.1", "HostPort": port.to_string()}
            ]
        });
        let script = format!(
            "case \"$1\" in\nps) printf '%s' '{}' ;;\ninspect) printf '%s' '{}' ;;\nesac",
            ps_row(&project.id),
            inspected
        );
        let (bin, calls) = fake_podman(dir.path(), &script);
        let view = status(&store, &bin, &project.id).await.unwrap();
        assert!(
            view.ready,
            "pending source must not change installed readiness"
        );
        assert_eq!(view.status.url, Some(format!("http://127.0.0.1:{port}")));
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(invocations(&calls)
            .iter()
            .all(|line| line.starts_with("ps ") || line.starts_with("inspect ")));
    }

    /// A draft never reached the engine, so deleting it is a pure store
    /// operation: no binary is handed in at all, and the stand-in — which
    /// fails any call it is asked to make — must stay entirely uninvoked.
    #[tokio::test]
    async fn delete_of_an_unbuilt_draft_never_touches_the_engine() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let store = Store::new(dir.path().join("projects"));
        let draft = draft(&store);
        let project_dir = store.root().join(&draft.id);
        assert!(project_dir.is_dir(), "the draft owns its directory");

        // The stand-in would fail any call and record it; with no binary
        // handed to the delete at all, its record must stay nonexistent.
        let (_bin, record) = fake_podman(
            dir.path(),
            "echo 'an unbuilt delete must not call the engine' >&2; exit 1",
        );
        delete(&store, None, &draft.id, async { Ok(()) })
            .await
            .expect("an unbuilt project is deletable");

        assert!(
            !record.exists(),
            "an unbuilt delete must run no podman command at all: {:?}",
            std::fs::read_to_string(&record).unwrap_or_default()
        );
        assert!(
            !project_dir.exists(),
            "the record and its reserved source directory must be gone"
        );
        assert!(
            store.load(&draft.id).is_err(),
            "a deleted project must not load"
        );
    }

    #[tokio::test]
    async fn failed_runtime_settings_cleanup_leaves_the_project_delete_retryable() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path().join("projects"));
        let project = draft(&store);
        let error = delete(&store, None, &project.id, async {
            Err("Keychain denied access".into())
        })
        .await
        .unwrap_err();
        assert_eq!(error, "Keychain denied access");
        assert_eq!(store.load(&project.id).unwrap(), project);
        delete(&store, None, &project.id, async { Ok(()) })
            .await
            .unwrap();
        assert!(store.load(&project.id).is_err());
    }

    /// An unknown id is the store's problem, not the engine's: the record is
    /// loaded first, so both a malformed id and a well-formed id with no
    /// directory fail with the store's actionable error and nothing else is
    /// consulted or changed.
    #[tokio::test]
    async fn delete_of_an_unknown_or_malformed_id_changes_nothing() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root.clone());
        let kept = draft(&store);

        let (bin, record) = fake_podman(
            dir.path(),
            "echo 'an unknown id must not call the engine' >&2; exit 1",
        );

        let err = delete(&store, Some(&bin), "not-a-project-id", async { Ok(()) })
            .await
            .expect_err("a malformed id must be refused");
        assert!(err.contains("valid project id"), "got {err}");

        let missing = "c".repeat(32);
        let err = delete(&store, Some(&bin), &missing, async { Ok(()) })
            .await
            .expect_err("an id with no record must be refused");
        assert!(err.contains("could not be read"), "got {err}");

        assert!(!record.exists(), "no engine call may precede a record");
        assert_eq!(
            store.load(&kept.id).expect("untouched record"),
            kept,
            "a refused delete must leave every other project alone"
        );
        assert!(root.join(&kept.id).is_dir());
    }

    /// The full teardown order for a built record: owned container, then data
    /// volume, then the record directory with its source snapshot inside — and
    /// the image deliberately untouched, so no `rmi` may ever appear.
    #[tokio::test]
    async fn delete_of_a_built_project_removes_container_then_volume_then_record() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let store = Store::new(dir.path().join("projects"));
        let mut built = draft(&store);
        mark_built(&store, &mut built);
        let id = built.id.clone();
        let project_dir = store.root().join(&id);
        let volume = format!("pockets-project-{id}-data");
        std::fs::write(
            store.source_path(&id).expect("source path").join("main.go"),
            b"package main",
        )
        .expect("source snapshot fixture");

        let script = [
            format!(r#"if [ "$1" = "ps" ]; then printf '{}'; fi"#, ps_row(&id)),
            format!(
                r#"if [ "$1" = "inspect" ]; then printf '{}'; fi"#,
                container_json(&id, true)
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{}'; fi"#,
                volume_ls_row(&id)
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                volume_inspect_json(&id)
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &script);

        delete(&store, Some(&bin), &id, async {
            assert!(project_dir.is_dir());
            assert_eq!(
                invocations(&record).last(),
                Some(&format!("volume rm {volume}"))
            );
            Ok(())
        })
        .await
        .expect("built delete succeeds");

        assert_eq!(
            invocations(&record),
            vec![
                format!("ps --all --filter name=^pockets-project-{id}$ --format json"),
                format!("inspect {CONTAINER_ID}"),
                format!("rm -f {CONTAINER_ID}"),
                format!("volume ls --filter name=^{volume}$ --format json"),
                format!("volume inspect {volume}"),
                format!("volume rm {volume}"),
            ],
            "container first, volume second, image untouched"
        );
        assert!(!project_dir.exists(), "the record directory must be gone");
        assert!(store.load(&id).is_err(), "a deleted project must not load");
    }

    /// Teardown runs before the record goes, so a refusal in the engine — here
    /// a foreign container squatting on the managed name — leaves the
    /// authoritative record exactly as it was and the delete retryable.
    #[tokio::test]
    async fn a_refused_teardown_leaves_the_record_in_place_for_a_retry() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let store = Store::new(dir.path().join("projects"));
        let mut built = draft(&store);
        mark_built(&store, &mut built);
        let id = built.id.clone();
        let project_dir = store.root().join(&id);

        let script = [
            format!(r#"if [ "$1" = "ps" ]; then printf '{}'; fi"#, ps_row(&id)),
            format!(
                r#"if [ "$1" = "inspect" ]; then printf '{}'; fi"#,
                container_json(&id, false)
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &script);

        let err = delete(&store, Some(&bin), &id, async { Ok(()) })
            .await
            .expect_err("a foreign container must block the delete");
        assert!(err.contains("refusing"), "got {err}");

        assert_eq!(
            store.load(&id).expect("the record must survive"),
            built,
            "a refused teardown must leave the record byte-for-byte intact"
        );
        assert!(project_dir.is_dir(), "nothing is deleted out of order");

        let calls = invocations(&record);
        assert!(
            !calls.iter().any(|line| line.starts_with("rm")),
            "the refusal must stop before any removal: {calls:?}"
        );
        assert!(
            !calls.iter().any(|line| line.starts_with("volume")),
            "a refused container must stop the whole teardown: {calls:?}"
        );
    }

    #[test]
    fn mark_previewed_requires_built_and_survives_store_reopen_preserving_identity() {
        let dir = tempfile::TempDir::new().expect("temp store root");
        let root = dir.path().join("projects");
        let store = Store::new(root.clone());

        // Never built: marking a preview is refused with a build-first
        // error, and the refusal leaves the draft untouched on disk.
        let unbuilt = draft(&store);
        let err = mark_previewed(&store, &unbuilt.id)
            .expect_err("unbuilt mark_previewed must be refused");
        assert!(
            err.contains("not been built"),
            "unbuilt refusal must be build-first, got {err}"
        );
        assert_eq!(
            store.load(&unbuilt.id).expect("record"),
            unbuilt,
            "a refused mark_previewed must not change the record"
        );

        // Built saved fixture: the mark persists, and a brand-new Store
        // over the same root must observe exactly the one allowed field
        // change. The `Installed` phase the successful build committed —
        // and the id, manifest, image id, and every source summary field
        // — are all preserved verbatim.
        let mut built = draft(&store);
        mark_built(&store, &mut built);
        let id = built.id.clone();

        let previewed = mark_previewed(&store, &id).expect("mark previewed");
        assert!(previewed.previewed);

        let reopened = Store::new(root);
        let reloaded = reopened.load(&id).expect("load after reopen");
        let mut expected = built.clone();
        expected.previewed = true;
        assert_eq!(reloaded, expected);
        assert_eq!(reloaded.phase, Phase::Installed);
        assert_eq!(reloaded.id, built.id);
        assert_eq!(reloaded.manifest, built.manifest);
        assert_eq!(reloaded.built_image_id, built.built_image_id);
        assert_eq!(reloaded.summary(), built.summary());
    }
}
