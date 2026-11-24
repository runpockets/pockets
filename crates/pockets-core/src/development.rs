//! External development folder ownership and explicit source publication.
//!
//! Only picker-selected paths enter here. Links are private local metadata,
//! separate from portable manifests and installed revisions. Every operation
//! treats external source as read-only; unlink and project deletion remove
//! only Pockets-owned metadata/snapshots. No model or runtime is needed.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::projects::{Project, Store};

const LINK_FILE: &str = "development.json";
const MAX_LINK_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FolderIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Link {
    version: u8,
    project_id: String,
    folder: Option<PathBuf>,
    identity: Option<FolderIdentity>,
    // Accept older development records and drop their grant on the next write.
    #[serde(default, skip_serializing, rename = "cliAccess")]
    _legacy_cli_access: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    preview_hosts: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevelopmentStatus {
    pub linked: bool,
    /// Deliberately disclosed user-selected development location. Managed
    /// source paths, secret locations and AI workspaces are never returned.
    pub folder: Option<String>,
    pub available: bool,
    pub error: Option<String>,
}

fn metadata_path(store: &Store, id: &str) -> Result<PathBuf, String> {
    store.load(id)?;
    Ok(store.source_path(id)?.with_file_name(LINK_FILE))
}

fn read_link(store: &Store, id: &str) -> Result<Option<Link>, String> {
    let path = metadata_path(store, id)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("The development link could not be read: {error}")),
    };
    if !metadata.is_file() || metadata.len() > MAX_LINK_BYTES {
        return Err(
            "The development link must be a bounded regular file. Relink the project.".into(),
        );
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_LINK_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("The development link could not be read: {error}"))?;
    if bytes.len() as u64 > MAX_LINK_BYTES {
        return Err("The development link is too large. Relink the project.".into());
    }
    let link: Link = serde_json::from_slice(&bytes)
        .map_err(|error| format!("The development link is invalid: {error}"))?;
    if link.version != 1
        || link.project_id != id
        || link
            .folder
            .as_ref()
            .is_some_and(|folder| !folder.is_absolute())
        || (link.folder.is_none() && (link.identity.is_some() || !link.preview_hosts.is_empty()))
        || normalize_preview_hosts(&link.preview_hosts).as_deref()
            != Ok(link.preview_hosts.as_slice())
    {
        return Err("The development link has an unsupported version, project, or folder.".into());
    }
    if let Some(folder) = &link.folder {
        reject_managed_folder(store, folder)?;
    }
    Ok(Some(link))
}

pub(crate) fn reject_managed_folder(store: &Store, folder: &Path) -> Result<(), String> {
    let managed = canonical_location(store.root())?;
    let ai = managed.parent().map(|parent| parent.join("ai"));
    if folder.starts_with(&managed)
        || managed.starts_with(folder)
        || ai.is_some_and(|ai| folder.starts_with(ai))
    {
        return Err("Choose an external project folder, outside Pockets' managed storage.".into());
    }
    Ok(())
}

/// Canonicalize the existing ancestor even before the store is allocated
/// (notably `/var` versus `/private/var` on macOS). This creates nothing.
fn canonical_location(path: &Path) -> Result<PathBuf, String> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or("The project store has no parent directory.")?;
            let name = path
                .file_name()
                .ok_or("The project store path is invalid.")?;
            Ok(canonical_location(parent)?.join(name))
        }
        Err(error) => Err(format!(
            "The project store location could not be resolved: {error}"
        )),
    }
}

#[cfg(unix)]
fn folder_identity(metadata: &std::fs::Metadata) -> Option<FolderIdentity> {
    use std::os::unix::fs::MetadataExt;
    Some(FolderIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn folder_identity(_metadata: &std::fs::Metadata) -> Option<FolderIdentity> {
    None
}

fn checked_folder(store: &Store, link: &Link) -> Result<PathBuf, String> {
    let folder = link
        .folder
        .as_ref()
        .ok_or("Link a development folder first.")?;
    reject_managed_folder(store, folder)?;
    let metadata = std::fs::symlink_metadata(folder).map_err(|_| {
        "The linked folder is unavailable. Locate it again with Relink folder.".to_owned()
    })?;
    if !metadata.is_dir() || folder_identity(&metadata) != link.identity {
        return Err(
            "The linked folder was replaced. Select its real directory with Relink folder.".into(),
        );
    }
    let canonical = std::fs::canonicalize(folder)
        .map_err(|error| format!("The linked folder could not be resolved: {error}"))?;
    if canonical != *folder {
        return Err("The linked folder now resolves elsewhere. Relink its real directory.".into());
    }
    Ok(canonical)
}

pub(crate) fn status(store: &Store, id: &str) -> Result<DevelopmentStatus, String> {
    let Some(link) = read_link(store, id)?.filter(|link| link.folder.is_some()) else {
        return Ok(DevelopmentStatus {
            linked: false,
            folder: None,
            available: false,
            error: None,
        });
    };
    let error = checked_folder(store, &link).err();
    Ok(DevelopmentStatus {
        linked: true,
        folder: link
            .folder
            .as_ref()
            .map(|folder| folder.to_string_lossy().into_owned()),
        available: error.is_none(),
        error,
    })
}

/// The linked development folder as recorded, without checking that it is
/// still available. `None` means the project has no linked folder.
pub(crate) fn linked_folder(store: &Store, id: &str) -> Result<Option<PathBuf>, String> {
    Ok(read_link(store, id)?.and_then(|link| link.folder))
}

pub(crate) fn is_linked(store: &Store, id: &str) -> Result<bool, String> {
    Ok(read_link(store, id)?.is_some_and(|link| link.folder.is_some()))
}

/// Retain the external-development designation after unlinking so legacy AI
/// can never resume an obsolete workspace and overwrite external changes.
#[cfg(test)]
pub(crate) fn is_external(store: &Store, id: &str) -> Result<bool, String> {
    Ok(read_link(store, id)?.is_some())
}

fn write_link(store: &Store, link: &Link) -> Result<(), String> {
    // Refuse corrupt/symlinked metadata rather than blessing it by replacement.
    read_link(store, &link.project_id)?;
    let path = metadata_path(store, &link.project_id)?;
    let staging = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(link).map_err(|error| error.to_string())?;
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
        std::fs::rename(&staging, &path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(staging);
        return Err(format!("The development link could not be saved: {error}"));
    }
    Ok(())
}

/// Link a folder to an existing Pocket or register it once as a new draft.
/// Linking captures an initial snapshot only for a new project. Attaching or
/// relinking an existing project does not change its working/installed source.
pub(crate) fn link(store: &Store, id: Option<&str>, selected: &Path) -> Result<Project, String> {
    let metadata = std::fs::symlink_metadata(selected)
        .map_err(|error| format!("The selected folder could not be read: {error}"))?;
    if !metadata.is_dir() {
        return Err("Select the project's real directory, not a symbolic link.".into());
    }
    let folder = std::fs::canonicalize(selected).map_err(|error| error.to_string())?;
    reject_managed_folder(store, &folder)?;
    let (_, manifest, _) = crate::source::capture_approved(&folder, "the selected folder")?;
    if let Some(id) = id {
        let project = store.load(id)?;
        if project.manifest.metadata.id != manifest.metadata.id {
            return Err(
                "The selected manifest belongs to a different Pocket. Its metadata.id must match."
                    .into(),
            );
        }
        crate::source::enforce_storage_pinned(&project, &manifest)?;
    }
    let projects = store.list()?;
    for project in &projects {
        if let Some(previous) =
            read_link(store, &project.id)?.filter(|link| link.folder.as_ref() == Some(&folder))
        {
            if id.is_some_and(|id| id != project.id) {
                return Err(
                    "This folder is already linked to another Pocket. Unlink it there first."
                        .into(),
                );
            }
            let link = Link {
                version: 1,
                project_id: project.id.clone(),
                folder: Some(folder),
                identity: folder_identity(&metadata),
                _legacy_cli_access: false,
                preview_hosts: if previous.identity == folder_identity(&metadata) {
                    previous.preview_hosts
                } else {
                    Vec::new()
                },
            };
            write_link(store, &link)?;
            return Ok(project.clone());
        }
    }
    let (project, created) = match id {
        Some(id) => {
            let project = store.load(id)?;
            (project, false)
        }
        None => (crate::source::import_project(store, &folder)?, true),
    };
    let link = Link {
        version: 1,
        project_id: project.id.clone(),
        folder: Some(folder),
        identity: folder_identity(&metadata),
        _legacy_cli_access: false,
        preview_hosts: Vec::new(),
    };
    if let Err(error) = write_link(store, &link) {
        if created {
            if let Err(cleanup) = store.remove(&project.id) {
                return Err(format!(
                    "{error} Removing the new draft also failed: {cleanup}"
                ));
            }
        }
        return Err(error);
    }
    Ok(project)
}

pub(crate) fn unlink(store: &Store, id: &str) -> Result<(), String> {
    if let Some(mut link) = read_link(store, id)? {
        if link.folder.is_some() {
            link.folder = None;
            link.identity = None;
            link.preview_hosts.clear();
            write_link(store, &link)?;
        }
    }
    Ok(())
}

fn normalize_preview_hosts(hosts: &[String]) -> Result<Vec<String>, String> {
    use crate::manifest::MAX_PREVIEW_APIS as MAX_HOSTS;
    if hosts.len() > MAX_HOSTS {
        return Err(format!("Enter up to {MAX_HOSTS} HTTPS hostnames."));
    }
    let mut approved = Vec::new();
    for input in hosts {
        let host = input.trim().to_ascii_lowercase();
        crate::manifest::validate_preview_host(&host)?;
        approved.push(host);
    }
    approved.sort();
    approved.dedup();
    Ok(approved)
}

pub(crate) fn preview_approval_scope(store: &Store, id: &str) -> Result<String, String> {
    let link = read_link(store, id)?.ok_or("Link a development folder first.")?;
    checked_folder(store, &link)?;
    serde_json::to_string(&(id, link.folder, link.identity))
        .map_err(|_| "The preview approval scope could not be read.".to_owned())
}

pub(crate) fn preview_hosts(store: &Store, id: &str) -> Result<Vec<String>, String> {
    let link = read_link(store, id)?.ok_or("Link a development folder first.")?;
    checked_folder(store, &link)?;
    Ok(link.preview_hosts)
}

pub(crate) fn set_preview_hosts(
    store: &Store,
    id: &str,
    hosts: &[String],
) -> Result<Vec<String>, String> {
    let mut link = read_link(store, id)?.ok_or("Link a development folder first.")?;
    checked_folder(store, &link)?;
    link.preview_hosts = normalize_preview_hosts(hosts)?;
    write_link(store, &link)?;
    Ok(link.preview_hosts)
}

/// Export the current verified working snapshot and attach it to the same
/// project. This leaves both the installed pin and existing history intact.
pub(crate) fn export(store: &Store, id: &str, parent: &Path) -> Result<Project, String> {
    if is_linked(store, id)? {
        return Err(
            "This Pocket already has a source folder. Use Relink folder to reconnect it.".into(),
        );
    }
    let project = store.load(id)?;
    let files = crate::source::capture_record_files(&store.source_path(id)?, &project.files)?;
    crate::source::verify_against_record("the saved source", &files, &project)?;
    let destination = crate::scaffold::write_new_folder(
        store,
        parent,
        &project.manifest.metadata.id,
        files
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice())),
    )?;
    link(store, Some(id), &destination).map_err(|error| {
        format!("{error} The saved source is available at {}. Use Link development folder on this Pocket to retry.", destination.display())
    })
}

/// Publish one filtered capture; changes made after capture remain outside
/// this revision. This function never writes to the linked source folder.
pub(crate) fn sync(store: &Store, id: &str) -> Result<Project, String> {
    let project = store.load(id)?;
    let link = read_link(store, id)?.ok_or("Link a development folder first.")?;
    let folder = checked_folder(store, &link)?;
    let (files, manifest, summary) = crate::source::capture_approved(&folder, "the linked folder")?;
    checked_folder(store, &link)?;
    if manifest.metadata.id != project.manifest.metadata.id {
        return Err("The linked manifest's metadata.id changed. Restore it before syncing.".into());
    }
    crate::source::enforce_storage_pinned(&project, &manifest)?;
    if summary.source_digest == project.source_digest {
        return Ok(project);
    }
    crate::revisions::archive_current(store, id, "Before external sync")?;
    let updated =
        crate::source::apply_captured_snapshot(store, &project, files, manifest, summary)?;
    if let Err(error) = crate::revisions::archive_current(store, id, "External source sync") {
        return Err(format!(
            "The working source was saved, but its new revision could not be archived: {error}"
        ));
    }
    Ok(updated)
}

pub(crate) fn folder(store: &Store, id: &str) -> Result<PathBuf, String> {
    let link = read_link(store, id)?.ok_or("Link a development folder first.")?;
    checked_folder(store, &link)
}

/// True when the verified linked folder captures cleanly to a different
/// source digest than `project.source_digest`. Missing, moved, or mid-edit
/// invalid folders quietly return `false` so listing never fails or guesses.
pub(crate) fn has_unsynced_changes(store: &Store, project: &Project) -> bool {
    let Ok(Some(link)) = read_link(store, &project.id) else {
        return false;
    };
    let Ok(folder) = checked_folder(store, &link) else {
        return false;
    };
    let Ok((_, _, summary)) = crate::source::capture_approved(&folder, "the linked folder") else {
        return false;
    };
    summary.source_digest != project.source_digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::{InstalledRevision, Phase};

    fn fixture() -> (tempfile::TempDir, Store, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("external project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("pocket.yaml"), "apiVersion: pockets.dev/v1alpha1\nkind: Pocket\nmetadata:\n  id: tracker\n  name: Tracker\n  version: 0.1.0\n  description: A portfolio tracker\nruntime:\n  image: localhost/tracker:0.1.0\n  architectures: [arm64, amd64]\nweb:\n  port: 8080\n  healthcheck: /health\nstorage:\n  name: data\n  path: /data\n").unwrap();
        std::fs::write(
            root.join("Containerfile"),
            "FROM scratch\nCOPY server.js /server.js\n",
        )
        .unwrap();
        std::fs::write(root.join("server.js"), "first version").unwrap();
        let store = Store::new(temp.path().join("app-data/projects"));
        (temp, store, root)
    }

    #[test]
    fn links_persist_without_duplicate_projects_or_portable_absolute_paths() {
        let (_temp, store, root) = fixture();
        let first = link(&store, None, &root).unwrap();
        let reopened = Store::new(store.root().to_owned());
        let second = link(&reopened, None, &root).unwrap();
        assert_eq!(first, second);
        assert_eq!(store.list().unwrap().len(), 1);
        let status = status(&reopened, &first.id).unwrap();
        assert!(status.linked && status.available);
        assert_eq!(
            status.folder,
            Some(
                std::fs::canonicalize(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert!(!serde_json::to_string(&first)
            .unwrap()
            .contains(root.to_str().unwrap()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(metadata_path(&store, &first.id).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn saved_links_with_legacy_cli_grants_keep_their_folder_and_api_approvals() {
        let (_temp, store, root) = fixture();
        let project = link(&store, None, &root).unwrap();
        set_preview_hosts(&store, &project.id, &["api.example.com".into()]).unwrap();
        let path = metadata_path(&store, &project.id).unwrap();
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["cliAccess"] = serde_json::Value::Bool(true);
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();

        assert_eq!(
            folder(&store, &project.id).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
        assert_eq!(
            preview_hosts(&store, &project.id).unwrap(),
            ["api.example.com"]
        );
        assert_eq!(link(&store, Some(&project.id), &root).unwrap(), project);
        let rewritten: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(rewritten.get("cliAccess").is_none());
        assert_eq!(
            preview_hosts(&store, &project.id).unwrap(),
            ["api.example.com"]
        );
    }

    #[test]
    fn preview_network_authorization_is_local_and_resets_when_relinked() {
        let (temp, store, root) = fixture();
        let project = link(&store, None, &root).unwrap();
        assert!(preview_hosts(&store, &project.id).unwrap().is_empty());
        assert_eq!(
            set_preview_hosts(
                &store,
                &project.id,
                &["API.Example.com".into(), "api.example.com".into()]
            )
            .unwrap(),
            ["api.example.com"]
        );
        for invalid in [
            "127.0.0.1",
            "api.example.com:443",
            "https://api.example.com",
            "*.example.com",
            "localhost",
            "printer.local",
            "router.home.arpa",
        ] {
            assert!(set_preview_hosts(&store, &project.id, &[invalid.into()]).is_err());
        }
        assert_eq!(link(&store, Some(&project.id), &root).unwrap(), project);
        assert_eq!(
            preview_hosts(&store, &project.id).unwrap(),
            ["api.example.com"]
        );
        assert!(!serde_json::to_string(&project)
            .unwrap()
            .contains("api.example.com"));
        let relocated = temp.path().join("relocated");
        std::fs::rename(&root, &relocated).unwrap();
        link(&store, Some(&project.id), &relocated).unwrap();
        assert!(preview_hosts(&store, &project.id).unwrap().is_empty());
    }

    #[test]
    fn exports_saved_source_in_place_and_refuses_to_overwrite_an_existing_folder() {
        let (temp, store, root) = fixture();
        let project = crate::source::import_project(&store, &root).unwrap();
        let baseline =
            crate::revisions::archive_current(&store, &project.id, "Saved source").unwrap();
        let target = temp.path().join("tracker");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("server.js"), "existing source").unwrap();
        assert!(export(&store, &project.id, temp.path())
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(
            std::fs::read_to_string(target.join("server.js")).unwrap(),
            "existing source"
        );
        assert!(!is_linked(&store, &project.id).unwrap());
        let parent = temp.path().join("new parent");
        std::fs::create_dir(&parent).unwrap();
        assert_eq!(export(&store, &project.id, &parent).unwrap(), project);
        let exported = folder(&store, &project.id).unwrap();
        assert_eq!(
            std::fs::read_to_string(exported.join("server.js")).unwrap(),
            "first version"
        );
        assert_eq!(
            crate::revisions::list(&store, &project.id).unwrap(),
            vec![baseline]
        );
        assert_eq!(store.list().unwrap(), vec![project]);
    }

    #[test]
    fn external_sync_preserves_installed_identity_storage_and_user_data() {
        let (_temp, store, root) = fixture();
        let mut project = link(&store, None, &root).unwrap();
        let image = format!("sha256:{}", "a".repeat(64));
        project.phase = Phase::Installed;
        project.built_image_id = Some(image.clone());
        project.installed = Some(InstalledRevision {
            image_id: image,
            manifest: project.manifest.clone(),
            source_digest: project.source_digest.clone(),
            revision_id: None,
            built_at_seconds: None,
        });
        store.save(&project).unwrap();
        std::fs::create_dir(root.join("data")).unwrap();
        std::fs::write(root.join("data/holdings.json"), "private holdings").unwrap();
        std::fs::write(root.join(".env"), "SECRET=never-copy").unwrap();
        std::fs::write(root.join("server.js"), "external changes").unwrap();
        let synced = sync(&store, &project.id).unwrap();
        assert_eq!(synced.id, project.id);
        assert_eq!(synced.installed, project.installed);
        assert_eq!(synced.built_image_id, project.built_image_id);
        assert_eq!(synced.phase, Phase::Installed);
        assert!(synced.has_pending_changes());
        assert!(!synced
            .files
            .iter()
            .any(|file| file.contains("holdings") || file == ".env"));
        assert_eq!(
            std::fs::read_to_string(root.join("data/holdings.json")).unwrap(),
            "private holdings"
        );
        assert_eq!(
            std::fs::read_to_string(store.source_path(&project.id).unwrap().join("server.js"))
                .unwrap(),
            "external changes"
        );
        assert_eq!(
            crate::revisions::list(&store, &project.id).unwrap().len(),
            2
        );
        sync(&store, &project.id).unwrap();
        assert_eq!(
            crate::revisions::list(&store, &project.id).unwrap().len(),
            2
        );
    }

    #[test]
    fn missing_folders_relink_without_replacing_the_snapshot() {
        let (temp, store, root) = fixture();
        let project = link(&store, None, &root).unwrap();
        let moved = temp.path().join("moved project");
        std::fs::rename(&root, &moved).unwrap();
        assert!(!status(&store, &project.id).unwrap().available);
        assert!(sync(&store, &project.id).is_err());
        std::fs::write(moved.join("server.js"), "not synced yet").unwrap();
        assert_eq!(link(&store, Some(&project.id), &moved).unwrap(), project);
        assert!(status(&store, &project.id).unwrap().available);
        assert_eq!(store.load(&project.id).unwrap(), project);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn invalid_edits_and_changed_app_identity_do_not_replace_good_source() {
        let (_temp, store, root) = fixture();
        let project = link(&store, None, &root).unwrap();
        let manifest = std::fs::read_to_string(root.join("pocket.yaml")).unwrap();
        std::fs::write(root.join("pocket.yaml"), "incomplete: [").unwrap();
        assert!(sync(&store, &project.id).is_err());
        assert_eq!(store.load(&project.id).unwrap(), project);
        std::fs::write(
            root.join("pocket.yaml"),
            manifest.replace("id: tracker", "id: another-app"),
        )
        .unwrap();
        assert!(sync(&store, &project.id)
            .unwrap_err()
            .contains("metadata.id"));
        assert_eq!(store.load(&project.id).unwrap(), project);
        assert!(crate::revisions::list(&store, &project.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn one_folder_cannot_be_owned_by_two_pockets() {
        let (_temp, store, root) = fixture();
        let owner = link(&store, None, &root).unwrap();
        let other = crate::source::import_project(&store, &root).unwrap();
        assert!(link(&store, Some(&other.id), &root)
            .unwrap_err()
            .contains("another Pocket"));
        assert!(status(&store, &owner.id).unwrap().linked);
        assert!(!status(&store, &other.id).unwrap().linked);
    }

    #[test]
    fn unlink_and_project_deletion_never_remove_external_source() {
        let (_temp, store, root) = fixture();
        let project = link(&store, None, &root).unwrap();
        unlink(&store, &project.id).unwrap();
        unlink(&store, &project.id).unwrap();
        assert!(!status(&store, &project.id).unwrap().linked);
        assert!(is_external(&store, &project.id).unwrap());
        let record = read_link(&store, &project.id).unwrap().unwrap();
        assert!(record.folder.is_none() && record.identity.is_none());
        assert_eq!(store.load(&project.id).unwrap(), project);
        link(&store, Some(&project.id), &root).unwrap();
        store.remove(&project.id).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("server.js")).unwrap(),
            "first version"
        );
        assert!(root.join("pocket.yaml").is_file());
    }

    #[test]
    fn managed_directories_cannot_be_linked_as_external_projects() {
        let (temp, store, root) = fixture();
        assert!(link(&store, None, temp.path()).is_err());
        let project = link(&store, None, &root).unwrap();
        assert!(link(&store, None, &store.source_path(&project.id).unwrap()).is_err());
        assert!(status(&store, "../escape").is_err());
    }

    #[test]
    fn changed_installed_storage_is_refused_before_any_source_or_history_write() {
        let (_temp, store, root) = fixture();
        let mut project = link(&store, None, &root).unwrap();
        project.built_image_id = Some(format!("sha256:{}", "a".repeat(64)));
        project.phase = Phase::Installed;
        store.save(&project).unwrap();
        let before = store.load(&project.id).unwrap();
        let manifest = std::fs::read_to_string(root.join("pocket.yaml")).unwrap();
        std::fs::write(
            root.join("pocket.yaml"),
            manifest.replace("path: /data", "path: /other-data"),
        )
        .unwrap();
        assert!(sync(&store, &project.id).unwrap_err().contains("pinned"));
        assert_eq!(store.load(&project.id).unwrap(), before);
        assert!(crate::revisions::list(&store, &project.id)
            .unwrap()
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn replaced_folders_and_symlinked_metadata_are_refused() {
        use std::os::unix::fs::symlink;
        let (temp, store, root) = fixture();
        let project = link(&store, None, &root).unwrap();
        std::fs::rename(&root, temp.path().join("original")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert!(!status(&store, &project.id).unwrap().available);
        assert!(sync(&store, &project.id).is_err());
        let metadata = metadata_path(&store, &project.id).unwrap();
        std::fs::remove_file(&metadata).unwrap();
        let victim = temp.path().join("private.json");
        std::fs::write(&victim, "private").unwrap();
        symlink(&victim, &metadata).unwrap();
        assert!(status(&store, &project.id).is_err());
        assert!(unlink(&store, &project.id).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "private");
    }

    #[test]
    fn has_unsynced_changes_tracks_live_edits_and_ignores_invalid_folders() {
        let (_temp, store, root) = fixture();
        let project = link(&store, None, &root).unwrap();
        assert!(!has_unsynced_changes(&store, &project));

        std::fs::write(root.join("server.js"), "second version").unwrap();
        assert!(has_unsynced_changes(&store, &project));

        let synced = sync(&store, &project.id).unwrap();
        assert!(!has_unsynced_changes(&store, &synced));

        // Mid-edit invalid YAML returns false rather than erroring list_projects.
        std::fs::write(root.join("pocket.yaml"), "metadata: [broken").unwrap();
        assert!(!has_unsynced_changes(&store, &synced));
    }
}
