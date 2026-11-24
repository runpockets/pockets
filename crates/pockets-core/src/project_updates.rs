//! Explicit application updates. The caller holds BuildJobs, then Controller,
//! through deployment and commit. Building the candidate happens beforehand,
//! while the installed app is still available.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::creation::BuiltCandidate;
use crate::pocket::{self, PocketState, ReplacementReceipt, Target};
use crate::projects::{InstalledRevision, Phase, Project, Store};

fn require_same_source(current: &Project, staged: &Project) -> Result<(), String> {
    if current.id != staged.id
        || current.source_digest != staged.source_digest
        || current.manifest != staged.manifest
        || current.files != staged.files
        || current.total_bytes != staged.total_bytes
        || current.containerfile != staged.containerfile
        || current.installed != staged.installed
    {
        return Err(
            "The pocket changed while its update was building. Build the current revision again."
                .to_owned(),
        );
    }
    Ok(())
}

fn prepare_installation(
    store: &Store,
    candidate: &BuiltCandidate,
) -> Result<(Project, Project), String> {
    let current = store.load(&candidate.project.id)?;
    require_same_source(&current, &candidate.project)?;
    if let Some(installed) = &current.installed {
        if installed.manifest.storage.path != current.manifest.storage.path {
            return Err(
                "An update cannot change the data-volume mount path. Keep the installed storage path."
                    .to_owned(),
            );
        }
    }
    let revision = crate::revisions::archive_current(store, &current.id, "Build / Update")?;
    let mut next = current.clone();
    next.installed = Some(InstalledRevision {
        image_id: candidate.image_id.clone(),
        manifest: current.manifest.clone(),
        source_digest: current.source_digest.clone(),
        revision_id: Some(revision.id),
        built_at_seconds: Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "the system clock is before the Unix epoch")?
                .as_secs() as i64,
        ),
    });
    next.built_image_id = Some(candidate.image_id.clone());
    next.phase = Phase::Installed;
    next.validate()?;
    Ok((current, next))
}

async fn save_installation(
    store: &Store,
    expected: &Project,
    next: Project,
) -> Result<Project, String> {
    let root = store.root().to_path_buf();
    let expected = expected.clone();
    tokio::task::spawn_blocking(move || {
        let store = Store::new(root);
        let fresh = store.load(&expected.id)?;
        require_same_source(&fresh, &expected)?;
        let mut committed = fresh;
        committed.installed = next.installed;
        committed.built_image_id = next.built_image_id;
        committed.phase = Phase::Installed;
        store.save(&committed)?;
        // The install is committed; the version log only describes it, so
        // a failure to record it never turns a working Update into an error.
        if let Err(error) =
            crate::versions::record_install(&store, &committed, expected.installed.as_ref())
        {
            eprintln!("pockets: the version history was not updated: {error}");
        }
        Ok(committed)
    })
    .await
    .map_err(|_| "recording the installed revision was interrupted".to_owned())?
}

async fn restore_after_failure(
    bin: &Path,
    current_target: &Target,
    previous_target: &Target,
    receipt: &ReplacementReceipt,
    previous_healthcheck: &str,
    cause: String,
) -> String {
    let restored = pocket::restore_replacement(bin, current_target, previous_target, receipt).await;
    let recovery = match restored {
        Ok(status) if receipt.was_running => {
            match status.url.filter(|_| status.state == PocketState::Running) {
                Some(url) => crate::health::wait_ready(&url, previous_healthcheck).await,
                None => Err("the previous container did not return to a running state".to_owned()),
            }
        }
        Ok(_) => Ok(()),
        Err(error) => Err(error),
    };
    match recovery {
        Ok(()) => format!(
            "{cause} The previous installed version was restored. Saved data was not rolled back."
        ),
        Err(error) => format!(
            "{cause} Restoring the previous version also failed: {error}. \
             The installed record still names the previous image; the data volume was not removed."
        ),
    }
}

/// Apply a built image to the same pocket. Installed metadata is committed
/// only after a previously running app is healthy on its replacement image.
/// A stopped app stays stopped. No path here removes or restores user data.
pub(crate) async fn apply_candidate(
    store: &Store,
    bin: &Path,
    candidate: BuiltCandidate,
) -> Result<Project, String> {
    let root = store.root().to_path_buf();
    let (previous, next) = tokio::task::spawn_blocking(move || {
        prepare_installation(&Store::new(root), &candidate)
    })
    .await
    .map_err(|_| "preparing the installed revision was interrupted".to_owned())??;

    let Some(installed) = previous.installed.as_ref() else {
        // First installation still does not launch the app.
        return save_installation(store, &previous, next).await;
    };
    let previous_target = Target::managed(&installed.manifest, &previous.id, &installed.image_id)?;
    let replacement = next.installed.as_ref().ok_or("the update has no image")?;
    let next_target = Target::managed(&replacement.manifest, &next.id, &replacement.image_id)?;
    let current = pocket::inspect_target(bin, &previous_target).await?;
    if current.state == PocketState::Running {
        crate::local_sites::Site::Managed(&previous)
            .running_address(store, current.url.as_deref())?;
    }
    let receipt = pocket::replace_target(bin, &previous_target, &next_target).await?;
    let readiness = if receipt.was_running {
        match &receipt.status.url {
            Some(url) if receipt.status.state == PocketState::Running => {
                crate::health::wait_ready(url, &replacement.manifest.web.healthcheck).await
            }
            _ => Err("the updated container did not reach a running state".to_owned()),
        }
    } else {
        Ok(())
    };
    if let Err(error) = readiness {
        return Err(restore_after_failure(
            bin,
            &next_target,
            &previous_target,
            &receipt,
            &installed.manifest.web.healthcheck,
            format!("The update did not become healthy: {error}."),
        )
        .await);
    }
    match save_installation(store, &previous, next).await {
        Ok(project) => Ok(project),
        Err(error) => Err(restore_after_failure(
            bin,
            &next_target,
            &previous_target,
            &receipt,
            &installed.manifest.web.healthcheck,
            format!("The updated image could not be recorded: {error}."),
        )
        .await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Store, Project) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(
            source.join("pocket.yaml"),
            include_str!("../../../examples/hello-pocket/pocket.yaml"),
        )
        .unwrap();
        std::fs::write(source.join("Containerfile"), "FROM scratch\n").unwrap();
        let store = Store::new(temp.path().join("store"));
        let project = crate::source::import_project(&store, &source).unwrap();
        (temp, store, project)
    }

    fn candidate(project: Project, digit: char) -> BuiltCandidate {
        BuiltCandidate {
            project,
            image_id: format!("sha256:{}", digit.to_string().repeat(64)),
        }
    }

    #[tokio::test]
    async fn applying_a_first_candidate_preserves_identity_without_starting_an_engine() {
        let (_temp, store, project) = fixture();
        let installed = apply_candidate(
            &store,
            Path::new("/missing-podman"),
            candidate(project.clone(), 'a'),
        )
        .await
        .unwrap();
        assert_eq!(installed.id, project.id);
        assert_eq!(installed.source_digest, project.source_digest);
        assert_eq!(installed.phase, Phase::Installed);
        assert!(!installed.has_pending_changes());
        assert!(installed.installed.as_ref().unwrap().revision_id.is_some());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_stale_candidate_cannot_deploy_or_change_the_installed_record() {
        let (temp, store, project) = fixture();
        let installed = apply_candidate(
            &store,
            Path::new("/missing-podman"),
            candidate(project, 'a'),
        )
        .await
        .unwrap();
        let stale = candidate(installed.clone(), 'b');
        let source = temp.path().join("source");
        std::fs::write(
            source.join("extra.txt"),
            "an edit after the candidate build",
        )
        .unwrap();
        let working =
            crate::revisions::publish_working(&store, &installed.id, &source, "External edit")
                .unwrap();
        let error = apply_candidate(&store, Path::new("/missing-podman"), stale)
            .await
            .unwrap_err();
        assert!(error.contains("changed while its update was building"));
        assert_eq!(store.load(&installed.id).unwrap(), working);
        assert_eq!(working.installed, installed.installed);
    }

    #[tokio::test]
    async fn deployment_failure_does_not_commit_a_new_installed_revision() {
        let (temp, store, project) = fixture();
        let installed = apply_candidate(
            &store,
            Path::new("/missing-podman"),
            candidate(project, 'a'),
        )
        .await
        .unwrap();
        std::fs::write(temp.path().join("source/extra.txt"), "new working revision").unwrap();
        let working = crate::revisions::publish_working(
            &store,
            &installed.id,
            &temp.path().join("source"),
            "External edit",
        )
        .unwrap();
        assert!(apply_candidate(
            &store,
            Path::new("/missing-podman"),
            candidate(working.clone(), 'b')
        )
        .await
        .is_err());
        assert_eq!(store.load(&installed.id).unwrap(), working);
        assert_eq!(working.installed, installed.installed);
        assert!(working.has_pending_changes());
    }
}
