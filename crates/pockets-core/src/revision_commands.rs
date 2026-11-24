//! Source history for managed snapshots and installed revisions.

use crate::revisions::RevisionSummary;
use crate::AppContext;

pub async fn list_project_revisions(
    ctx: &AppContext,
    id: String,
) -> Result<Vec<RevisionSummary>, String> {
    let store = ctx.store();
    tokio::task::spawn_blocking(move || crate::revisions::list(&store, &id))
        .await
        .map_err(|_| "Reading source history was interrupted.".to_owned())?
}

#[derive(serde::Serialize)]
pub struct RestoreResult {
    pub project: crate::projects::Project,
}

pub async fn restore_project_revision(
    ctx: &AppContext,
    id: String,
    revision_id: String,
) -> Result<RestoreResult, String> {
    let _building = ctx
        .build_jobs
        .try_claim()
        .ok_or("Wait for the current Build/Update to finish before restoring source.")?;
    let _lifecycle = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    tokio::task::spawn_blocking(move || {
        if crate::development::is_linked(&store, &id)? {
            return Err("Restore source in your coding tool, then Update. To restore a Pockets snapshot, unlink the development folder first.".to_owned());
        }
        let current = store.load(&id)?;
        let available = crate::revisions::list(&store, &id)?;
        let revision = available
            .iter()
            .find(|revision| revision.id == revision_id)
            .ok_or("This source revision does not exist.")?;
        if revision.source_digest == current.source_digest {
            return Ok(RestoreResult { project: current });
        }
        crate::revisions::archive_current(&store, &id, "Before restore")?;
        let project = crate::revisions::restore_working(&store, &id, &revision_id)?;
        Ok(RestoreResult { project })
    })
    .await
    .map_err(|_| "Restoring source was interrupted.".to_owned())?
}

pub async fn list_project_versions(
    ctx: &AppContext,
    id: String,
) -> Result<crate::versions::VersionsReport, String> {
    let store = ctx.store();
    tokio::task::spawn_blocking(move || crate::versions::list(&store, &id))
        .await
        .map_err(|_| "Reading version history was interrupted.".to_owned())?
}

/// Names a recorded version in the owner's own words, or clears the name.
pub async fn set_project_version_note(
    ctx: &AppContext,
    id: String,
    revision_id: String,
    note: Option<String>,
) -> Result<(), String> {
    // An Update appends to the same log; never write it concurrently.
    let _building = ctx
        .build_jobs
        .try_claim()
        .ok_or("Wait for the current Build/Update to finish before naming a version.")?;
    let store = ctx.store();
    tokio::task::spawn_blocking(move || {
        crate::versions::set_note(&store, &id, &revision_id, note.as_deref())
    })
    .await
    .map_err(|_| "Saving the version note was interrupted.".to_owned())?
}
