//! Persistent browser addresses for directly published, installed Pockets.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, TcpListener};

use serde::{Deserialize, Serialize};

use crate::manifest;
use crate::pocket::loopback_url_port;
use crate::projects::{Project, Store};

const BUNDLED_HOST: &str = "hello-pocket.localhost";

#[derive(Clone, Copy)]
pub(crate) enum Site<'a> {
    Bundled,
    Managed(&'a Project),
}

// Existing registries map a hostname to just the installation ID. Upgrade a
// row when its Pocket is used. Retain retired names to protect browser data.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Assignment {
    Legacy(String),
    Pinned {
        id: String,
        port: std::num::NonZeroU16,
    },
}

impl Assignment {
    fn id(&self) -> &str {
        match self {
            Self::Legacy(id) | Self::Pinned { id, .. } => id,
        }
    }
}

pub(crate) struct Address {
    pub host: String,
    pub port: u16,
}

impl Address {
    pub fn url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Stable DNS-safe slug (e.g. `"notes"` or `"notes-2"`) stripped of `.localhost`.
    pub fn slug(&self) -> Option<&str> {
        self.host.strip_suffix(".localhost")
    }
}

/// Looks up the saved local site slug (`"notes"`, `"notes-2"`, ...) for a
/// managed project, falling back to its validated installed manifest id when
/// the Pocket has not been started yet.
pub(crate) fn slug_for_project(store: &Store, project: &Project) -> String {
    let path = store.root().with_file_name("local-sites.json");
    if let Ok(bytes) = fs::read(&path) {
        if let Ok(sites) = serde_json::from_slice::<BTreeMap<String, Assignment>>(&bytes) {
            if let Some((host, _)) = sites.iter().find(|(_, row)| row.id() == project.id) {
                if let Some(label) = host.strip_suffix(".localhost") {
                    if manifest::validate_id("local site", label).is_ok() {
                        return label.to_owned();
                    }
                }
            }
        }
    }
    project.installed_manifest().metadata.id.clone()
}

/// Looks up the pinned host loopback port for a managed project from
/// `local-sites.json`, if it has been assigned one.
pub(crate) fn pinned_port_for_project(store: &Store, project_id: &str) -> Option<u16> {
    let path = store.root().with_file_name("local-sites.json");
    let bytes = fs::read(&path).ok()?;
    let sites = serde_json::from_slice::<BTreeMap<String, Assignment>>(&bytes).ok()?;
    sites.values().find_map(|row| match row {
        Assignment::Pinned { id, port } if id == project_id => Some(port.get()),
        _ => None,
    })
}

impl Site<'_> {
    pub fn running_address(self, store: &Store, mapping: Option<&str>) -> Result<Address, String> {
        let port = mapping
            .and_then(loopback_url_port)
            .ok_or("The running Pocket has no verified loopback port.")?;
        self.address(store, Some(port))
    }

    // With no mapping this is called only when preparing a start. A legacy
    // running Pocket keeps the port already verified by ownership inspection.
    pub fn address(self, store: &Store, observed: Option<u16>) -> Result<Address, String> {
        let path = store.root().with_file_name("local-sites.json");
        let io_error = |err| format!("Could not access the Pocket address registry: {err}");
        fs::create_dir_all(path.parent().ok_or("Missing app data directory.")?)
            .map_err(io_error)?;
        // The lifecycle lock serializes commands within Desktop. Serialize
        // this read/modify/write across Desktop processes as well.
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("lock"))
            .map_err(io_error)?;
        lock.lock()
            .map_err(|err| format!("Cannot lock Pocket addresses. Retry the operation. {err}"))?;
        let mut sites: BTreeMap<String, Assignment> = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|err| format!("Invalid Pocket address registry: {err}"))?,
            Err(err) if err.kind() == ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => return Err(io_error(err)),
        };

        let (id, slug) = match self {
            Self::Bundled => ("bundled", "hello-pocket"),
            Self::Managed(project) => (
                project.id.as_str(),
                project.installed_manifest().metadata.id.as_str(),
            ),
        };
        let existing = sites.iter().find(|(_, row)| row.id() == id);
        let host = match existing {
            Some((host, _)) => host.clone(),
            None if matches!(self, Self::Bundled) => BUNDLED_HOST.to_owned(),
            None => available_host(slug, &sites),
        };
        let label = host
            .strip_suffix(".localhost")
            .ok_or("Invalid saved local hostname.")?;
        manifest::validate_id("local site", label)?;
        if let Some((_, Assignment::Pinned { port, .. })) = existing {
            let port = port.get();
            if observed.is_none() && !port_available(port)? {
                return Err(format!("Local port {port} is in use. Free that port and start the Pocket again to use {host}:{port}."));
            }
            if observed.is_some_and(|actual| actual != port) {
                return Err(format!("The running Pocket is mapped to a different port. Stop it in Pockets and start it again to restore {host}:{port}."));
            }
            return Ok(Address { host, port });
        }

        let reserved: BTreeSet<_> = sites
            .values()
            .filter_map(|row| match row {
                Assignment::Pinned { port, .. } => Some(port.get()),
                Assignment::Legacy(_) => None,
            })
            .collect();
        let port = match observed {
            Some(port) if reserved.contains(&port) => return Err(format!("Port {port} belongs to another installation. Stop this Pocket and start it again to assign its own port.")),
            Some(port) => port,
            None => available_port(&reserved)?,
        };
        sites.insert(
            host.clone(),
            Assignment::Pinned {
                id: id.to_owned(),
                port: port.try_into().map_err(|_| "Invalid local port.")?,
            },
        );
        let staged = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
        let bytes = serde_json::to_vec(&sites).map_err(|err| err.to_string())?;
        if let Err(err) = fs::write(&staged, bytes).and_then(|_| fs::rename(&staged, &path)) {
            let _ = fs::remove_file(staged);
            return Err(io_error(err));
        }
        Ok(Address { host, port })
    }
}

fn available_host(slug: &str, sites: &BTreeMap<String, Assignment>) -> String {
    let mut host = format!("{slug}.localhost");
    let mut number = 2;
    while host == BUNDLED_HOST || sites.contains_key(&host) {
        let suffix = format!("-{number}");
        let len = slug.len().min(manifest::MAX_ID_LEN - suffix.len());
        host = format!("{}{suffix}.localhost", slug[..len].trim_end_matches('-'));
        number += 1;
    }
    host
}

fn available_port(reserved: &BTreeSet<u16>) -> Result<u16, String> {
    for port in 18788..=32767 {
        if reserved.contains(&port) {
            continue;
        }
        if port_available(port)? {
            return Ok(port);
        }
    }
    Err("No available Pocket ports in the range 18788–32767.".into())
}

// A Mac listener can occupy a port even if it is free inside the Podman VM.
fn port_available(port: u16) -> Result<bool, String> {
    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == ErrorKind::AddrInUse => Ok(false),
        Err(err) => Err(format!("Could not check local port {port}: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(store: &Store, project: &Project) -> Result<String, String> {
        Ok(Site::Managed(project).address(store, None)?.host)
    }
    fn project(store: &Store, slug: &str) -> Project {
        let mut manifest = manifest::bundled_manifest().unwrap();
        manifest.metadata.id = slug.to_owned();
        store
            .create(
                manifest,
                crate::projects::SourceSummary {
                    files: vec!["Containerfile".into()],
                    total_bytes: 13,
                    containerfile: "FROM scratch\n".into(),
                    source_digest: "a".repeat(64),
                },
            )
            .unwrap()
    }

    #[test]
    fn site_names_are_stable_and_never_reassigned_to_other_projects() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("projects"));
        let first = project(&store, "notes");
        let second = project(&store, "notes");
        assert_eq!(host(&store, &first).unwrap(), "notes.localhost");
        assert_eq!(host(&store, &second).unwrap(), "notes-2.localhost");

        let store = Store::new(store.root().to_path_buf());
        store.remove(&first.id).unwrap();
        assert_eq!(host(&store, &second).unwrap(), "notes-2.localhost");
        let third = project(&store, "notes");
        assert_eq!(host(&store, &third).unwrap(), "notes-3.localhost");
    }

    #[test]
    fn upgrades_legacy_names_and_checks_the_recorded_port() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("projects"));
        let first = project(&store, "notes");
        let second = project(&store, "notes");
        fs::write(
            temp.path().join("local-sites.json"),
            serde_json::json!({
                "notes.localhost": "retired", "notes-2.localhost": first.id
            })
            .to_string(),
        )
        .unwrap();

        let site = Site::Managed(&first);
        let address = site
            .running_address(&store, Some("http://127.0.0.1:45278"))
            .unwrap();
        assert_eq!(address.url(), "http://notes-2.localhost:45278");
        assert!(site
            .running_address(&store, Some("http://127.0.0.1:45279"))
            .is_err());
        assert!(Site::Managed(&second)
            .running_address(&store, Some("http://127.0.0.1:45278"))
            .is_err());
        let next = Site::Managed(&second).address(&store, None).unwrap();
        assert_eq!(next.host, "notes-3.localhost");
        assert_ne!(next.port, address.port);
    }

    #[test]
    fn reserves_the_bundled_name_and_keeps_duplicate_labels_within_dns_limits() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("projects"));
        let bundled_copy = project(&store, "hello-pocket");
        assert_eq!(
            host(&store, &bundled_copy).unwrap(),
            "hello-pocket-2.localhost"
        );

        // Truncation for the numeric suffix can land on a hyphen.
        let prefix = "a".repeat(60);
        let slug = format!("{prefix}-bc");
        let first = project(&store, &slug);
        let second = project(&store, &slug);
        assert_eq!(host(&store, &first).unwrap(), format!("{slug}.localhost"));
        assert_eq!(
            host(&store, &second).unwrap(),
            format!("{prefix}-2.localhost")
        );
    }
}
