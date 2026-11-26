//! Lifecycle inspection, creation, stopping, and removal of
//! Pockets-managed containers.
//!
//! Follows `docs/architecture.md`: state is reconciled from Podman, never
//! from frontend state. Mutations are deliberately narrow — creating the
//! dedicated data volume, a fresh `podman run` from fixed, secure
//! arguments, stopping the owned container, and removing the owned
//! container and data volume of a project being deleted — and every
//! container mutation targets the inspected container ID, never a name.
//! Removal is the narrowest mutation here: [`remove_target`] and
//! [`remove_volume`] refuse [`Target::Bundled`] outright and enforce the
//! ownership label before deleting anything, so neither the bundled
//! example nor a foreign container or volume is ever removed, no saved
//! container configuration is ever restarted, and the Podman machine and
//! unrelated containers are never touched.
//!
//! Images are deliberately never removed, by this module or by a project
//! deletion. A built image is content-addressed, so two pockets built
//! from identical sources share one digest, and deleting the digest of
//! the project being deleted could destroy an image another pocket still
//! runs from. A deletion therefore leaves its image in the container
//! store — a deliberate leak bounded by the number of distinct sources
//! built, and owned by the engine's own pruning — rather than guess at
//! who else shares it. The module drives `podman` exclusively through
//! fixed argument arrays via [`runtime::run`].
//!
//! An explicit update of a managed project swaps its container in place
//! through [`replace_target`]: same project id, name, data volume, and
//! engine-validated loopback host port, with the candidate image already
//! in the local store — and [`restore_replacement`] puts the previous
//! container back when the higher layer's health wait or record commit
//! fails. Neither ever builds or pulls an image, waits on health, writes
//! a record, or removes a volume.
//!
//! Every lifecycle function operates on a [`Target`]: either the fixed
//! bundled example — reached through the [`inspect`], [`start`], and
//! [`stop`] wrappers used by the current command surface — or a
//! native-managed project built by [`Target::managed`].
//! A target is never deserialized and its fields are private: the name,
//! volume, ownership label, image, port, and mount path are fixed by the
//! constructors, so arbitrary arguments, paths, or identities can never
//! arrive from the frontend.
//!
//! Identity is the target's exact container name. A name match alone is
//! not ownership: the container must also carry the target's ownership
//! label under [`OWNERSHIP_LABEL`] (`hello-pocket` for the bundled
//! example, `project-<id>` for a managed project), and a foreign
//! container or volume with the managed name is refused. A URL is only
//! ever derived from a single, engine-reported `127.0.0.1` mapping of
//! the target's own container port; arbitrary or wildcard mappings are
//! rejected instead of guessed.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::manifest::{self, Manifest};
use crate::runtime;

/// Fixed Podman container name of the bundled example.
pub const CONTAINER_NAME: &str = "pockets-hello-pocket";

/// Ownership label key written on every container Pockets manages.
pub(crate) const OWNERSHIP_LABEL: &str = "dev.pockets.managed";

/// Required value of [`OWNERSHIP_LABEL`] for the bundled example.
pub(crate) const OWNERSHIP_VALUE: &str = "hello-pocket";

/// Required value of [`OWNERSHIP_LABEL`] for a managed project, prefixed
/// to the validated project ID: `project-<id>`.
pub(crate) const MANAGED_OWNERSHIP_PREFIX: &str = "project-";

/// Container name prefix for a managed project, prefixed to the
/// validated project ID: `pockets-project-<id>`.
pub(crate) const MANAGED_NAME_PREFIX: &str = "pockets-project-";

/// The only host address a port mapping may be published on.
const LOOPBACK_IP: &str = "127.0.0.1";

/// Container port of the bundled example, in the `<port>/<protocol>`
/// key form used by `podman inspect` port maps. Mirrors the embedded
/// manifest's `web.port`; the create-args test pins the two together.
const BUNDLED_PORT_KEY: &str = "8080/tcp";
const BUNDLED_PORT: u16 = 8080;

/// Container mount path of the bundled example's data volume. Mirrors
/// the embedded manifest's `storage.path`.
const BUNDLED_STORAGE_PATH: &str = "/data";

/// Image reference of the bundled example. Mirrors the embedded
/// manifest's `runtime.image`.
const BUNDLED_IMAGE: &str = "localhost/pockets-hello:0.1.0";

/// Dedicated data volume of the bundled example, spelled as
/// `pockets-<metadata.id>-<storage.name>` resolves for the embedded
/// manifest.
const BUNDLED_VOLUME: &str = "pockets-hello-pocket-data";

/// Required length of a managed project ID: 32 lowercase hex characters.
const PROJECT_ID_LEN: usize = 32;

/// Required prefix of a managed image ID, followed by exactly
/// [`IMAGE_ID_HEX_LEN`] lowercase hex characters.
const IMAGE_ID_PREFIX: &str = "sha256:";

/// Required hex length of the digest part of a managed image ID.
const IMAGE_ID_HEX_LEN: usize = 64;

/// A fully validated lifecycle target: everything the lifecycle
/// functions may vary — exact container name, ownership label value,
/// dedicated data volume, image reference, internal port, and container
/// mount path — for the one container a call is allowed to control.
///
/// Deliberately not `Deserialize` and its fields are unreachable from
/// outside this module: the only ways to obtain one are
/// [`Target::bundled`] and [`Target::managed`], both of which fix every
/// field from native-controlled, validated inputs. The frontend never
/// names a container, volume, image, port, or path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The fixed bundled example, mirroring the embedded manifest.
    Bundled,
    /// A native-managed project pinned to an immutable image digest.
    Managed(ManagedTarget),
}

/// Identity and manifest-derived fields of a native-managed project.
/// All fields are private and every path into this variant goes through
/// [`Target::managed`], which validates the project ID, image digest,
/// and manifest; outside this module the value is opaque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedTarget {
    /// Project identifier: exactly 32 lowercase hexadecimal characters.
    project_id: String,
    /// Immutable image digest (`sha256:<64 lowercase hex>`), overriding
    /// any mutable tag the manifest carries.
    image_id: String,
    /// Internal container port, from `web.port`.
    port: u16,
    /// Container mount path, from `storage.path`.
    storage_path: String,
}

impl Target {
    /// The fixed bundled example: name `pockets-hello-pocket`,
    /// ownership `hello-pocket`, volume `pockets-hello-pocket-data`,
    /// and the embedded manifest's port, mount path, and image.
    pub fn bundled() -> Self {
        Target::Bundled
    }

    /// A native-managed project target.
    ///
    /// `project_id` must be exactly 32 lowercase hexadecimal characters
    /// and `image_id` exactly `sha256:` plus 64 lowercase hexadecimal
    /// characters; the manifest must pass [`Manifest::validate`] and
    /// [`Manifest::validate_host`]. The target then derives its identity
    /// from the project ID alone — container `pockets-project-<id>`,
    /// volume `pockets-project-<id>-data`, ownership label
    /// `dev.pockets.managed=project-<id>` — and takes only the internal
    /// port (`web.port`) and container mount path (`storage.path`) from
    /// the manifest. The run image is the immutable `image_id`, which
    /// overrides the manifest's mutable `runtime.image` tag.
    pub fn managed(manifest: &Manifest, project_id: &str, image_id: &str) -> Result<Self, String> {
        if !is_lower_hex(project_id, PROJECT_ID_LEN) {
            return Err(format!(
                "project id must be exactly {PROJECT_ID_LEN} lowercase hexadecimal characters, found {project_id:?}"
            ));
        }
        let digest = image_id.strip_prefix(IMAGE_ID_PREFIX).ok_or_else(|| {
            format!(
                "image id must be an immutable `{IMAGE_ID_PREFIX}<{IMAGE_ID_HEX_LEN} lowercase hex>` digest, found {image_id:?}"
            )
        })?;
        if !is_lower_hex(digest, IMAGE_ID_HEX_LEN) {
            return Err(format!(
                "image id must be `{IMAGE_ID_PREFIX}` followed by exactly {IMAGE_ID_HEX_LEN} lowercase hexadecimal characters, found {image_id:?}"
            ));
        }
        manifest.validate()?;
        manifest.validate_host()?;
        Ok(Target::Managed(ManagedTarget {
            project_id: project_id.to_owned(),
            image_id: image_id.to_owned(),
            port: manifest.web.port,
            storage_path: manifest.storage.path.clone(),
        }))
    }

    /// The exact Podman container name of this target.
    fn name(&self) -> String {
        match self {
            Target::Bundled => CONTAINER_NAME.to_owned(),
            Target::Managed(managed) => format!("{MANAGED_NAME_PREFIX}{}", managed.project_id),
        }
    }

    /// The required value of [`OWNERSHIP_LABEL`] for this target.
    fn ownership_value(&self) -> String {
        match self {
            Target::Bundled => OWNERSHIP_VALUE.to_owned(),
            Target::Managed(managed) => format!("{MANAGED_OWNERSHIP_PREFIX}{}", managed.project_id),
        }
    }

    /// The `--label` argument asserting this target's ownership.
    fn ownership_label_arg(&self) -> String {
        format!("{}={}", OWNERSHIP_LABEL, self.ownership_value())
    }

    /// The dedicated named data volume for this target.
    fn volume(&self) -> String {
        match self {
            Target::Bundled => BUNDLED_VOLUME.to_owned(),
            Target::Managed(managed) => {
                format!("{MANAGED_NAME_PREFIX}{}-data", managed.project_id)
            }
        }
    }

    /// The container port this target publishes.
    fn port(&self) -> u16 {
        match self {
            Target::Bundled => BUNDLED_PORT,
            Target::Managed(managed) => managed.port,
        }
    }

    /// The container mount path of the data volume.
    fn storage_path(&self) -> String {
        match self {
            Target::Bundled => BUNDLED_STORAGE_PATH.to_owned(),
            Target::Managed(managed) => managed.storage_path.clone(),
        }
    }

    /// The exact image reference the container is created from. For a
    /// managed project this is the immutable digest, never a tag.
    fn image(&self) -> &str {
        match self {
            Target::Bundled => BUNDLED_IMAGE,
            Target::Managed(managed) => &managed.image_id,
        }
    }

    /// The `<port>/<protocol>` key this target's mapping is reported
    /// under in `podman inspect` port maps.
    fn port_key(&self) -> String {
        match self {
            Target::Bundled => BUNDLED_PORT_KEY.to_owned(),
            Target::Managed(managed) => format!("{}/tcp", managed.port),
        }
    }

    fn runtime_scope(&self) -> Result<Option<crate::runtime_env::Scope>, String> {
        match self {
            Self::Bundled => Ok(None),
            Self::Managed(target) => crate::runtime_env::Scope::new(
                &target.project_id,
                crate::runtime_settings::Environment::Installed,
            )
            .map(Some),
        }
    }

    /// Fixed isolation flags and a Podman-published persistent loopback port.
    fn run_args(&self, host_port: u16) -> Vec<String> {
        let publish = format!("{LOOPBACK_IP}:{host_port}:{}", self.port());
        vec![
            "run".to_owned(),
            "--detach".to_owned(),
            "--name".to_owned(),
            self.name(),
            "--label".to_owned(),
            self.ownership_label_arg(),
            "--pull=never".to_owned(),
            "--cap-drop=ALL".to_owned(),
            "--security-opt=no-new-privileges".to_owned(),
            "--publish".to_owned(),
            publish,
            "--volume".to_owned(),
            format!("{}:{}", self.volume(), self.storage_path()),
            self.image().to_owned(),
        ]
    }

    /// Host preflight before any mutation. The bundled manifest is
    /// embedded, so it is re-validated against the host here exactly as
    /// before; a managed target was already validated by
    /// [`Target::managed`] and re-validates nothing at this boundary.
    fn validate_start(&self) -> Result<(), String> {
        match self {
            Target::Bundled => {
                let manifest = manifest::bundled_manifest()?;
                manifest.validate_host()
            }
            Target::Managed(_) => Ok(()),
        }
    }

    /// Appends the exact development build command when a bundled run
    /// failure is a missing image, which `--pull=never` surfaces instead
    /// of a registry pull. A managed digest is imported by the project
    /// pipeline, not built from the example directory, so its error is
    /// returned unmodified.
    fn missing_image_hint(&self, err: String) -> String {
        match self {
            Target::Bundled => with_build_hint(err, self.image()),
            Target::Managed(_) => err,
        }
    }

    /// Validates one `podman inspect` result against this target. Exact
    /// name, argument-safe ID, and target ownership are always enforced.
    /// `require_mapping` adds the safe-mapping rule when the caller
    /// needs a URL; without it, an absent or unsafe mapping simply
    /// yields no URL (stop never uses one). Pure so fixtures can test
    /// it.
    fn parse_inspect(
        &self,
        stdout: &[u8],
        require_mapping: bool,
    ) -> Result<OwnedContainer, String> {
        let name = self.name();
        let entries: Vec<InspectEntry> = serde_json::from_slice(stdout)
            .map_err(|err| format!("could not parse `podman inspect` output: {err}"))?;
        let [entry] = entries.as_slice() else {
            return Err(format!(
                "`podman inspect` returned {} results for {name}, expected exactly one",
                entries.len()
            ));
        };

        // Podman reports bare names; some engines prefix a slash.
        let inspected = entry.name.strip_prefix('/').unwrap_or(&entry.name);
        if inspected != name {
            return Err(format!(
                "`podman inspect` returned container {:?}, which is not {name:?}",
                entry.name
            ));
        }

        // The ID is handed to lifecycle commands later; keep it argument-safe
        // before it ever leaves this function.
        if !is_safe_container_id(&entry.id) {
            return Err(format!(
                "`podman inspect` reported an unusable container id for {name}"
            ));
        }

        let ownership = self.ownership_value();
        let owned = entry
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .and_then(|labels| labels.get(OWNERSHIP_LABEL))
            .is_some_and(|value| *value == ownership);
        if !owned {
            return Err(format!(
                "container {name} exists but is not managed by Pockets \
                 (expected label {OWNERSHIP_LABEL}={ownership}); refusing to control it"
            ));
        }

        let running = entry.state.as_ref().is_some_and(|state| state.running);
        let port = loopback_port(
            entry
                .network_settings
                .as_ref()
                .and_then(|settings| settings.ports.as_ref()),
            &self.port_key(),
        );
        let url = match port {
            Some(port) => Some(format!("http://{LOOPBACK_IP}:{port}")),
            None if running && require_mapping => {
                return Err(format!(
                    "container {name} is running but publishes no single {} mapping on {LOOPBACK_IP}; refusing to guess an address",
                    self.port_key()
                ))
            }
            // Stopped containers legitimately have no host mapping yet, and
            // mapping-free lookups (stop) never use the URL.
            None => None,
        };

        Ok(OwnedContainer {
            id: entry.id.clone(),
            running,
            url,
            runtime_bindings: entry
                .config
                .as_ref()
                .and_then(|config| config.labels.as_ref())
                .and_then(|labels| labels.get(crate::runtime_env::BINDINGS_LABEL))
                .cloned(),
        })
    }
}

/// Lifecycle state of the Pocket's container, as observed from Podman.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PocketState {
    /// No container exists, or it exists but is not running.
    Stopped,
    /// The owned container is running with a valid loopback mapping.
    Running,
}

/// Serializable Pocket status for the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PocketStatus {
    pub state: PocketState,
    /// URL of the running Pocket, built only from an inspected
    /// `127.0.0.1` port mapping. `None` when stopped.
    pub url: Option<String>,
    pub message: String,
}

/// A container verified to be managed by Pockets, keyed by the Podman ID
/// that later lifecycle operations (start, stop) must target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedContainer {
    /// Full container ID reported by Podman.
    pub(crate) id: String,
    /// Whether Podman reports the container as running.
    pub(crate) running: bool,
    /// Validated loopback URL when a safe mapping is reported; `None`
    /// for a stopped container (its mapping may be absent) or when the
    /// lookup did not require a mapping, as for `stop`.
    pub(crate) url: Option<String>,
    runtime_bindings: Option<String>,
}

/// Reports the target's state from Podman without modifying anything.
///
/// Returns `stopped` when no such container exists. Errors when a
/// container with the target's name exists but is not owned for this
/// target, or when the owned container runs without a single safe
/// loopback mapping of the target's port.
pub async fn inspect_target(bin: &Path, target: &Target) -> Result<PocketStatus, String> {
    let name = target.name();
    Ok(status_for(lookup_owned(bin, target, true).await?, &name))
}

/// Creates the target's container and starts it detached.
///
/// A running Pocket is left untouched and reported as-is; HTTP readiness
/// belongs to the command boundary, not here. A stopped owned container
/// is removed by its inspected ID (`podman rm`, no `-v`, so the data
/// volume is preserved) and recreated from the fixed secure target
/// arguments — a saved container configuration is never restarted.
///
/// Errors when the target cannot run on this host (the bundled manifest
/// is re-validated here; a managed target validated at construction),
/// when a foreign container or volume claims a managed name, or when the
/// image is missing (the bundled message then names the exact build
/// command).
pub(crate) async fn start_target(
    bin: &Path,
    target: &Target,
    values: &crate::runtime_env::Values,
    store: &crate::projects::Store,
    site: crate::local_sites::Site<'_>,
) -> Result<PocketStatus, String> {
    target.validate_start()?;
    let name = target.name();

    let existing = lookup_owned(bin, target, true).await?;
    if existing.as_ref().is_some_and(|c| c.running) {
        let status = status_for(existing, &name);
        site.running_address(store, status.url.as_deref())?;
        return Ok(status);
    }

    let port = site.address(store, None)?.port;
    ensure_data_volume(bin, target).await?;
    // Recreate from the fixed arguments; a saved container
    // configuration is never restarted. `rm` without `-v` keeps data.
    if let Some(container) = existing {
        runtime::run(bin, &["rm", &container.id]).await?;
    }

    let scope = target.runtime_scope()?;
    let bindings = if let Some(scope) = &scope {
        crate::runtime_env::clear(bin, scope).await?;
        crate::runtime_env::prepare(bin, scope, values).await?
    } else {
        Default::default()
    };
    if let Err(error) = run_pinned(bin, target, port, &bindings).await {
        if let Some(scope) = &scope {
            if matches!(lookup_owned(bin, target, false).await, Ok(None)) {
                if let Err(cleanup) = crate::runtime_env::clear(bin, scope).await {
                    return Err(format!("{error} {cleanup}"));
                }
            }
        }
        return Err(target.missing_image_hint(error));
    }

    // Reconcile from Podman rather than trusting the run invocation.
    inspect_pinned(bin, target, port).await
}

/// Stops the target's owned container with a bounded five-second grace
/// period.
///
/// Idempotent: a missing or already-stopped Pocket is reported without
/// any command. Only the inspected container ID is ever passed to
/// `podman stop` — never a name — and the container, its volume, and the
/// Podman machine are left intact, so data survives. The lookup still
/// enforces name and ownership, but not the safe-mapping rule: stop must
/// work even when a running container's port mapping is absent or
/// unsafe, because stop never uses the URL. The result is read back from
/// Podman rather than assumed.
pub async fn stop_target(bin: &Path, target: &Target) -> Result<PocketStatus, String> {
    let name = target.name();
    let Some(container) = lookup_owned(bin, target, false).await? else {
        return Ok(status_for(None, &name));
    };
    if !container.running {
        return Ok(status_for(Some(container), &name));
    }

    runtime::run(bin, &["stop", "--time", "5", &container.id]).await?;

    // Reconcile from Podman rather than assuming the stop succeeded.
    Ok(status_for(lookup_owned(bin, target, true).await?, &name))
}

/// Removes the target's owned container, whether it is running or not,
/// with `podman rm -f`.
///
/// Hard teardown for a project being deleted: unlike [`stop_target`], the
/// container definition itself goes away. The lookup is the same
/// name-and-ownership lookup stop uses, so a container that does not
/// carry this target's [`OWNERSHIP_LABEL`] is refused and never removed,
/// and only the inspected container ID is ever passed to `podman rm` —
/// never a name. Idempotent: no such container is success and no command
/// runs at all. [`Target::Bundled`] is refused before anything else, so
/// the bundled example is never removed here.
///
/// A container the engine reports and then loses before the removal is an
/// error, not a silent success: the caller retries, the lookup then finds
/// nothing, and the delete completes.
pub async fn remove_target(bin: &Path, target: &Target) -> Result<(), String> {
    reject_bundled_removal(target)?;
    // `require_mapping` is false exactly as for stop: removal never uses a
    // URL, and a running container with an absent or unsafe mapping must
    // still be removable.
    let Some(container) = lookup_owned(bin, target, false).await? else {
        return Ok(());
    };
    runtime::run(bin, &["rm", "-f", &container.id]).await?;
    Ok(())
}

/// Removes the target's dedicated data volume, destroying the project's
/// persisted data.
///
/// The volume is located by its deterministic managed name and must carry
/// this target's [`OWNERSHIP_LABEL`]: a foreign volume claiming that name
/// is refused rather than deleted, exactly as [`ensure_data_volume`]
/// refuses to adopt one. A volume that does not exist is success, so the
/// call is idempotent and safe to retry after a partial teardown.
/// [`Target::Bundled`] is refused before anything else, so the bundled
/// example's data volume is never removed here.
///
/// The name is the only handle Podman offers for a volume, and it is
/// native-owned by construction: [`Target::volume`] spells it from the
/// validated project id, so it can never carry an arbitrary argument.
pub async fn remove_volume(bin: &Path, target: &Target) -> Result<(), String> {
    reject_bundled_removal(target)?;
    // Absent is the desired end state; foreign is a refusal. Both are
    // decided before any mutation, so a refusal never deletes anything.
    if !volume_exists(bin, target, "remove").await? {
        return Ok(());
    }
    runtime::run(bin, &["volume", "rm", &target.volume()]).await?;
    Ok(())
}

/// Checks whether the target's dedicated Podman volume exists and carries this
/// target's ownership label. Refuses a foreign volume with the managed name.
pub async fn has_owned_volume(bin: &Path, target: &Target) -> Result<bool, String> {
    volume_exists(bin, target, "export").await
}

/// Maximum supported backup archive size shared symmetrically across export
/// and restore (4 GiB).
pub const MAX_BACKUP_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Timeout for Podman volume export and import transfers (10 minutes to
/// accommodate multi-GiB archives over local or VM-backed storage).
const BACKUP_TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Exports the target's Pockets-owned Podman volume as a tar archive written
/// atomically to `destination` with owner-only (`0600` on Unix) permissions.
///
/// Refuses a missing volume with an actionable error, refuses any foreign
/// volume claiming the managed name, and enforces [`MAX_BACKUP_ARCHIVE_BYTES`].
pub async fn export_volume(bin: &Path, target: &Target, destination: &Path) -> Result<(), String> {
    export_volume_with_limit(bin, target, destination, Some(MAX_BACKUP_ARCHIVE_BYTES)).await
}

async fn export_volume_with_limit(
    bin: &Path,
    target: &Target,
    destination: &Path,
    max_bytes: Option<u64>,
) -> Result<(), String> {
    if !volume_exists(bin, target, "export").await? {
        return Err(
            "This Pocket has no saved data volume yet. Start it at least once before exporting a backup."
                .to_owned(),
        );
    }
    runtime::run_to_file(
        bin,
        &["volume", "export", &target.volume()],
        destination,
        BACKUP_TRANSFER_TIMEOUT,
        max_bytes,
    )
    .await
}

/// Resets one managed project's saved data by stopping and removing its owned
/// container (releasing Podman's volume attachment) and removing its owned data
/// volume. The built image and project record stay intact; the next Start
/// creates a fresh empty volume.
pub async fn reset_volume(bin: &Path, target: &Target) -> Result<(), String> {
    reject_bundled_removal(target)?;
    // Verify volume ownership before touching the container so a foreign volume
    // claiming the managed name never causes a partial teardown.
    let _ = volume_exists(bin, target, "reset").await?;
    remove_target(bin, target).await?;
    remove_volume(bin, target).await?;
    Ok(())
}

/// Validates that `source` is a readable, non-empty tar archive before stopping
/// a Pocket or modifying its Podman volume. Accepts standard POSIX/USTAR/GNU
/// tar archives (verified by header checksum) as well as empty 1024-byte
/// zero-block tar archives produced by `podman volume export` on empty volumes.
fn validate_tar_backup(source: &Path) -> Result<(), String> {
    use std::io::Read;

    let meta = std::fs::metadata(source)
        .map_err(|err| format!("Could not read the selected backup file: {err}"))?;
    if !meta.is_file() {
        return Err("The selected backup path is not a regular file.".to_owned());
    }
    if meta.len() < 512 {
        return Err("The selected file is not a valid Pocket backup (.tar archive).".to_owned());
    }
    if meta.len() > MAX_BACKUP_ARCHIVE_BYTES {
        return Err(format!(
            "The selected backup archive exceeds the maximum supported size of {MAX_BACKUP_ARCHIVE_BYTES} bytes."
        ));
    }
    let mut file = std::fs::File::open(source)
        .map_err(|err| format!("Could not open the selected backup file: {err}"))?;
    let mut header = [0u8; 512];
    file.read_exact(&mut header)
        .map_err(|err| format!("Could not read the selected backup file: {err}"))?;

    // Empty tar archive: all-zero 512-byte header blocks.
    if header.iter().all(|byte| *byte == 0) {
        return Ok(());
    }

    // Standard tar header checksum (bytes 148..156, octal, computed with that
    // field treated as ASCII spaces).
    let raw_checksum = std::str::from_utf8(&header[148..156])
        .ok()
        .map(|text| text.trim_matches(|ch: char| ch == '\0' || ch == ' '))
        .filter(|text| !text.is_empty())
        .and_then(|text| u32::from_str_radix(text, 8).ok());
    let Some(expected) = raw_checksum else {
        return Err("The selected file is not a valid Pocket backup (.tar archive).".to_owned());
    };
    let unsigned_sum: u32 = header
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if (148..156).contains(&index) {
                b' ' as u32
            } else {
                byte as u32
            }
        })
        .sum();
    if unsigned_sum != expected {
        return Err("The selected file is not a valid Pocket backup (.tar archive).".to_owned());
    }
    Ok(())
}

/// Restores one managed project's Podman data volume from a `.tar` backup
/// archive. Stops and removes the owned container first so writes are quiesced
/// before taking an owner-only rollback snapshot, replaces the volume cleanly
/// (avoiding leftover stale files), and rolls back to the snapshot if `podman
/// volume import` fails. If rollback itself fails, the rollback snapshot is
/// preserved on disk and its path is included in the returned error.
pub async fn restore_volume(bin: &Path, target: &Target, source: &Path) -> Result<(), String> {
    reject_bundled_removal(target)?;
    validate_tar_backup(source)?;
    let had_volume = volume_exists(bin, target, "restore").await?;

    // Quiesce container writes before exporting the pre-restore snapshot.
    remove_target(bin, target).await?;

    let rollback_path = if had_volume {
        let temp = std::env::temp_dir().join(format!(
            "pockets-restore-rollback-{}-{}.tar",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        runtime::run_to_file(
            bin,
            &["volume", "export", &target.volume()],
            &temp,
            BACKUP_TRANSFER_TIMEOUT,
            None,
        )
        .await
        .map_err(|err| format!("Could not snapshot existing data before restoring: {err}"))?;
        Some(temp)
    } else {
        None
    };

    let restore_result = async {
        remove_volume(bin, target).await?;
        ensure_data_volume(bin, target).await?;
        runtime::run_from_file(
            bin,
            &["volume", "import", &target.volume(), "-"],
            source,
            BACKUP_TRANSFER_TIMEOUT,
        )
        .await
    }
    .await;

    if let Err(err) = restore_result {
        if let Some(backup) = rollback_path.as_deref() {
            let rollback_result: Result<(), String> = async {
                remove_volume(bin, target).await?;
                ensure_data_volume(bin, target).await?;
                runtime::run_from_file(
                    bin,
                    &["volume", "import", &target.volume(), "-"],
                    backup,
                    BACKUP_TRANSFER_TIMEOUT,
                )
                .await
            }
            .await;

            match rollback_result {
                Ok(()) => {
                    let _ = tokio::fs::remove_file(backup).await;
                    return Err(err);
                }
                Err(rollback_err) => {
                    return Err(format!(
                        "Backup restore failed ({err}), and automatic rollback also failed ({rollback_err}). \
                         Your previous data snapshot has been preserved for manual recovery at: {}",
                        backup.display()
                    ));
                }
            }
        }
        return Err(err);
    }

    if let Some(backup) = rollback_path.as_deref() {
        let _ = tokio::fs::remove_file(backup).await;
    }
    Ok(())
}

/// What one same-pocket replacement left behind, for the higher layer
/// that owns the health wait and the record commit: the reconciled
/// status to report, and everything a rollback needs to put the exact
/// previous container back — whether it had been running at all, and on
/// which engine-validated loopback port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplacementReceipt {
    /// Status reconciled from Podman after the replacement. Running with
    /// the original loopback URL when a running pocket was replaced;
    /// Stopped when the pocket was stopped or absent and stays that way.
    pub status: PocketStatus,
    /// Whether the previous container was running when the replacement
    /// began. A restore only ever recreates a container the user had
    /// actually left running.
    pub was_running: bool,
    /// The engine-reported `127.0.0.1` host port of the replaced
    /// container, derived only from its own validated inspect mapping —
    /// never from the frontend. `None` exactly when `was_running` is
    /// false.
    pub host_port: Option<u16>,
    bindings: crate::runtime_env::Bindings,
}

/// Swaps one managed project's container for the same project's next
/// pinned image without touching its name, data volume, or host port.
///
/// Both targets must be [`Target::Managed`] values for the same project
/// — the same project id, so the same container name, ownership label,
/// and dedicated data volume — with the same storage mount path; a
/// bundled target, a cross-project pair, or a disagreeing mount path is
/// refused before any engine command runs. This function never builds
/// or pulls an image, never waits on health, and never writes a record:
/// it returns the reconciled status and a [`ReplacementReceipt`] so the
/// caller owns the health wait and the commit, and can hand the receipt
/// to [`restore_replacement`] when either fails.
///
/// Before any destructive work: the previous container goes through the
/// same name-and-ownership lookup, requiring the safe mapping when it
/// runs so the host port is only ever the engine's own validated
/// loopback one; the data volume must be Pockets-owned; and the
/// candidate image must already exist in the local store — nothing is
/// ever pulled, and a running pocket is never torn down for an image
/// that is not there.
///
/// When the previous pocket ran it is stopped gracefully, its container
/// definition is removed (never `-v`, so the data volume is preserved),
/// and the candidate starts from the same secure fixed arguments with
/// only the publish pinned to the same host port and the same volume. If
/// the candidate fails to start or does not reconcile as running, the
/// previous container is recreated on the same port and data before the
/// original error returns, with any rollback failure reported verbatim
/// beside it — a failed replacement never leaves a false healthy claim.
///
/// When the previous pocket was stopped or absent, only a stale owned
/// container definition is removed and the result is `Stopped` without
/// launching anything: an update never starts an app the user left
/// stopped. The data volume is never removed by any path here.
pub(crate) async fn replace_target(
    bin: &Path,
    previous: &Target,
    next: &Target,
) -> Result<ReplacementReceipt, String> {
    validate_replacement_pair(previous, next)?;
    let name = next.name();

    // Identity first, and the previous container's own engine-validated
    // loopback port: the mapping-required lookup refuses foreign
    // containers and unsafe mappings before anything mutates, and the
    // port below is derived only from that validated URL.
    let existing = lookup_owned(bin, previous, true).await?;
    let was_running = existing.as_ref().is_some_and(|container| container.running);

    if !was_running {
        // Stopped or absent: retire a stale owned definition so the
        // next explicit start creates from the new image, but never
        // launch it here. `rm` without `-v` keeps the data volume.
        if let Some(container) = existing {
            runtime::run(bin, &["rm", &container.id]).await?;
            if container.runtime_bindings.is_some() {
                if let Some(scope) = previous.runtime_scope()? {
                    crate::runtime_env::clear(bin, &scope).await?;
                }
            }
        }
        return Ok(ReplacementReceipt {
            status: status_for(None, &name),
            was_running: false,
            host_port: None,
            bindings: Default::default(),
        });
    }

    // Running: the mapping-required lookup guarantees the validated URL,
    // and its port is the only one the replacement may pin.
    let url = existing
        .as_ref()
        .and_then(|container| container.url.as_deref())
        .ok_or_else(|| format!("container {name} is running without a validated loopback URL"))?;
    let host_port = loopback_url_port(url).ok_or_else(|| {
        format!("container {name} reports the loopback URL {url:?} with no usable host port")
    })?;
    let scope = previous
        .runtime_scope()?
        .ok_or("A replacement requires a managed Pocket.")?;
    let bindings = crate::runtime_env::Bindings::from_label(
        &scope,
        existing
            .as_ref()
            .and_then(|container| container.runtime_bindings.as_deref()),
    )?;
    bindings.verify(bin, &scope).await?;

    // The candidate must already be in the local store, and the data
    // volume must be ours, before anything destructive happens.
    ensure_candidate_image(bin, next).await?;
    ensure_data_volume(bin, next).await?;

    // Graceful stop, then retire the old definition; the volume stays.
    stop_target(bin, previous).await?;
    remove_target(bin, previous).await?;

    // Start the candidate on the same port and volume, then reconcile
    // from Podman rather than trusting the run invocation.
    match start_candidate(bin, next, host_port, &bindings).await {
        Ok(status) => Ok(ReplacementReceipt {
            status,
            was_running: true,
            host_port: Some(host_port),
            bindings,
        }),
        Err(err) => {
            // The pocket was live before the candidate: it must not be
            // left down because the candidate failed. Restore first,
            // then report the original error beside the outcome.
            match restore_owned(bin, next, previous, Some(host_port), &bindings).await {
                Ok(status) if status.state == PocketState::Running => Err(format!(
                    "{err} The previous Pocket was restored and is running on port {host_port}."
                )),
                Ok(status) => Err(format!(
                    "{err} The previous Pocket was recreated on port {host_port} but is not \
                     running: {}",
                    status.message
                )),
                Err(restore_err) => Err(format!(
                    "{err} The previous Pocket could NOT be restored either: {restore_err}"
                )),
            }
        }
    }
}

/// Rolls a same-pocket replacement back to its previous container, for
/// the higher layer that found the new container unhealthy or could not
/// commit its record after [`replace_target`] succeeded.
///
/// Refuses the same way [`replace_target`] does before touching the
/// engine — both targets must be the same managed project with the same
/// mount path — and a receipt claiming the pocket was running without a
/// nonzero loopback port is malformed and refused rather than restored
/// somewhere else. It then removes only the current owned container
/// definition (`rm -f`, never `-v`, so data survives) and recreates the
/// previous container only when the receipt says it had been running,
/// pinned to the receipt's exact port and the same data volume. When a
/// restore fails its error is returned as-is: a pocket the higher layer
/// just rejected is never started automatically in its place.
pub(crate) async fn restore_replacement(
    bin: &Path,
    current: &Target,
    previous: &Target,
    receipt: &ReplacementReceipt,
) -> Result<PocketStatus, String> {
    validate_replacement_pair(current, previous)?;
    let host_port = if receipt.was_running {
        let port = receipt.host_port.filter(|port| *port != 0).ok_or_else(|| {
            "the replacement receipt says the Pocket was running but records no valid loopback \
             host port; refusing to restore it somewhere else"
                .to_owned()
        })?;
        Some(port)
    } else {
        None
    };
    restore_owned(bin, current, previous, host_port, &receipt.bindings).await
}

/// Both sides of a replacement must name the same managed project: the
/// same project id — which is what fixes the container name, ownership
/// label, and dedicated data volume — and the same storage mount path.
/// A [`Target::Bundled`] on either side, a pair spanning two projects,
/// or a mount path that moved is refused here, before any engine call,
/// so replacement can never aim its destructive commands at another
/// project's container or at the bundled example.
fn validate_replacement_pair(previous: &Target, next: &Target) -> Result<(), String> {
    let (Target::Managed(previous), Target::Managed(next)) = (previous, next) else {
        return Err(
            "only managed project Pockets can be replaced in place; the bundled example is \
             never a replacement target"
                .to_owned(),
        );
    };
    if previous.project_id != next.project_id {
        return Err(format!(
            "replacement targets belong to different projects ({} and {}); refusing",
            previous.project_id, next.project_id
        ));
    }
    if previous.storage_path != next.storage_path {
        return Err(format!(
            "replacement targets disagree on the storage mount path ({} and {}); refusing",
            previous.storage_path, next.storage_path
        ));
    }
    Ok(())
}

/// The candidate image must already exist in the local store before a
/// running pocket is touched. `image exists` is a lookup, never a pull:
/// a managed digest is built or imported by the project pipeline, and a
/// missing one is the caller's error to fix, not a reason to tear down
/// the live container or fetch anything from a registry.
async fn ensure_candidate_image(bin: &Path, target: &Target) -> Result<(), String> {
    let image = target.image();
    if runtime::run(bin, &["image", "exists", image]).await.is_ok() {
        return Ok(());
    }
    Err(format!(
        "the replacement image {image} is not present in the local container store; \
         build or import it before replacing the Pocket"
    ))
}

/// Verify that Podman actually published the requested mapping before
/// a new container's status can reach a health probe or browser.
async fn inspect_pinned(bin: &Path, target: &Target, port: u16) -> Result<PocketStatus, String> {
    let status = inspect_target(bin, target).await?;
    if status.state == PocketState::Running
        && status.url.as_deref().and_then(loopback_url_port) != Some(port)
    {
        return Err(format!(
            "Podman did not publish the Pocket on its assigned loopback port {port}."
        ));
    }
    Ok(status)
}

/// Starts `target` detached on the pinned loopback host port and
/// reconciles it to a running status, so the caller never has to trust
/// the run invocation. The error states what Podman reported — it is
/// never worded as a health claim.
async fn start_candidate(
    bin: &Path,
    target: &Target,
    host_port: u16,
    bindings: &crate::runtime_env::Bindings,
) -> Result<PocketStatus, String> {
    run_pinned(bin, target, host_port, bindings).await?;
    let status = inspect_pinned(bin, target, host_port).await?;
    if status.state != PocketState::Running {
        return Err(format!(
            "the replacement container is not running: {}",
            status.message
        ));
    }
    Ok(status)
}

/// The rollback core shared by [`replace_target`]'s failed-candidate
/// path and [`restore_replacement`]: remove the current owned container
/// definition if there is one (`rm -f`, never `-v`), then — only when
/// `host_port` is `Some`, meaning the previous pocket really ran —
/// recreate the previous container pinned to that port and its own
/// volume, reconciled from Podman. Never touches the volume, never
/// pulls, and never falls back to `current` when restoring fails.
async fn restore_owned(
    bin: &Path,
    current: &Target,
    previous: &Target,
    host_port: Option<u16>,
    bindings: &crate::runtime_env::Bindings,
) -> Result<PocketStatus, String> {
    let name = current.name();
    // Removal never uses a URL, so the mapping-optional lookup is enough.
    if let Some(container) = lookup_owned(bin, current, false).await? {
        runtime::run(bin, &["rm", "-f", &container.id]).await?;
    }
    let Some(port) = host_port else {
        return Ok(status_for(None, &name));
    };
    run_pinned(bin, previous, port, bindings).await?;
    inspect_pinned(bin, previous, port).await
}

/// Run on the installed port, retaining all fixed isolation arguments.
async fn run_pinned(
    bin: &Path,
    target: &Target,
    host_port: u16,
    bindings: &crate::runtime_env::Bindings,
) -> Result<(), String> {
    bindings.run(bin, target.run_args(host_port)).await.map_err(|error| {
        if error.contains("address already in use") || error.contains("port is already allocated") {
            format!("Local port {host_port} is in use. Free that port and start the Pocket again. {error}")
        } else {
            error
        }
    })
}

/// Refuses a removal aimed at the bundled example.
///
/// The bundled Pocket is the app's own fixed example rather than a user
/// project: it is created and stopped here, never deleted. Only a
/// [`Target::Managed`] target — whose name, volume, and label are derived
/// from a validated project id — may be torn down.
fn reject_bundled_removal(target: &Target) -> Result<(), String> {
    match target {
        Target::Bundled => Err(
            "the bundled example Pocket is never removed; only a managed project's own \
             container and data volume can be deleted"
                .to_owned(),
        ),
        Target::Managed(_) => Ok(()),
    }
}

/// Reports the bundled Pocket's state from Podman without modifying
/// anything. Wrapper over [`inspect_target`] with [`Target::bundled`];
/// behavior is identical to before target generalization.
pub async fn inspect(bin: &Path) -> Result<PocketStatus, String> {
    inspect_target(bin, &Target::bundled()).await
}

/// Creates the bundled example's container and starts it detached.
/// Wrapper over [`start_target`] with [`Target::bundled`].
pub async fn start(bin: &Path, store: &crate::projects::Store) -> Result<PocketStatus, String> {
    start_target(
        bin,
        &Target::bundled(),
        &Default::default(),
        store,
        crate::local_sites::Site::Bundled,
    )
    .await
}

/// Stops the bundled example's owned container. Wrapper over
/// [`stop_target`] with [`Target::bundled`].
pub async fn stop(bin: &Path) -> Result<PocketStatus, String> {
    stop_target(bin, &Target::bundled()).await
}

/// Looks up the bundled example's owned container, or `None` when it
/// does not exist, requiring a safe loopback mapping while running.
/// Used only by the native live Podman test; production paths go
/// through the general target lookups.
#[cfg(test)]
pub(crate) async fn inspect_owned(bin: &Path) -> Result<Option<OwnedContainer>, String> {
    lookup_owned(bin, &Target::bundled(), true).await
}

/// Maps an inspection result to the frontend status, naming the
/// target's container in every message.
fn status_for(container: Option<OwnedContainer>, name: &str) -> PocketStatus {
    match container {
        None => PocketStatus {
            state: PocketState::Stopped,
            url: None,
            message: format!("Pocket {name} has not been created yet."),
        },
        Some(container) if container.running => PocketStatus {
            state: PocketState::Running,
            // A mapping-requirement lookup guarantees a safe mapping
            // whenever the container is running.
            url: container.url,
            message: format!("Pocket {name} is running."),
        },
        Some(_) => PocketStatus {
            state: PocketState::Stopped,
            url: None,
            message: format!("Pocket {name} is stopped."),
        },
    }
}

/// The bundled data volume name, `pockets-<metadata.id>-<storage.name>`,
/// spelled from the manifest. The bundled [`Target`] carries the resolved
/// constant; the create-args test pins the two together so they cannot
/// drift from the embedded manifest.
#[cfg(test)]
fn data_volume_name(manifest: &Manifest) -> String {
    format!("pockets-{}-{}", manifest.metadata.id, manifest.storage.name)
}

/// Appends the exact development build command when the run failure is a
/// missing image, which `--pull=never` surfaces instead of a registry pull.
fn with_build_hint(err: String, image: &str) -> String {
    let lower = err.to_lowercase();
    let missing = ["image not known", "no such image", "unable to find image"]
        .iter()
        .any(|marker| lower.contains(marker));
    if missing {
        format!("{err} Build the development image first: podman build -t {image} examples/hello-pocket")
    } else {
        err
    }
}

/// Makes sure the target's dedicated data volume exists and is owned for
/// this target.
///
/// A foreign volume with the managed name is refused; the volume is
/// never removed or recreated, so stopping preserves its data.
async fn ensure_data_volume(bin: &Path, target: &Target) -> Result<(), String> {
    if volume_exists(bin, target, "use").await? {
        return Ok(());
    }
    let volume = target.volume();
    runtime::run(
        bin,
        &[
            "volume",
            "create",
            "--label",
            &target.ownership_label_arg(),
            &volume,
        ],
    )
    .await
    .map(|_| ())
}

/// Whether the target's dedicated data volume already exists, refusing a
/// volume that claims the managed name without this target's ownership
/// label.
///
/// `refusal` names the action being blocked — `use` when a start would
/// adopt the volume, `remove` when a deletion would destroy it — so the
/// message stays actionable for whichever command gave up. `Ok(false)`
/// means nothing named that volume exists: there is nothing to adopt and
/// nothing to destroy. Read-only apart from the two lookups; the volume is
/// never created, changed, or removed here.
async fn volume_exists(bin: &Path, target: &Target, refusal: &str) -> Result<bool, String> {
    let volume = target.volume();
    let name_filter = format!("name=^{volume}$");
    let output = runtime::run(
        bin,
        &["volume", "ls", "--filter", &name_filter, "--format", "json"],
    )
    .await?;
    let entries: Vec<VolumeListEntry> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse `podman volume ls` output: {err}"))?;

    if !entries.iter().any(|entry| entry.name == volume) {
        return Ok(false);
    }

    let output = runtime::run(bin, &["volume", "inspect", &volume]).await?;
    let entries: Vec<VolumeInspectEntry> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse `podman volume inspect` output: {err}"))?;
    let [entry] = entries.as_slice() else {
        return Err(format!(
            "`podman volume inspect` returned {} results for {volume}, expected exactly one",
            entries.len()
        ));
    };

    let ownership = target.ownership_value();
    let owned = entry
        .labels
        .as_ref()
        .and_then(|labels| labels.get(OWNERSHIP_LABEL))
        .is_some_and(|value| *value == ownership);
    if !owned {
        return Err(format!(
            "volume {volume} exists but is not managed by Pockets \
             (expected label {OWNERSHIP_LABEL}={ownership}); refusing to {refusal} it"
        ));
    }
    Ok(true)
}

/// Looks up the target's owned container, or `None` when it does not
/// exist.
///
/// Errors when a container with the target's name lacks this target's
/// ownership, or when the owned container is running without exactly one
/// `127.0.0.1` mapping of the target's port with a valid nonzero host
/// port (unless `require_mapping` is false). Read-only; the returned ID
/// is the identity subsequent lifecycle operations must act on.
async fn lookup_owned(
    bin: &Path,
    target: &Target,
    require_mapping: bool,
) -> Result<Option<OwnedContainer>, String> {
    let Some(id) = find_managed_id(bin, target).await? else {
        return Ok(None);
    };
    let output = runtime::run(bin, &["inspect", &id]).await?;
    Ok(Some(target.parse_inspect(&output.stdout, require_mapping)?))
}

/// Finds the container ID whose Podman name is exactly the target's.
///
/// The `name` filter accepts a regex, so the anchors make it an exact
/// match; the parsed names are still compared exactly as defense in
/// depth. `--all` includes created and exited containers so a stopped
/// Pocket is still recognized as ours.
async fn find_managed_id(bin: &Path, target: &Target) -> Result<Option<String>, String> {
    let name = target.name();
    let name_filter = format!("name=^{name}$");
    let output = runtime::run(
        bin,
        &["ps", "--all", "--filter", &name_filter, "--format", "json"],
    )
    .await?;
    let entries: Vec<PsEntry> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse `podman ps` output: {err}"))?;

    let Some(entry) = entries.iter().find(|entry| entry.has_name(&name)) else {
        return Ok(None);
    };
    // The ID comes from command output and becomes an argument later;
    // accept only the alphanumeric form Podman emits.
    if !is_safe_container_id(&entry.id) {
        return Err(format!(
            "`podman ps` reported an unusable container id for {name}"
        ));
    }
    Ok(Some(entry.id.clone()))
}

/// Strict `podman inspect` validation against the bundled target: a
/// running container must expose a safe loopback mapping. Test-only
/// entry point over [`Target::parse_inspect`]; production callers use
/// the mapping-optional lookup directly.
#[cfg(test)]
fn parse_inspect(stdout: &[u8]) -> Result<OwnedContainer, String> {
    Target::bundled().parse_inspect(stdout, true)
}

/// Bundled-target entry point over [`Target::parse_inspect`] with an
/// optional mapping requirement. Test-only; production callers go
/// through `lookup_owned`, which always validates against a target.
#[cfg(test)]
fn parse_inspect_with_mapping(
    stdout: &[u8],
    require_mapping: bool,
) -> Result<OwnedContainer, String> {
    Target::bundled().parse_inspect(stdout, require_mapping)
}

/// Host port of the sole safe mapping under `port_key`, if there is
/// one: exactly one binding, `HostIp` equal to [`LOOPBACK_IP`], and a
/// `HostPort` that parses as a nonzero `u16`. Anything else — no
/// mapping, a wildcard or empty `HostIp`, multiple bindings, or a
/// malformed port — yields `None`.
fn loopback_port(
    ports: Option<&HashMap<String, Option<Vec<PortBinding>>>>,
    port_key: &str,
) -> Option<u16> {
    let bindings = ports?.get(port_key)?.as_ref()?;
    let [binding] = bindings.as_slice() else {
        return None;
    };
    if binding.host_ip.as_deref()? != LOOPBACK_IP {
        return None;
    }
    let port: u16 = binding.host_port.as_deref()?.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some(port)
}

/// Host port of a loopback URL that [`Target::parse_inspect`] itself
/// produced: exactly `http://127.0.0.1:<port>` with a nonzero port.
/// Anything else is `None` — a host port is only ever derived from the
/// engine's own validated mapping, never from a URL that arrived from
/// anywhere else. Pure so fixtures can test it.
pub(crate) fn loopback_url_port(url: &str) -> Option<u16> {
    let digits = url.strip_prefix(&format!("http://{LOOPBACK_IP}:"))?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let port: u16 = digits.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some(port)
}

/// Podman container IDs are lowercase hex; rejecting anything else keeps
/// command output out of the argument position untouched.
pub(crate) fn is_safe_container_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// Exactly `len` lowercase hexadecimal ASCII characters.
fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One row of `podman ps --format json`. Podman marshals `Names` as an
/// array; Docker-compatible engines use a single string.
#[derive(Debug, Deserialize)]
struct PsEntry {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Names")]
    names: PsNames,
}

impl PsEntry {
    fn has_name(&self, name: &str) -> bool {
        let matches =
            |candidate: &str| candidate == name || candidate.strip_prefix('/') == Some(name);
        match &self.names {
            PsNames::One(candidate) => matches(candidate),
            PsNames::Many(candidates) => candidates.iter().any(|candidate| matches(candidate)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PsNames {
    One(String),
    Many(Vec<String>),
}

/// The inspected fields this module needs. All other inspect output is
/// ignored; pointers stay `Option` because Podman can emit nulls.
#[derive(Debug, Deserialize)]
struct InspectEntry {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "State")]
    state: Option<InspectState>,
    #[serde(rename = "Config")]
    config: Option<InspectConfig>,
    #[serde(rename = "NetworkSettings")]
    network_settings: Option<InspectNetworkSettings>,
}

#[derive(Debug, Deserialize)]
struct InspectState {
    #[serde(rename = "Running")]
    running: bool,
}

#[derive(Debug, Deserialize)]
struct InspectConfig {
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct InspectNetworkSettings {
    /// `null` per port key until a mapping exists.
    #[serde(rename = "Ports")]
    ports: Option<HashMap<String, Option<Vec<PortBinding>>>>,
}

/// One host port mapping. Podman's Docker-compatible JSON spells the IP
/// key `HostIp` and keeps the port as a string.
#[derive(Debug, Deserialize)]
struct PortBinding {
    #[serde(rename = "HostIp", default)]
    host_ip: Option<String>,
    #[serde(rename = "HostPort", default)]
    host_port: Option<String>,
}

/// One row of `podman volume ls --format json`.
#[derive(Debug, Deserialize)]
struct VolumeListEntry {
    #[serde(rename = "Name")]
    name: String,
}

/// One `podman volume inspect` result; both it and `volume ls` marshal
/// the same `InspectVolumeData` shape.
#[derive(Debug, Deserialize)]
struct VolumeInspectEntry {
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "6e1f2c3d4b5a69788796a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4";

    /// Builds a one-element `podman inspect` array with the managed name.
    /// `label` is the `dev.pockets.managed` value, or `None` for a
    /// container with no Pockets labels; `ports` is raw JSON.
    fn inspect_json(label: Option<&str>, running: bool, ports: &str) -> String {
        let config = match label {
            Some(value) => format!(r#""Config":{{"Labels":{{"{OWNERSHIP_LABEL}":"{value}"}}}}"#,),
            None => r#""Config":{}"#.to_owned(),
        };
        format!(
            r#"[{{"Id":"{ID}","Name":"{CONTAINER_NAME}","State":{{"Status":{},"Running":{running}}},{config},"NetworkSettings":{{"Ports":{ports}}}}}]"#,
            if running {
                r#""running""#
            } else {
                r#""exited""#
            }
        )
    }

    #[test]
    fn owned_container_with_loopback_mapping_is_accepted() {
        let running = inspect_json(
            Some(OWNERSHIP_VALUE),
            true,
            r#"{"8080/tcp":[{"HostIp":"127.0.0.1","HostPort":"44011"}]}"#,
        );
        let owned = parse_inspect(running.as_bytes()).expect("owned running container is accepted");
        assert_eq!(owned.id, ID);
        assert!(owned.running);
        assert_eq!(owned.url.as_deref(), Some("http://127.0.0.1:44011"));

        // A leading slash on the inspected name is still this container.
        let slashed = running.replace(
            &format!(r#""Name":"{CONTAINER_NAME}""#),
            &format!(r#""Name":"/{CONTAINER_NAME}""#),
        );
        assert!(parse_inspect(slashed.as_bytes()).is_ok());

        // Stopped: a null port map is fine and yields no URL.
        let stopped = inspect_json(Some(OWNERSHIP_VALUE), false, r#"{"8080/tcp":null}"#);
        let owned = parse_inspect(stopped.as_bytes()).expect("owned stopped container is accepted");
        assert!(!owned.running);
        assert_eq!(owned.url, None);
    }

    #[test]
    fn foreign_containers_and_unsafe_mappings_are_refused() {
        let mapping = |ip: &str, port: &str| {
            format!(r#"{{"8080/tcp":[{{"HostIp":"{ip}","HostPort":"{port}"}}]}}"#)
        };
        let cases = [
            // Foreign ownership: another tool (or user) owns the name.
            (
                "no Pockets label",
                inspect_json(None, true, r#"{"8080/tcp":null}"#),
            ),
            (
                "foreign label value",
                inspect_json(Some("someone-else"), true, r#"{"8080/tcp":null}"#),
            ),
            // Running but not safely published on loopback.
            (
                "wildcard host IP",
                inspect_json(Some(OWNERSHIP_VALUE), true, &mapping("0.0.0.0", "44011")),
            ),
            (
                "empty host IP",
                inspect_json(Some(OWNERSHIP_VALUE), true, &mapping("", "44011")),
            ),
            (
                "port zero",
                inspect_json(Some(OWNERSHIP_VALUE), true, &mapping("127.0.0.1", "0")),
            ),
            (
                "port not a number",
                inspect_json(
                    Some(OWNERSHIP_VALUE),
                    true,
                    &mapping("127.0.0.1", "backport"),
                ),
            ),
            (
                "port out of range",
                inspect_json(Some(OWNERSHIP_VALUE), true, &mapping("127.0.0.1", "70000")),
            ),
            (
                "no mapping while running",
                inspect_json(Some(OWNERSHIP_VALUE), true, "null"),
            ),
            // Two bindings is not "exactly one", even when one is loopback.
            (
                "IPv6 loopback alongside IPv4",
                inspect_json(
                    Some(OWNERSHIP_VALUE),
                    true,
                    r#"{"8080/tcp":[{"HostIp":"127.0.0.1","HostPort":"44011"},{"HostIp":"::1","HostPort":"44011"}]}"#,
                ),
            ),
        ];
        for (what, json) in cases {
            let err = parse_inspect(json.as_bytes()).expect_err(&format!("{what} must be refused"));
            assert!(
                err.contains("refusing"),
                "{what}: expected a refusal, got {err}"
            );
        }
    }

    #[test]
    fn create_args_are_secure_loopback_and_pull_free() {
        let manifest = manifest::bundled_manifest().expect("bundled manifest parses");
        let target = Target::bundled();
        let volume = target.volume();
        assert_eq!(volume, "pockets-hello-pocket-data");

        // The bundled target must keep mirroring the embedded manifest
        // exactly, so its fixed arguments stay the manifest's arguments.
        assert_eq!(target.port(), manifest.web.port);
        assert_eq!(target.storage_path(), manifest.storage.path);
        assert_eq!(target.image(), manifest.runtime.image);
        assert_eq!(volume, data_volume_name(&manifest));

        let args = target.run_args(18788);
        let value_after = |flag: &str| {
            args.iter()
                .position(|arg| arg == flag)
                .and_then(|index| args.get(index + 1))
                .map(String::as_str)
        };

        assert_eq!(args.first().map(String::as_str), Some("run"));
        assert!(args.iter().any(|arg| arg == "--detach"));
        assert_eq!(value_after("--name"), Some(CONTAINER_NAME));
        assert_eq!(
            value_after("--label"),
            Some("dev.pockets.managed=hello-pocket")
        );
        assert!(args.iter().any(|arg| arg == "--pull=never"));
        assert!(args.iter().any(|arg| arg == "--cap-drop=ALL"));
        assert!(args
            .iter()
            .any(|arg| arg == "--security-opt=no-new-privileges"));
        assert_eq!(value_after("--publish"), Some("127.0.0.1:18788:8080"));
        assert_eq!(
            value_after("--volume"),
            Some("pockets-hello-pocket-data:/data")
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("localhost/pockets-hello:0.1.0")
        );

        // The fixed surface must never grow a privilege or an escape.
        for forbidden in ["--privileged", "--network", "--net", "--device", "--mount"] {
            assert!(
                !args.iter().any(|arg| arg.starts_with(forbidden)),
                "unexpected {forbidden} in {args:?}"
            );
        }
        assert!(
            !args.iter().any(|arg| arg == "-v" || arg == "-p"),
            "shorthands must not appear: {args:?}"
        );
    }

    #[test]
    fn managed_target_args_carry_project_identity_and_never_bundled_defaults() {
        // A manifest that differs from the bundled example in every
        // field the target may draw from it, so any leftover bundled
        // constant or manifest tag is visible in the arguments.
        let base = manifest::bundled_manifest().expect("bundled manifest parses");
        let manifest = Manifest {
            metadata: manifest::Metadata {
                id: "other-pocket".to_owned(),
                ..base.metadata.clone()
            },
            runtime: manifest::Runtime {
                image: "registry.example/pocket-app:latest".to_owned(),
                architectures: base.runtime.architectures.clone(),
            },
            web: manifest::Web {
                port: 9000,
                healthcheck: "/healthz".to_owned(),
            },
            storage: manifest::Storage {
                name: "state".to_owned(),
                path: "/var/lib/app".to_owned(),
            },
            ..base
        };
        manifest.validate().expect("mutated manifest is valid");

        let project_id = "0123456789abcdef0123456789abcdef";
        let image_id = format!("sha256:{}", "ab".repeat(32));
        let target =
            Target::managed(&manifest, project_id, &image_id).expect("managed target is built");

        let args = target.run_args(18788);
        let value_after = |flag: &str| {
            args.iter()
                .position(|arg| arg == flag)
                .and_then(|index| args.get(index + 1))
                .map(String::as_str)
        };

        // Identity comes from the project ID, port and mount path from
        // this manifest, and the image from the immutable digest.
        assert_eq!(
            value_after("--name"),
            Some("pockets-project-0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            value_after("--label"),
            Some("dev.pockets.managed=project-0123456789abcdef0123456789abcdef")
        );
        assert_eq!(value_after("--publish"), Some("127.0.0.1:18788:9000"));
        assert_eq!(
            value_after("--volume"),
            Some("pockets-project-0123456789abcdef0123456789abcdef-data:/var/lib/app")
        );
        assert_eq!(args.last().map(String::as_str), Some(image_id.as_str()));

        // The safety surface is the same fixed surface as bundled.
        assert!(args.iter().any(|arg| arg == "--pull=never"));
        assert!(args.iter().any(|arg| arg == "--cap-drop=ALL"));
        assert!(args
            .iter()
            .any(|arg| arg == "--security-opt=no-new-privileges"));
        for forbidden in ["--privileged", "--network", "--net", "--device", "--mount"] {
            assert!(
                !args.iter().any(|arg| arg.starts_with(forbidden)),
                "unexpected {forbidden} in {args:?}"
            );
        }

        // Nothing bundled may leak into a managed project's arguments:
        // no hello identity, no bundled image or tag, no bundled port,
        // and no manifest tag the digest was supposed to override.
        let joined = args.join(" ");
        for leak in [
            "hello",
            "pockets-hello",
            "8080",
            "/data",
            "0.1.0",
            "registry.example",
        ] {
            assert!(!joined.contains(leak), "unexpected {leak:?} in {joined}");
        }
    }

    #[test]
    fn mapping_optional_lookup_validates_identity_but_still_refuses_foreign() {
        // Owned and running behind a wildcard mapping: still a validated
        // identity for stop, whose lookup skips the mapping requirement.
        let wildcard = inspect_json(
            Some(OWNERSHIP_VALUE),
            true,
            r#"{"8080/tcp":[{"HostIp":"0.0.0.0","HostPort":"44011"}]}"#,
        );
        let owned = parse_inspect_with_mapping(wildcard.as_bytes(), false)
            .expect("stop lookup tolerates an unsafe mapping");
        assert!(owned.running);
        assert_eq!(owned.url, None);

        // No mapping at all while running is likewise tolerated here.
        let unmapped = inspect_json(Some(OWNERSHIP_VALUE), true, "null");
        let owned = parse_inspect_with_mapping(unmapped.as_bytes(), false)
            .expect("stop lookup tolerates a missing mapping");
        assert_eq!(owned.id, ID);

        // Foreign ownership is refused even without the mapping rule.
        let foreign = inspect_json(None, true, "null");
        let err = parse_inspect_with_mapping(foreign.as_bytes(), false)
            .expect_err("foreign container is still refused");
        assert!(err.contains("refusing"), "got {err}");
    }

    /// Project id shared by the managed removal fixtures, together with the
    /// container name, data volume, and ownership value that id fixes. The
    /// managed-arguments test above pins this exact spelling against
    /// [`Target::managed`], so these constants cannot drift from it.
    const MANAGED_ID: &str = "0123456789abcdef0123456789abcdef";
    const MANAGED_CONTAINER: &str = "pockets-project-0123456789abcdef0123456789abcdef";
    const MANAGED_VOLUME: &str = "pockets-project-0123456789abcdef0123456789abcdef-data";
    const MANAGED_OWNERSHIP: &str = "project-0123456789abcdef0123456789abcdef";

    /// The managed target the removal fixtures drive. The bundled manifest is
    /// a valid manifest, so only the project id and an immutable digest
    /// distinguish this target from any other.
    fn managed_target() -> Target {
        let manifest = manifest::bundled_manifest().expect("bundled manifest parses");
        Target::managed(
            &manifest,
            MANAGED_ID,
            &format!("sha256:{}", "ab".repeat(32)),
        )
        .expect("managed target is built")
    }

    /// One `podman ps --format json` row naming the managed container.
    fn managed_ps_row() -> String {
        format!(r#"[{{"Id":"{ID}","Names":["{MANAGED_CONTAINER}"]}}]"#)
    }

    /// One `podman inspect` element for the managed container. `label` is the
    /// `dev.pockets.managed` value, or `None` for a container with no Pockets
    /// labels. Removal never uses a URL, so the port map is null.
    fn managed_inspect_json(label: Option<&str>) -> String {
        let config = match label {
            Some(value) => format!(r#""Config":{{"Labels":{{"{OWNERSHIP_LABEL}":"{value}"}}}}"#),
            None => r#""Config":{}"#.to_owned(),
        };
        format!(
            r#"[{{"Id":"{ID}","Name":"{MANAGED_CONTAINER}","State":{{"Status":"exited","Running":false}},{config},"NetworkSettings":{{"Ports":{{"8080/tcp":null}}}}}}]"#
        )
    }

    /// One `podman volume ls --format json` row naming the managed volume.
    fn managed_volume_ls_row() -> String {
        format!(r#"[{{"Name":"{MANAGED_VOLUME}"}}]"#)
    }

    /// One `podman volume inspect` result for the managed volume: ours when
    /// `owned`, otherwise carrying another tool's label under the same name.
    fn managed_volume_inspect_json(owned: bool) -> String {
        let labels = if owned {
            format!(r#"{{"{OWNERSHIP_LABEL}":"{MANAGED_OWNERSHIP}"}}"#)
        } else {
            r#"{"Owner":"someone-else"}"#.to_owned()
        };
        format!(r#"[{{"Labels":{labels}}}]"#)
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

    /// The fixed lookup-then-remove shape of a container teardown: one
    /// anchored `ps` lookup, one `inspect` of what it found, and a forced
    /// `rm` aimed at the inspected ID rather than the container's name.
    #[tokio::test]
    async fn remove_target_deletes_the_owned_container_by_its_inspected_id() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let script = [
            format!(
                r#"if [ "$1" = "ps" ]; then printf '{}'; fi"#,
                managed_ps_row()
            ),
            format!(
                r#"if [ "$1" = "inspect" ]; then printf '{}'; fi"#,
                managed_inspect_json(Some(MANAGED_OWNERSHIP))
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &script);

        remove_target(&bin, &managed_target())
            .await
            .expect("the owned container is removed");

        assert_eq!(
            invocations(&record),
            vec![
                format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json"),
                format!("inspect {ID}"),
                format!("rm -f {ID}"),
            ],
            "-f must cover a running container and the ID is the only identity used"
        );
    }

    /// A foreign container that merely carries the managed name is refused by
    /// the same lookup stop uses, and the refusal happens before `rm` exists
    /// in the argument list at all.
    #[tokio::test]
    async fn remove_target_refuses_a_container_without_the_ownership_label() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let script = [
            format!(
                r#"if [ "$1" = "ps" ]; then printf '{}'; fi"#,
                managed_ps_row()
            ),
            format!(
                r#"if [ "$1" = "inspect" ]; then printf '{}'; fi"#,
                managed_inspect_json(None)
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &script);

        let err = remove_target(&bin, &managed_target())
            .await
            .expect_err("an unlabelled container under the managed name must be refused");
        assert!(err.contains("refusing"), "expected a refusal, got {err}");
        assert!(
            err.contains(MANAGED_CONTAINER),
            "the refusal must name the container, got {err}"
        );

        let calls = invocations(&record);
        assert_eq!(calls.len(), 2, "lookup only: {calls:?}");
        assert!(
            !calls.iter().any(|line| line.starts_with("rm")),
            "a refused container must never be removed: {calls:?}"
        );
    }

    /// The data volume is addressed by its deterministic managed name — the
    /// only handle Podman gives for a volume — but only after the ownership
    /// label proves it is ours.
    #[tokio::test]
    async fn remove_volume_deletes_the_owned_volume_by_its_managed_name() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let script = [
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{}'; fi"#,
                managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(true)
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &script);

        remove_volume(&bin, &managed_target())
            .await
            .expect("the owned volume is removed");

        assert_eq!(
            invocations(&record),
            vec![
                format!("volume ls --filter name=^{MANAGED_VOLUME}$ --format json"),
                format!("volume inspect {MANAGED_VOLUME}"),
                format!("volume rm {MANAGED_VOLUME}"),
            ],
            "the volume is proved ours before anything is destroyed"
        );
    }

    #[tokio::test]
    async fn remove_volume_refuses_a_foreign_volume_and_destroys_nothing() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let script = [
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{}'; fi"#,
                managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(false)
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &script);

        let err = remove_volume(&bin, &managed_target())
            .await
            .expect_err("a foreign volume claiming the managed name must be refused");
        assert!(err.contains("refusing to remove"), "got {err}");
        assert!(
            err.contains(MANAGED_VOLUME),
            "the refusal must name the volume, got {err}"
        );

        let calls = invocations(&record);
        assert!(
            !calls.iter().any(|line| line.starts_with("volume rm")),
            "a refused volume must never be removed: {calls:?}"
        );
    }

    #[tokio::test]
    async fn export_volume_streams_owned_volume_to_file_and_refuses_foreign_or_missing() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let dest = dir.path().join("backup.tar");

        // Owned volume exports cleanly to destination.
        let owned_script = [
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{}'; fi"#,
                managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(true)
            ),
            r#"if [ "$1" = "volume" ] && [ "$2" = "export" ]; then printf 'tar-archive-bytes'; fi"#
                .to_owned(),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &owned_script);
        export_volume(&bin, &managed_target(), &dest)
            .await
            .expect("owned volume should export");
        assert_eq!(
            std::fs::read(&dest).expect("exported file should exist"),
            b"tar-archive-bytes"
        );
        assert_eq!(
            invocations(&record),
            vec![
                format!("volume ls --filter name=^{MANAGED_VOLUME}$ --format json"),
                format!("volume inspect {MANAGED_VOLUME}"),
                format!("volume export {MANAGED_VOLUME}"),
            ]
        );

        // Missing volume returns an actionable error and writes nothing.
        let missing_dir = tempfile::TempDir::new().expect("temp fixture dir");
        let missing_dest = missing_dir.path().join("missing.tar");
        let (bin, _) = fake_podman(
            missing_dir.path(),
            r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '[]'; fi"#,
        );
        let err = export_volume(&bin, &managed_target(), &missing_dest)
            .await
            .expect_err("missing volume must fail");
        assert!(err.contains("no saved data volume"), "got {err}");
        assert!(!missing_dest.exists());

        // Foreign volume is refused before export runs.
        let foreign_dir = tempfile::TempDir::new().expect("temp fixture dir");
        let foreign_dest = foreign_dir.path().join("foreign.tar");
        let foreign_script = [
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{}'; fi"#,
                managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(false)
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(foreign_dir.path(), &foreign_script);
        let err = export_volume(&bin, &managed_target(), &foreign_dest)
            .await
            .expect_err("foreign volume must be refused");
        assert!(err.contains("refusing to export"), "got {err}");
        assert!(!foreign_dest.exists());
        assert!(!invocations(&record)
            .iter()
            .any(|line| line.starts_with("volume export")));
    }

    #[tokio::test]
    async fn reset_volume_removes_owned_container_and_volume_and_refuses_foreign_volume() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let script = [
            format!(
                r#"if [ "$1" = "ps" ]; then printf '{}'; fi"#,
                managed_ps_row()
            ),
            format!(
                r#"if [ "$1" = "inspect" ]; then printf '{}'; fi"#,
                managed_inspect_json(Some(MANAGED_OWNERSHIP))
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{}'; fi"#,
                managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(true)
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &script);
        reset_volume(&bin, &managed_target())
            .await
            .expect("owned container and volume reset cleanly");
        let calls = invocations(&record);
        let rm_container = calls
            .iter()
            .position(|line| line == &format!("rm -f {ID}"))
            .expect("container removed");
        let rm_volume = calls
            .iter()
            .position(|line| line == &format!("volume rm {MANAGED_VOLUME}"))
            .expect("volume removed");
        assert!(
            rm_container < rm_volume,
            "container must be removed before its volume: {calls:?}"
        );

        // Foreign volume must refuse before touching the container.
        let foreign_dir = tempfile::TempDir::new().expect("temp fixture dir");
        let foreign_script = [
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{}'; fi"#,
                managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(false)
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(foreign_dir.path(), &foreign_script);
        let err = reset_volume(&bin, &managed_target())
            .await
            .expect_err("foreign volume must refuse reset");
        assert!(err.contains("refusing to reset"), "got {err}");
        assert!(!invocations(&record)
            .iter()
            .any(|line| line.starts_with("rm") || line.starts_with("volume rm")));
    }

    /// Builds a minimal valid 1024-byte POSIX tar archive with one file entry
    /// and a valid header checksum.
    fn minimal_valid_tar() -> Vec<u8> {
        let mut tar = vec![0u8; 1536];
        tar[0..8].copy_from_slice(b"note.txt");
        tar[100..107].copy_from_slice(b"0000644");
        tar[108..115].copy_from_slice(b"0000000");
        tar[116..123].copy_from_slice(b"0000000");
        tar[124..135].copy_from_slice(b"00000000005");
        tar[136..147].copy_from_slice(b"00000000000");
        tar[156] = b'0';
        tar[257..263].copy_from_slice(b"ustar\0");
        tar[263..265].copy_from_slice(b"00");
        let sum: u32 = tar[0..512]
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    b' ' as u32
                } else {
                    b as u32
                }
            })
            .sum();
        let checksum = format!("{sum:06o}\0 ");
        tar[148..156].copy_from_slice(checksum.as_bytes());
        tar[512..517].copy_from_slice(b"hello");
        tar
    }

    #[tokio::test]
    async fn restore_volume_validates_tar_replaces_volume_and_rolls_back_on_import_failure() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");

        // 1. Non-tar file is rejected before touching Podman at all.
        let bad_file = dir.path().join("not-a-tar.txt");
        std::fs::write(&bad_file, b"plain text, definitely not a tar archive")
            .expect("write bad file");
        let (bin, record) = fake_podman(dir.path(), "");
        let err = restore_volume(&bin, &managed_target(), &bad_file)
            .await
            .expect_err("non-tar file must be rejected");
        assert!(err.contains("not a valid Pocket backup"), "got {err}");
        assert!(!record.exists(), "invalid backup must never invoke podman");

        // 2. Valid tar restores cleanly: snapshots old volume, removes
        //    container + old volume, recreates volume, imports tar via stdin.
        let ok_dir = tempfile::TempDir::new().expect("temp fixture dir");
        let valid_tar = ok_dir.path().join("backup.tar");
        let tar_bytes = minimal_valid_tar();
        std::fs::write(&valid_tar, &tar_bytes).expect("write tar");
        let imported_capture = ok_dir.path().join("imported.bin");
        let ok_script = [
            format!(
                r#"if [ "$1" = "ps" ]; then printf '{}'; fi"#,
                managed_ps_row()
            ),
            format!(
                r#"if [ "$1" = "inspect" ]; then printf '{}'; fi"#,
                managed_inspect_json(Some(MANAGED_OWNERSHIP))
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then
                    if [ -f "{removed}" ]; then printf '[]'; else printf '{ls}'; fi
                fi"#,
                removed = ok_dir.path().join("removed").display(),
                ls = managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(true)
            ),
            r#"if [ "$1" = "volume" ] && [ "$2" = "export" ]; then printf 'old-snapshot'; fi"#
                .to_owned(),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "rm" ]; then touch "{}"; fi"#,
                ok_dir.path().join("removed").display()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "create" ]; then rm -f "{}"; fi"#,
                ok_dir.path().join("removed").display()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "import" ]; then cat > "{}"; fi"#,
                imported_capture.display()
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(ok_dir.path(), &ok_script);
        restore_volume(&bin, &managed_target(), &valid_tar)
            .await
            .expect("valid tar should restore");
        assert_eq!(
            std::fs::read(&imported_capture).expect("captured stdin"),
            tar_bytes
        );
        let calls = invocations(&record);
        let rm_idx = calls
            .iter()
            .position(|line| line == &format!("rm -f {ID}"))
            .expect("container removed");
        let export_idx = calls
            .iter()
            .position(|line| line == &format!("volume export {MANAGED_VOLUME}"))
            .expect("rollback snapshot exported");
        assert!(
            rm_idx < export_idx,
            "container must be stopped/removed before exporting rollback snapshot: {calls:?}"
        );
        assert!(calls.contains(&format!("volume rm {MANAGED_VOLUME}")));
        assert!(calls.contains(&format!(
            "volume create --label {OWNERSHIP_LABEL}={MANAGED_OWNERSHIP} {MANAGED_VOLUME}"
        )));
        assert!(calls.contains(&format!("volume import {MANAGED_VOLUME} -")));

        // 3. When the primary import fails, rollback automatically re-imports
        //    the pre-restore snapshot.
        let fail_dir = tempfile::TempDir::new().expect("temp fixture dir");
        let fail_tar = fail_dir.path().join("backup.tar");
        std::fs::write(&fail_tar, &tar_bytes).expect("write tar");
        let rollback_capture = fail_dir.path().join("rolled-back.bin");
        let attempt_flag = fail_dir.path().join("attempted");
        let fail_script = [
            r#"if [ "$1" = "ps" ]; then printf '[]'; fi"#.to_owned(),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then
                    if [ -f "{removed}" ]; then printf '[]'; else printf '{ls}'; fi
                fi"#,
                removed = fail_dir.path().join("removed").display(),
                ls = managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(true)
            ),
            r#"if [ "$1" = "volume" ] && [ "$2" = "export" ]; then printf 'previous-volume-bytes'; fi"#
                .to_owned(),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "rm" ]; then touch "{}"; fi"#,
                fail_dir.path().join("removed").display()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "create" ]; then rm -f "{}"; fi"#,
                fail_dir.path().join("removed").display()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "import" ]; then
                    if [ ! -f "{attempt}" ]; then
                        touch "{attempt}"
                        echo "corrupt tar payload" >&2
                        exit 1
                    fi
                    cat > "{rollback}"
                fi"#,
                attempt = attempt_flag.display(),
                rollback = rollback_capture.display()
            ),
        ]
        .join("\n");
        let (bin, _) = fake_podman(fail_dir.path(), &fail_script);
        let err = restore_volume(&bin, &managed_target(), &fail_tar)
            .await
            .expect_err("failed import must return error");
        assert!(err.contains("corrupt tar payload"), "got {err}");
        assert_eq!(
            std::fs::read(&rollback_capture).expect("rollback should have re-imported"),
            b"previous-volume-bytes"
        );
    }

    #[tokio::test]
    async fn restore_volume_preserves_owner_only_rollback_archive_when_rollback_also_fails() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let tar_path = dir.path().join("backup.tar");
        std::fs::write(&tar_path, minimal_valid_tar()).expect("write tar");
        let attempt_flag = dir.path().join("attempted");
        let removed_flag = dir.path().join("removed");

        let script = [
            format!(
                r#"if [ "$1" = "ps" ]; then printf '{}'; fi"#,
                managed_ps_row()
            ),
            format!(
                r#"if [ "$1" = "inspect" ]; then printf '{}'; fi"#,
                managed_inspect_json(Some(MANAGED_OWNERSHIP))
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then
                    if [ -f "{removed}" ]; then printf '[]'; else printf '{ls}'; fi
                fi"#,
                removed = removed_flag.display(),
                ls = managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(true)
            ),
            r#"if [ "$1" = "volume" ] && [ "$2" = "export" ]; then printf 'precious-existing-data'; fi"#
                .to_owned(),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "rm" ]; then touch "{}"; fi"#,
                removed_flag.display()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "create" ]; then rm -f "{}"; fi"#,
                removed_flag.display()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "import" ]; then
                    if [ ! -f "{attempt}" ]; then
                        touch "{attempt}"
                        echo "primary import failed: corrupt payload" >&2
                        exit 1
                    fi
                    echo "rollback import failed: disk full" >&2
                    exit 1
                fi"#,
                attempt = attempt_flag.display()
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &script);

        let err = restore_volume(&bin, &managed_target(), &tar_path)
            .await
            .expect_err("double failure must return an error");

        assert!(
            err.contains("primary import failed: corrupt payload"),
            "error must include primary restore failure: {err}"
        );
        assert!(
            err.contains("rollback import failed: disk full"),
            "error must include rollback failure: {err}"
        );
        let marker = "manual recovery at: ";
        let (_, recovery_path_str) = err
            .split_once(marker)
            .expect("error must include recovery archive path");
        let recovery_path = std::path::PathBuf::from(recovery_path_str.trim());
        assert!(
            recovery_path.is_file(),
            "rollback archive must be preserved on disk at {}",
            recovery_path.display()
        );
        assert_eq!(
            std::fs::read(&recovery_path).expect("read preserved rollback archive"),
            b"precious-existing-data"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&recovery_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o600,
                "rollback archive must have owner-only 0600 permissions"
            );
        }

        let calls = invocations(&record);
        let rm_idx = calls
            .iter()
            .position(|line| line == &format!("rm -f {ID}"))
            .expect("container removed");
        let export_idx = calls
            .iter()
            .position(|line| line == &format!("volume export {MANAGED_VOLUME}"))
            .expect("rollback snapshot exported");
        assert!(rm_idx < export_idx);

        let _ = std::fs::remove_file(&recovery_path);
    }

    #[tokio::test]
    async fn export_volume_enforces_size_limit_and_owner_only_permissions() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let dest = dir.path().join("export.tar");
        let script = [
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{}'; fi"#,
                managed_volume_ls_row()
            ),
            format!(
                r#"if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{}'; fi"#,
                managed_volume_inspect_json(true)
            ),
            r#"if [ "$1" = "volume" ] && [ "$2" = "export" ]; then printf '0123456789abcdef'; fi"#
                .to_owned(),
        ]
        .join("\n");
        let (bin, _) = fake_podman(dir.path(), &script);

        // Exceeding the configured limit aborts and leaves no destination or temp file.
        let err = export_volume_with_limit(&bin, &managed_target(), &dest, Some(8))
            .await
            .expect_err("oversized export must fail");
        assert!(err.contains("exceeds the maximum supported size"), "got {err}");
        assert!(!dest.exists());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".pockets-export-"))
            .collect();
        assert!(leftovers.is_empty(), "temp export file must be cleaned up");

        // Within the limit succeeds with 0600 permissions on Unix.
        export_volume_with_limit(&bin, &managed_target(), &dest, Some(16))
            .await
            .expect("export within limit should succeed");
        assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789abcdef");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Unified default limit supports archives well above the old 512 MiB cap.
        assert!(MAX_BACKUP_ARCHIVE_BYTES > 512 * 1024 * 1024);
    }

    /// Idempotence for the retry path: an absent container and an absent
    /// volume are the desired end state, reported after a lookup and with no
    /// removal command ever issued.
    #[tokio::test]
    async fn removing_an_absent_container_and_volume_is_success() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let script = [
            r#"if [ "$1" = "ps" ]; then printf '[]'; fi"#.to_owned(),
            r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '[]'; fi"#.to_owned(),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &script);
        let target = managed_target();

        remove_target(&bin, &target)
            .await
            .expect("an absent container is already deleted");
        remove_volume(&bin, &target)
            .await
            .expect("an absent volume is already deleted");

        assert_eq!(
            invocations(&record),
            vec![
                format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json"),
                format!("volume ls --filter name=^{MANAGED_VOLUME}$ --format json"),
            ],
            "nothing exists to remove, so no removal may run"
        );
    }

    /// The bundled example is never a removal target: the refusal is a match
    /// on the variant, so it happens without a single Podman command.
    #[tokio::test]
    async fn removal_refuses_the_bundled_target_before_touching_the_engine() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        // Any engine call would record itself and fail outright.
        let (bin, record) = fake_podman(
            dir.path(),
            "echo 'the bundled example must never be removed' >&2; exit 1",
        );

        let err = remove_target(&bin, &Target::bundled())
            .await
            .expect_err("the bundled container is never removed");
        assert!(err.contains("bundled"), "got {err}");
        let err = remove_volume(&bin, &Target::bundled())
            .await
            .expect_err("the bundled data volume is never removed");
        assert!(err.contains("bundled"), "got {err}");
        assert!(
            !record.exists(),
            "a refused bundled target must run nothing at all: {:?}",
            std::fs::read_to_string(&record).unwrap_or_default()
        );
    }

    /// The image ids the replacement fixtures pin: the digest the pocket
    /// runs on today and the freshly built replacement digest (the same
    /// digest [`managed_target`] already carries as its "previous").
    const OLD_IMAGE: &str =
        "sha256:abababababababababababababababababababababababababababababababab";
    const NEW_IMAGE: &str =
        "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

    /// The validated loopback host port the running fixtures publish on,
    /// which a replacement must carry over verbatim.
    const HOST_PORT: u16 = 44011;

    /// The port map a running replacement fixture reports: exactly one
    /// `127.0.0.1` binding of the target's internal port on
    /// [`HOST_PORT`], the only mapping shape the engine contract accepts.
    const RUNNING_PORTS_JSON: &str = r#"{"8080/tcp":[{"HostIp":"127.0.0.1","HostPort":"44011"}]}"#;

    /// A distinct container id for the candidate that a successful `run`
    /// brings up, so a preserved port or volume can never be an accident
    /// of the fixtures reusing one id everywhere.
    const REPLACEMENT_NEW_ID: &str =
        "f0e1d2c3b4a5968778695a4b3c2d1e0fabcd9876543210feabcd0123456789ab";

    /// The managed target the replacement fixtures drive, pinned to an
    /// arbitrary immutable digest.
    fn managed_target_on(image_id: &str) -> Target {
        let manifest = manifest::bundled_manifest().expect("bundled manifest parses");
        Target::managed(&manifest, MANAGED_ID, image_id).expect("managed target is built")
    }

    /// One `podman ps --format json` row naming the managed container
    /// with id `id`.
    fn managed_ps_row_with(id: &str) -> String {
        format!(r#"[{{"Id":"{id}","Names":["{MANAGED_CONTAINER}"]}}]"#)
    }

    /// One `podman inspect` element for the managed container in an
    /// arbitrary state; `ports` is the raw port-map JSON.
    fn replacement_inspect_json(id: &str, status: &str, running: bool, ports: &str) -> String {
        format!(
            r#"[{{"Id":"{id}","Name":"{MANAGED_CONTAINER}","State":{{"Status":"{status}","Running":{running}}},"Config":{{"Labels":{{"{OWNERSHIP_LABEL}":"{MANAGED_OWNERSHIP}"}}}},"NetworkSettings":{{"Ports":{ports}}}}}]"#
        )
    }

    /// The exact recorded line a pinned start of `image_id` on
    /// `host_port` emits, spelled by the production
    /// [`Target::run_args`] so the expectation can never drift
    /// from what the code is able to run.
    fn pinned_run_line(image_id: &str, host_port: u16) -> String {
        managed_target_on(image_id).run_args(host_port).join(" ")
    }

    /// Writes an executable stand-in for `podman` from a prepared
    /// script body, recording every invocation exactly like
    /// [`fake_podman`].
    fn write_fake_engine(dir: &Path, script: String) -> (std::path::PathBuf, std::path::PathBuf) {
        let record = dir.join("invocations.txt");
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

    /// A stateful stand-in for `podman` covering the whole replacement
    /// flow. Its container defaults to running behind the validated
    /// [`HOST_PORT`] mapping and changes only as real commands act on
    /// it: `stop` stops it, `rm` retires the definition, and `run`
    /// brings the candidate up — or, per `run_result`, has it exit
    /// immediately (`"exited"`) or fail to start at all (`"fail"`, which
    /// leaves the state untouched). `image exists` answers per
    /// `image_present`. The data volume is reported owned and is not
    /// deletable here at all, so any `volume rm` would be a fixture
    /// bug rather than a passing line. Only shell builtins are used.
    fn fake_replacement_engine(
        dir: &Path,
        run_result: &str,
        image_present: bool,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let record = dir.join("invocations.txt");
        let state = dir.join("state.txt");
        let running_json = replacement_inspect_json(ID, "running", true, RUNNING_PORTS_JSON);
        let exited_json = replacement_inspect_json(ID, "exited", false, "null");
        let candidate_case = match run_result {
            "fail" => "printf 'candidate start exploded' >&2\n         exit 1".to_owned(),
            "exited" => format!("printf 'exited\\n' > '{}'", state.display()),
            _ => format!("printf 'running\\n' > '{}'", state.display()),
        };
        // The digest tells a candidate start from a rollback of the
        // previous image: only the candidate obeys `run_result`; a
        // rollback run always brings the previous container up.
        let create_case = format!(
            "case \"$*\" in\n         *{new_image}*) {candidate} ;;\n         *) printf 'running\\n' > '{state}' ;;\n       esac",
            new_image = NEW_IMAGE,
            candidate = candidate_case,
            state = state.display(),
        );
        let image_case = if image_present {
            ":".to_owned()
        } else {
            "if [ \"$2\" = \"exists\" ]; then printf 'Error: no such image' >&2; exit 1; fi"
                .to_owned()
        };
        let script = format!(
            r#"#!/bin/sh
{{ printf '%s ' "$@"; printf '\n'; }} >> '{record}'
state=running
if [ -f '{state}' ]; then read state < '{state}'; fi
case "$1" in
  ps)
    if [ "$state" = "absent" ]; then printf '[]'; else printf '{ps}'; fi
    ;;
  inspect)
    case "$state" in
      running) printf '{running_json}' ;;
      exited) printf '{exited_json}' ;;
      *) printf 'Error: no such container' >&2
         exit 1 ;;
    esac
    ;;
  image)
    {image_case}
    ;;
  volume)
    if [ "$2" = "ls" ]; then printf '{vol_ls}'; fi
    if [ "$2" = "inspect" ]; then printf '{vol_owned}'; fi
    ;;
  stop) printf 'exited\n' > '{state}' ;;
  rm) printf 'absent\n' > '{state}' ;;
  run)
    {create_case}
    ;;
esac
exit 0
"#,
            record = record.display(),
            state = state.display(),
            ps = managed_ps_row_with(ID),
            running_json = running_json,
            exited_json = exited_json,
            image_case = image_case,
            create_case = create_case,
            vol_ls = managed_volume_ls_row(),
            vol_owned = managed_volume_inspect_json(true),
        );
        let (bin, record) = write_fake_engine(dir, script);
        (bin, record)
    }

    /// A stand-in for `podman` after a replacement has put a *new*
    /// container in place, for exercising [`restore_replacement`] on
    /// its own. With `recreate` set (the receipt said the pocket was
    /// running): the new container is present and running for the
    /// removal lookup, and `run` replaces it with the previous container
    /// reconciled in state `recreate` (`"running"` or `"exited"`). With
    /// `recreate` unset nothing is recreatable, so the flow may only
    /// ever look the new container up. Records like [`fake_podman`].
    fn fake_restore_engine(
        dir: &Path,
        recreate: Option<&str>,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let record = dir.join("invocations.txt");
        let state = dir.join("restored.txt");
        let new_ps = managed_ps_row_with(REPLACEMENT_NEW_ID);
        let new_json =
            replacement_inspect_json(REPLACEMENT_NEW_ID, "running", true, RUNNING_PORTS_JSON);
        let script = match recreate {
            // Before any command: the new container runs. `rm` retires
            // it, and `run` brings the previous container back in the
            // reconciled state the test pinned.
            Some(state_name) => {
                let running = state_name == "running";
                let old_ps = managed_ps_row_with(ID);
                let old_json = replacement_inspect_json(
                    ID,
                    state_name,
                    running,
                    if running { RUNNING_PORTS_JSON } else { "null" },
                );
                format!(
                    r#"#!/bin/sh
{{ printf '%s ' "$@"; printf '\n'; }} >> '{record}'
stage=new
if [ -f '{state}' ]; then read stage < '{state}'; fi
case "$1" in
  ps)
    case "$stage" in
      new) printf '{new_ps}' ;;
      old) printf '{old_ps}' ;;
      *) printf '[]' ;;
    esac
    ;;
  inspect)
    case "$stage" in
      new) printf '{new_json}' ;;
      old) printf '{old_json}' ;;
      *) printf 'Error: no such container' >&2
         exit 1 ;;
    esac
    ;;
  rm) printf 'gone\n' > '{state}' ;;
  run) printf 'old\n' > '{state}' ;;
esac
exit 0
"#,
                    record = record.display(),
                    state = state.display(),
                    new_ps = new_ps,
                    new_json = new_json,
                    old_ps = old_ps,
                    old_json = old_json,
                )
            }
            None => format!(
                r#"#!/bin/sh
{{ printf '%s ' "$@"; printf '\n'; }} >> '{record}'
case "$1" in
  ps) printf '{new_ps}' ;;
  inspect) printf '{new_json}' ;;
esac
exit 0
"#,
                record = record.display(),
                new_ps = new_ps,
                new_json = new_json,
            ),
        };
        write_fake_engine(dir, script)
    }

    /// A running pocket is replaced image-for-image while everything the
    /// user observes stays put: graceful stop, definition retired
    /// without `-v`, and the candidate running on the *same* validated
    /// loopback port and the *same* named volume. The exact recorded
    /// sequence pins the whole contract — lookups prove identity, image,
    /// and volume before anything mutates, and the data volume is never
    /// anywhere near a removal.
    #[tokio::test]
    async fn replacing_a_running_pocket_preserves_its_port_and_volume() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_replacement_engine(dir.path(), "ok", true);

        let receipt = replace_target(
            &bin,
            &managed_target_on(OLD_IMAGE),
            &managed_target_on(NEW_IMAGE),
        )
        .await
        .expect("the candidate replaces the running container");

        assert!(receipt.was_running);
        assert_eq!(receipt.host_port, Some(HOST_PORT));
        assert_eq!(receipt.status.state, PocketState::Running);
        assert_eq!(
            receipt.status.url.as_deref(),
            Some("http://127.0.0.1:44011")
        );

        let lookup = format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json");
        let inspected = format!("inspect {ID}");
        assert_eq!(
            invocations(&record),
            vec![
                // Previous container, its validated port, the candidate
                // image, and the volume are all proved first…
                lookup.clone(),
                inspected.clone(),
                format!("image exists {NEW_IMAGE}"),
                format!("volume ls --filter name=^{MANAGED_VOLUME}$ --format json"),
                format!("volume inspect {MANAGED_VOLUME}"),
                // …then the graceful stop and its read-back…
                lookup.clone(),
                inspected.clone(),
                format!("stop --time 5 {ID}"),
                lookup.clone(),
                inspected.clone(),
                // …the definition-only removal…
                lookup.clone(),
                inspected.clone(),
                format!("rm -f {ID}"),
                // …the pinned start (spelled by run_args: same
                // security flags, same volume, port 44011, new image)…
                pinned_run_line(NEW_IMAGE, HOST_PORT),
                // …and the reconciled read-back.
                lookup,
                inspected,
            ],
        );
    }

    /// An update never starts an app the user left stopped: a stale
    /// owned definition is retired plainly — no `-f`, no `-v`, no
    /// candidate `run`, no health anything — and the receipt reports
    /// Stopped. An absent pocket needs not one command beyond the
    /// lookup that proves it absent.
    #[tokio::test]
    async fn replacing_a_stopped_pocket_retires_the_definition_without_launching() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_replacement_engine(dir.path(), "ok", true);
        std::fs::write(dir.path().join("state.txt"), "exited\n").expect("stale fixture");

        let receipt = replace_target(
            &bin,
            &managed_target_on(OLD_IMAGE),
            &managed_target_on(NEW_IMAGE),
        )
        .await
        .expect("a stopped pocket is replaced definitionally");

        assert!(!receipt.was_running);
        assert_eq!(receipt.host_port, None);
        assert_eq!(receipt.status.state, PocketState::Stopped);
        assert_eq!(receipt.status.url, None);
        assert_eq!(
            invocations(&record),
            vec![
                format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json"),
                format!("inspect {ID}"),
                format!("rm {ID}"),
            ],
            "no stop, no run, not even an image lookup: a stopped pocket stays stopped"
        );

        // Absent: the lookup proves nothing exists, and nothing else runs.
        std::fs::remove_file(&record).expect("clear the first record");
        let (bin, record) = fake_replacement_engine(dir.path(), "ok", true);
        std::fs::write(dir.path().join("state.txt"), "absent\n").expect("absent fixture");

        let receipt = replace_target(
            &bin,
            &managed_target_on(OLD_IMAGE),
            &managed_target_on(NEW_IMAGE),
        )
        .await
        .expect("an absent pocket is already replaced");
        assert!(!receipt.was_running);
        assert_eq!(receipt.status.state, PocketState::Stopped);
        assert_eq!(
            invocations(&record),
            vec![format!(
                "ps --all --filter name=^{MANAGED_CONTAINER}$ --format json"
            )],
        );
    }

    /// A candidate image that is not in the local store stops the
    /// replacement after the read-only lookups: no pull is ever issued,
    /// and above all no stop, remove, or run — a running pocket is never
    /// torn down for an image that is not there.
    #[tokio::test]
    async fn a_missing_candidate_image_is_refused_before_anything_is_torn_down() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_replacement_engine(dir.path(), "ok", false);

        let err = replace_target(
            &bin,
            &managed_target_on(OLD_IMAGE),
            &managed_target_on(NEW_IMAGE),
        )
        .await
        .expect_err("a missing candidate image must stop the swap");
        assert!(
            err.contains("not present in the local container store"),
            "{err}"
        );

        assert_eq!(
            invocations(&record),
            vec![
                format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json"),
                format!("inspect {ID}"),
                format!("image exists {NEW_IMAGE}"),
            ],
            "the refusal lands after lookups only: nothing was stopped or removed"
        );
    }

    /// When the candidate cannot start, the live pocket must not be
    /// left down: the same call rolls back to the previous image, pinned
    /// to the same validated port and the same volume, and the returned
    /// error carries both the original failure and the restored truth.
    #[tokio::test]
    async fn a_failed_candidate_start_rolls_the_previous_container_back_on_the_same_port() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_replacement_engine(dir.path(), "fail", true);

        let err = replace_target(
            &bin,
            &managed_target_on(OLD_IMAGE),
            &managed_target_on(NEW_IMAGE),
        )
        .await
        .expect_err("a candidate that cannot start must fail the replacement");
        assert!(
            err.contains("candidate start exploded"),
            "the original error must survive: {err}"
        );
        assert!(
            err.contains(&format!("restored and is running on port {HOST_PORT}")),
            "the rollback outcome must be stated: {err}"
        );

        let lookup = format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json");
        let inspected = format!("inspect {ID}");
        assert_eq!(
            invocations(&record),
            vec![
                lookup.clone(),
                inspected.clone(),
                format!("image exists {NEW_IMAGE}"),
                format!("volume ls --filter name=^{MANAGED_VOLUME}$ --format json"),
                format!("volume inspect {MANAGED_VOLUME}"),
                lookup.clone(),
                inspected.clone(),
                format!("stop --time 5 {ID}"),
                lookup.clone(),
                inspected.clone(),
                lookup.clone(),
                inspected.clone(),
                format!("rm -f {ID}"),
                // The candidate run fails; the container never exists,
                // so the rollback lookup removes nothing…
                pinned_run_line(NEW_IMAGE, HOST_PORT),
                lookup.clone(),
                // …and recreates the previous container pinned to the
                // same port and volume, reconciled by the read-back.
                pinned_run_line(OLD_IMAGE, HOST_PORT),
                lookup,
                inspected,
            ],
        );
    }

    /// A candidate that starts and then exits is not "replaced": the
    /// reconciled status says it is not running, the candidate is
    /// removed, and the previous container goes back on the same port
    /// before the error returns — the error states both plainly rather
    /// than leaving anyone to guess what is live now.
    #[tokio::test]
    async fn a_candidate_that_never_reconciles_running_rolls_back_to_the_previous_one() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_replacement_engine(dir.path(), "exited", true);

        let err = replace_target(
            &bin,
            &managed_target_on(OLD_IMAGE),
            &managed_target_on(NEW_IMAGE),
        )
        .await
        .expect_err("a candidate that exits immediately must fail the replacement");
        assert!(err.contains("is not running"), "original failure: {err}");
        assert!(
            err.contains(&format!("restored and is running on port {HOST_PORT}")),
            "the rollback outcome must be stated: {err}"
        );

        let lookup = format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json");
        let inspected = format!("inspect {ID}");
        assert_eq!(
            invocations(&record),
            vec![
                lookup.clone(),
                inspected.clone(),
                format!("image exists {NEW_IMAGE}"),
                format!("volume ls --filter name=^{MANAGED_VOLUME}$ --format json"),
                format!("volume inspect {MANAGED_VOLUME}"),
                lookup.clone(),
                inspected.clone(),
                format!("stop --time 5 {ID}"),
                lookup.clone(),
                inspected.clone(),
                lookup.clone(),
                inspected.clone(),
                format!("rm -f {ID}"),
                // The candidate runs but is not up when reconciled.
                pinned_run_line(NEW_IMAGE, HOST_PORT),
                lookup.clone(),
                inspected.clone(),
                // Rollback removes the candidate container, then puts
                // the previous one back pinned to the same port.
                lookup.clone(),
                inspected.clone(),
                format!("rm -f {ID}"),
                pinned_run_line(OLD_IMAGE, HOST_PORT),
                lookup,
                inspected,
            ],
        );
    }

    /// A higher layer that found the new container unhealthy hands back
    /// the receipt: the restore removes only the current owned container
    /// — it was running, so `-f` — and recreates the previous container
    /// pinned to the receipt's exact port on the same volume. No volume
    /// removal is even expressible against this fixture.
    #[tokio::test]
    async fn restore_replacement_removes_the_new_container_and_recreates_the_previous_one() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_restore_engine(dir.path(), Some("running"));
        let receipt = ReplacementReceipt {
            status: status_for(None, MANAGED_CONTAINER),
            was_running: true,
            host_port: Some(HOST_PORT),
            bindings: Default::default(),
        };

        let restored = restore_replacement(
            &bin,
            &managed_target_on(NEW_IMAGE),
            &managed_target_on(OLD_IMAGE),
            &receipt,
        )
        .await
        .expect("the previous container is recreated");

        assert_eq!(restored.state, PocketState::Running);
        assert_eq!(restored.url.as_deref(), Some("http://127.0.0.1:44011"));
        assert_eq!(
            invocations(&record),
            vec![
                format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json"),
                format!("inspect {REPLACEMENT_NEW_ID}"),
                format!("rm -f {REPLACEMENT_NEW_ID}"),
                pinned_run_line(OLD_IMAGE, HOST_PORT),
                format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json"),
                format!("inspect {ID}"),
            ],
            "only the inspected new-container id is removed, and the previous \
             image returns pinned to the receipt's port"
        );
    }

    /// A receipt that says the pocket was stopped means the user's
    /// stopped app stays stopped: the restore removes the current owned
    /// container and recreates nothing, however tempting the empty port
    /// map makes a launch look.
    #[tokio::test]
    async fn restoring_a_replacement_for_a_stopped_pocket_recreates_nothing() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_restore_engine(dir.path(), None);
        let receipt = ReplacementReceipt {
            status: status_for(None, MANAGED_CONTAINER),
            was_running: false,
            host_port: None,
            bindings: Default::default(),
        };

        let restored = restore_replacement(
            &bin,
            &managed_target_on(NEW_IMAGE),
            &managed_target_on(OLD_IMAGE),
            &receipt,
        )
        .await
        .expect("removing the current container is the whole job");

        assert_eq!(restored.state, PocketState::Stopped);
        assert_eq!(restored.url, None);
        assert_eq!(
            invocations(&record),
            vec![
                format!("ps --all --filter name=^{MANAGED_CONTAINER}$ --format json"),
                format!("inspect {REPLACEMENT_NEW_ID}"),
                format!("rm -f {REPLACEMENT_NEW_ID}"),
            ],
            "a stopped receipt never runs a container again"
        );
    }

    /// A receipt claiming a running pocket without a valid loopback port
    /// would restore onto an arbitrary address, so it is refused as
    /// malformed — and a mismatched or bundled target pair likewise —
    /// before a single Podman command could run.
    #[tokio::test]
    async fn restore_refuses_malformed_receipts_before_touching_the_engine() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_restore_engine(dir.path(), Some("running"));
        let current = managed_target_on(NEW_IMAGE);
        let previous = managed_target_on(OLD_IMAGE);

        for port in [None, Some(0u16)] {
            let receipt = ReplacementReceipt {
                status: status_for(None, MANAGED_CONTAINER),
                was_running: true,
                host_port: port,
                bindings: Default::default(),
            };
            let err = restore_replacement(&bin, &current, &previous, &receipt)
                .await
                .expect_err("a running receipt needs a real port");
            assert!(
                err.contains("host port"),
                "expected a receipt refusal: {err}"
            );
        }

        // Same pair rule as replace_target: bundled and foreign ids are
        // refused without an engine call.
        let stopped_receipt = ReplacementReceipt {
            status: status_for(None, MANAGED_CONTAINER),
            was_running: false,
            host_port: None,
            bindings: Default::default(),
        };
        let err = restore_replacement(&bin, &Target::bundled(), &previous, &stopped_receipt)
            .await
            .expect_err("the bundled example is never a restore target");
        assert!(err.contains("bundled"), "{err}");

        assert!(
            !record.exists(),
            "a refused restore must run nothing at all: {:?}",
            std::fs::read_to_string(&record).unwrap_or_default()
        );
    }

    /// The receipt's port is pinned verbatim, at the top of the range
    /// included, and a restore that comes back not-running is reported
    /// as not-running — the reconciled status never claims health the
    /// engine did not confirm.
    #[tokio::test]
    async fn restore_pins_the_receipt_port_verbatim_and_reconciles_the_truth() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_restore_engine(dir.path(), Some("exited"));
        let receipt = ReplacementReceipt {
            status: status_for(None, MANAGED_CONTAINER),
            was_running: true,
            host_port: Some(u16::MAX),
            bindings: Default::default(),
        };

        let restored = restore_replacement(
            &bin,
            &managed_target_on(NEW_IMAGE),
            &managed_target_on(OLD_IMAGE),
            &receipt,
        )
        .await
        .expect("the restore itself ran, even though the pocket is down");

        assert_eq!(restored.state, PocketState::Stopped);
        assert_eq!(restored.url, None);
        assert!(
            invocations(&record).contains(&pinned_run_line(OLD_IMAGE, u16::MAX)),
            "the receipt port is published verbatim: {:?}",
            invocations(&record)
        );
    }

    /// The whole replacement surface refuses before the engine: a
    /// bundled target on either side, a pair spanning two projects, and
    /// a pair whose storage mount path moved are all match-and-return
    /// decisions with zero commands issued. A foreign container squatting
    /// on the managed name is refused by the ordinary ownership lookup —
    /// after lookups only, with no stop, remove, or run ever considered.
    #[tokio::test]
    async fn replace_refuses_bundled_and_mismatched_targets_before_touching_the_engine() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (bin, record) = fake_podman(
            dir.path(),
            "echo 'a refused replacement must not call the engine' >&2; exit 1",
        );
        let previous = managed_target_on(OLD_IMAGE);

        let err = replace_target(&bin, &Target::bundled(), &previous)
            .await
            .expect_err("the bundled container is never a replacement's previous");
        assert!(err.contains("bundled"), "{err}");
        let err = replace_target(&bin, &previous, &Target::bundled())
            .await
            .expect_err("the bundled container is never a replacement's next");
        assert!(err.contains("bundled"), "{err}");

        let manifest = manifest::bundled_manifest().expect("bundled manifest parses");
        let foreign_id = "f".repeat(PROJECT_ID_LEN);
        let foreign_project = Target::managed(&manifest, &foreign_id, OLD_IMAGE)
            .expect("a second managed target is built");
        let err = replace_target(&bin, &previous, &foreign_project)
            .await
            .expect_err("a replacement must never span two projects");
        assert!(err.contains("different projects"), "{err}");

        let moved = Manifest {
            storage: manifest::Storage {
                path: "/var/lib/moved".to_owned(),
                ..manifest.storage.clone()
            },
            ..manifest.clone()
        };
        let moved_target =
            Target::managed(&moved, MANAGED_ID, NEW_IMAGE).expect("moved target is built");
        let err = replace_target(&bin, &previous, &moved_target)
            .await
            .expect_err("a moved mount path is a different contract, not an update");
        assert!(err.contains("mount path"), "{err}");

        assert!(
            !record.exists(),
            "refused pairs must run nothing at all: {:?}",
            std::fs::read_to_string(&record).unwrap_or_default()
        );

        // Foreign container on the managed name: the mapping-required
        // ownership lookup is the refusal, and it is the only thing the
        // engine sees.
        let foreign_container = [
            format!(
                r#"if [ "$1" = "ps" ]; then printf '{}'; fi"#,
                managed_ps_row()
            ),
            format!(
                r#"if [ "$1" = "inspect" ]; then printf '{}'; fi"#,
                managed_inspect_json(None)
            ),
        ]
        .join("\n");
        let (bin, record) = fake_podman(dir.path(), &foreign_container);

        let err = replace_target(&bin, &previous, &managed_target_on(NEW_IMAGE))
            .await
            .expect_err("a foreign container under the managed name must be refused");
        assert!(err.contains("refusing"), "{err}");

        let calls = invocations(&record);
        assert_eq!(calls.len(), 2, "lookup only: {calls:?}");
        for destructive in ["rm", "run", "stop", "volume rm"] {
            assert!(
                !calls.iter().any(|line| line.starts_with(destructive)),
                "a refused container must never see {destructive}: {calls:?}"
            );
        }
    }

    /// A host port is only ever derived from the engine's own validated
    /// `http://127.0.0.1:<port>` URL; anything else — another scheme or
    /// host, a trailing path, a non-numeric, zero, or out-of-range port —
    /// yields nothing to pin.
    #[test]
    fn host_ports_are_derived_only_from_validated_loopback_urls() {
        assert_eq!(loopback_url_port("http://127.0.0.1:44011"), Some(44011));
        assert_eq!(loopback_url_port("http://127.0.0.1:65535"), Some(u16::MAX));
        for url in [
            "http://127.0.0.1:0",
            "http://127.0.0.1",
            "http://127.0.0.1:44011/",
            "http://127.0.0.1:44011/healthz",
            "http://127.0.0.1:backport",
            "http://127.0.0.1:70000",
            "http://127.0.0.1:-1",
            "http://0.0.0.0:44011",
            "http://evil.example:44011",
            "https://127.0.0.1:44011",
            "http://127.0.0.11:44011",
        ] {
            assert_eq!(loopback_url_port(url), None, "{url} must yield no port");
        }
    }
}
