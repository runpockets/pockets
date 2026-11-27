//! Inspection and safe removal logic for `pockets uninstall`.
//!
//! All Podman resource deletion is strictly scoped to Pockets-owned resources
//! (identified by `dev.pockets.*` ownership labels or Pockets' reserved name
//! prefixes). Uninstalling Podman itself is always opt-in, checks for
//! non-Pockets containers and volumes before prompting, and uses the matching
//! package manager without shell interpolation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::app_update::{current_install_method, find_brew, InstallMethod};
use crate::{runtime, AppContext};

const BREW_TIMEOUT: Duration = Duration::from_secs(180);
const BREW_SERVICE_TIMEOUT: Duration = Duration::from_secs(60);
const MACHINE_STOP_TIMEOUT: Duration = Duration::from_secs(90);
const MACHINE_RM_TIMEOUT: Duration = Duration::from_secs(120);
const CLEANUP_CMD_TIMEOUT: Duration = Duration::from_secs(60);

/// Linux package managers supported by `packaging/install.sh` and `pockets uninstall`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxPackageManager {
    Apt,
    Dnf,
    Pacman,
}

impl LinuxPackageManager {
    pub fn name(self) -> &'static str {
        match self {
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Pacman => "pacman",
        }
    }

    pub fn remove_program_and_args(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::Apt => ("apt-get", &["remove", "-y", "podman"]),
            Self::Dnf => ("dnf", &["remove", "-y", "podman"]),
            Self::Pacman => ("pacman", &["-R", "--noconfirm", "podman"]),
        }
    }

    pub fn display_remove_command(self) -> &'static str {
        match self {
            Self::Apt => "sudo apt-get remove -y podman",
            Self::Dnf => "sudo dnf remove -y podman",
            Self::Pacman => "sudo pacman -R --noconfirm podman",
        }
    }
}

/// How Podman is installed on this machine, determining whether and how
/// `pockets uninstall` can offer to remove it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodmanInstallKind {
    NotInstalled,
    Homebrew {
        brew: PathBuf,
        prefix: PathBuf,
        podman_bin: PathBuf,
        podman_desktop_present: bool,
    },
    LinuxPackageManager {
        manager: LinuxPackageManager,
        podman_bin: PathBuf,
    },
    External {
        podman_bin: PathBuf,
        reason: String,
    },
}

impl PodmanInstallKind {
    pub fn podman_bin(&self) -> Option<&Path> {
        match self {
            Self::NotInstalled => None,
            Self::Homebrew { podman_bin, .. }
            | Self::LinuxPackageManager { podman_bin, .. }
            | Self::External { podman_bin, .. } => Some(podman_bin.as_path()),
        }
    }

    pub fn can_auto_uninstall(&self) -> bool {
        matches!(
            self,
            Self::Homebrew { .. } | Self::LinuxPackageManager { .. }
        )
    }
}

/// Summary of an existing Podman machine VM (macOS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanMachineInfo {
    pub name: String,
    pub running: bool,
    pub is_default: bool,
}

/// Read-only inventory of Podman resources found during uninstall inspection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PodmanInventory {
    pub engine_reachable: bool,
    pub pockets_containers: Vec<String>,
    pub other_containers: Vec<String>,
    pub pockets_volumes: Vec<String>,
    pub other_volumes: Vec<String>,
    pub pockets_networks: Vec<String>,
    pub pockets_secrets: Vec<String>,
    pub pockets_images: Vec<String>,
    pub machines: Vec<PodmanMachineInfo>,
}

impl PodmanInventory {
    pub fn has_other_workloads(&self) -> bool {
        !self.other_containers.is_empty() || !self.other_volumes.is_empty()
    }

    pub fn has_pockets_resources(&self) -> bool {
        !self.pockets_containers.is_empty()
            || !self.pockets_volumes.is_empty()
            || !self.pockets_networks.is_empty()
            || !self.pockets_secrets.is_empty()
            || !self.pockets_images.is_empty()
    }
}

/// Full read-only snapshot gathered before prompting the user.
#[derive(Debug, Clone)]
pub struct UninstallInspection {
    pub install_method: InstallMethod,
    pub data_dir: PathBuf,
    pub data_dir_exists: bool,
    pub registered_pockets_count: usize,
    pub podman_install: PodmanInstallKind,
    pub podman_inventory: Option<PodmanInventory>,
}

/// Result of removing Pockets-managed Podman resources and local data dir.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PocketsDataCleanupReport {
    pub containers_removed: usize,
    pub volumes_removed: usize,
    pub networks_removed: usize,
    pub secrets_removed: usize,
    pub images_removed: usize,
    pub data_dir_removed: bool,
    pub warnings: Vec<String>,
}

/// Result of removing the Pockets binary / Homebrew formula itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PocketsUninstallOutcome {
    UninstalledHomebrew { prefix: PathBuf },
    RemovedBinary { exe_path: PathBuf },
    SkippedDevelopment,
}

/// Read-only inspection of the local Pockets and Podman installation.
pub async fn inspect(ctx: &AppContext) -> UninstallInspection {
    let install_method = current_install_method();
    let data_dir = ctx.data_dir().to_path_buf();
    let data_dir_exists = data_dir.exists();
    let registered_pockets_count = ctx.store().list().map(|items| items.len()).unwrap_or(0);

    // Probe runtime first so `runtime::find_podman()` prefers the active binary
    // on multi-install machines (e.g. Homebrew + Podman Desktop).
    let report = runtime::detect_runtime().await;
    let podman_install = detect_podman_install(&install_method).await;
    let podman_inventory = match podman_install.podman_bin() {
        Some(bin) => Some(inspect_podman_inventory(bin, report.state == runtime::RuntimeState::Ready).await),
        None => None,
    };

    UninstallInspection {
        install_method,
        data_dir,
        data_dir_exists,
        registered_pockets_count,
        podman_install,
        podman_inventory,
    }
}

async fn detect_podman_install(install_method: &InstallMethod) -> PodmanInstallKind {
    // 1. Check whether Pockets itself is in Homebrew alongside Homebrew Podman.
    if let InstallMethod::Homebrew {
        prefix,
        has_brew_podman: true,
    } = install_method
    {
        if let Some(brew) = find_brew(prefix) {
            let podman_bin = [
                prefix.join("opt").join("podman").join("bin").join("podman"),
                prefix.join("bin").join("podman"),
            ]
            .into_iter()
            .find(|p| p.is_file())
            .or_else(runtime::find_podman);

            if let Some(podman_bin) = podman_bin {
                return PodmanInstallKind::Homebrew {
                    brew,
                    prefix: prefix.clone(),
                    podman_bin,
                    podman_desktop_present: is_podman_desktop_present(),
                };
            }
        }
    }

    // 2. Otherwise inspect any discovered `podman` executable(s).
    let Some(podman_bin) = runtime::find_podman() else {
        return PodmanInstallKind::NotInstalled;
    };

    if let Some((prefix, brew)) = detect_homebrew_podman_from_bin(&podman_bin) {
        return PodmanInstallKind::Homebrew {
            brew,
            prefix,
            podman_bin,
            podman_desktop_present: is_podman_desktop_present(),
        };
    }

    #[cfg(target_os = "macos")]
    {
        PodmanInstallKind::External {
            podman_bin,
            reason: "Podman appears to be managed outside Homebrew (e.g. Podman Desktop or the official macOS installer).".to_owned(),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        if let Some(manager) = detect_linux_podman_package_manager().await {
            return PodmanInstallKind::LinuxPackageManager {
                manager,
                podman_bin,
            };
        }
        PodmanInstallKind::External {
            podman_bin,
            reason: "Podman does not appear to be managed by apt, dnf, or pacman.".to_owned(),
        }
    }
}

fn detect_homebrew_podman_from_bin(podman_bin: &Path) -> Option<(PathBuf, PathBuf)> {
    let canonical = std::fs::canonicalize(podman_bin).ok()?;
    let prefix = homebrew_prefix_from_podman_path(&canonical)?;
    let brew = find_brew(&prefix)?;
    Some((prefix, brew))
}

fn homebrew_prefix_from_podman_path(canonical: &Path) -> Option<PathBuf> {
    let normalized = canonical.to_string_lossy().replace('\\', "/");
    let idx = normalized.find("/Cellar/podman/")?;
    Some(PathBuf::from(&normalized[..idx]))
}

fn is_podman_desktop_present() -> bool {
    #[cfg(target_os = "macos")]
    {
        Path::new("/Applications/Podman Desktop.app").exists()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn detect_linux_podman_package_manager() -> Option<LinuxPackageManager> {
    if command_exists("dpkg-query")
        && command_exists("apt-get")
        && probe_stdout_equals("dpkg-query", &["-W", "-f=${Status}", "podman"], "install ok installed").await
    {
        return Some(LinuxPackageManager::Apt);
    }
    if command_exists("rpm")
        && command_exists("dnf")
        && probe_exit_success("rpm", &["-q", "podman"]).await
    {
        return Some(LinuxPackageManager::Dnf);
    }
    if command_exists("pacman") && probe_exit_success("pacman", &["-Q", "podman"]).await {
        return Some(LinuxPackageManager::Pacman);
    }
    None
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .any(|p| p.is_file())
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn probe_exit_success(program: &str, args: &[&str]) -> bool {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .ok()
        .and_then(|res| res.ok())
        .is_some_and(|status| status.success())
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn probe_stdout_equals(program: &str, args: &[&str], expected: &str) -> bool {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(child) = child else {
        return false;
    };
    tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .ok()
        .and_then(|res| res.ok())
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .is_some_and(|stdout| stdout.trim() == expected)
}

async fn inspect_podman_inventory(bin: &Path, engine_hint_ready: bool) -> PodmanInventory {
    let mut inventory = PodmanInventory::default();

    // `podman machine list` works even when the VM is stopped on macOS.
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = runtime::run(bin, &["machine", "list", "--format", "json"]).await {
            if let Ok(machines) = parse_machines_json(&output.stdout) {
                inventory.machines = machines;
            }
        }
    }

    let ps_res = runtime::run(bin, &["ps", "--all", "--format", "json"]).await;
    let engine_reachable = ps_res.is_ok()
        || (engine_hint_ready
            && runtime::run(bin, &["info", "--format", "json"])
                .await
                .is_ok());
    inventory.engine_reachable = engine_reachable;

    if let Ok(output) = ps_res {
        if let Ok((pockets, other)) = parse_containers_json(&output.stdout) {
            inventory.pockets_containers = pockets;
            inventory.other_containers = other;
        }
    }

    if !engine_reachable {
        return inventory;
    }

    if let Ok(output) = runtime::run(bin, &["volume", "ls", "--format", "json"]).await {
        if let Ok((pockets, other)) = parse_volumes_json(&output.stdout) {
            inventory.pockets_volumes = pockets;
            inventory.other_volumes = other;
        }
    }

    if let Ok(output) = runtime::run(bin, &["network", "ls", "--format", "json"]).await {
        if let Ok(pockets) = parse_networks_json(&output.stdout) {
            inventory.pockets_networks = pockets;
        }
    }

    if let Ok(output) = runtime::run(bin, &["secret", "ls", "--format", "json"]).await {
        if let Ok(pockets) = parse_secrets_json(&output.stdout) {
            inventory.pockets_secrets = pockets;
        }
    }

    if let Ok(output) = runtime::run(bin, &["images", "--format", "json"]).await {
        if let Ok(pockets) = parse_images_json(&output.stdout) {
            inventory.pockets_images = pockets;
        }
    }

    inventory
}

/// Removes Pockets-managed Podman containers, volumes, networks, secrets,
/// images, and the local Pockets data directory. Never touches non-Pockets
/// resources or user Pocket source directories.
pub async fn remove_pockets_data(
    data_dir: &Path,
    podman_bin: Option<&Path>,
    inventory: Option<&PodmanInventory>,
    removing_podman_machines: bool,
) -> PocketsDataCleanupReport {
    let mut report = PocketsDataCleanupReport::default();

    if let (Some(bin), Some(inv)) = (podman_bin, inventory) {
        if inv.engine_reachable {
            for container in &inv.pockets_containers {
                match runtime::run_with_timeout(
                    bin,
                    &["rm", "-f", container],
                    CLEANUP_CMD_TIMEOUT,
                )
                .await
                {
                    Ok(_) => report.containers_removed += 1,
                    Err(err) => report
                        .warnings
                        .push(format!("Could not remove container {container}: {err}")),
                }
            }

            for volume in &inv.pockets_volumes {
                match runtime::run_with_timeout(
                    bin,
                    &["volume", "rm", "-f", volume],
                    CLEANUP_CMD_TIMEOUT,
                )
                .await
                {
                    Ok(_) => report.volumes_removed += 1,
                    Err(err) => report
                        .warnings
                        .push(format!("Could not remove volume {volume}: {err}")),
                }
            }

            for network in &inv.pockets_networks {
                match runtime::run_with_timeout(
                    bin,
                    &["network", "rm", network],
                    CLEANUP_CMD_TIMEOUT,
                )
                .await
                {
                    Ok(_) => report.networks_removed += 1,
                    Err(err) => report
                        .warnings
                        .push(format!("Could not remove network {network}: {err}")),
                }
            }

            for secret in &inv.pockets_secrets {
                match runtime::run_with_timeout(
                    bin,
                    &["secret", "rm", secret],
                    CLEANUP_CMD_TIMEOUT,
                )
                .await
                {
                    Ok(_) => report.secrets_removed += 1,
                    Err(err) => report
                        .warnings
                        .push(format!("Could not remove secret {secret}: {err}")),
                }
            }

            for image in &inv.pockets_images {
                match runtime::run_with_timeout(
                    bin,
                    &["rmi", "-f", image],
                    CLEANUP_CMD_TIMEOUT,
                )
                .await
                {
                    Ok(_) => report.images_removed += 1,
                    Err(err) => report
                        .warnings
                        .push(format!("Could not remove image {image}: {err}")),
                }
            }
        } else if !removing_podman_machines {
            report.warnings.push(
                "Podman engine was not running, so Pockets containers and volumes inside Podman were not inspected or removed."
                    .to_owned(),
            );
        }
    }

    if data_dir.exists() {
        match std::fs::remove_dir_all(data_dir) {
            Ok(()) => report.data_dir_removed = true,
            Err(err) => report.warnings.push(format!(
                "Could not remove Pockets data directory {}: {err}",
                data_dir.display()
            )),
        }
    }

    report
}

/// Stops and removes the given Podman machine VM(s). Must be called before
/// uninstalling the `podman` CLI binary itself.
pub async fn stop_and_remove_podman_machines(
    podman_bin: &Path,
    machines: &[PodmanMachineInfo],
) -> Result<usize, String> {
    let mut removed = 0usize;
    for machine in machines {
        if machine.running {
            let _ = runtime::run_with_timeout(
                podman_bin,
                &["machine", "stop", &machine.name],
                MACHINE_STOP_TIMEOUT,
            )
            .await;
        }
        runtime::run_with_timeout(
            podman_bin,
            &["machine", "rm", "-f", &machine.name],
            MACHINE_RM_TIMEOUT,
        )
        .await
        .map_err(|err| format!("Could not remove Podman machine '{}': {err}", machine.name))?;
        removed += 1;
    }
    Ok(removed)
}

/// Stops Homebrew's `pockets` background service if installed and running.
pub async fn stop_homebrew_service(prefix: &Path) -> Result<(), String> {
    let Some(brew) = find_brew(prefix) else {
        return Ok(());
    };
    let _ = run_bounded(&brew, &["services", "stop", "pockets"], BREW_SERVICE_TIMEOUT).await;
    Ok(())
}

/// Uninstalls Pockets itself according to its detected [`InstallMethod`].
///
/// For Homebrew installs, this MUST run before `brew uninstall podman` because
/// Homebrew refuses to uninstall a dependency while `pockets` still depends on it.
pub async fn uninstall_pockets_package(
    install_method: &InstallMethod,
) -> Result<PocketsUninstallOutcome, String> {
    match install_method {
        InstallMethod::Homebrew { prefix, .. } => {
            let brew = find_brew(prefix).ok_or_else(|| {
                "Could not locate `brew`. Run `brew uninstall pockets` manually.".to_owned()
            })?;
            run_bounded(&brew, &["uninstall", "pockets"], BREW_TIMEOUT).await?;
            Ok(PocketsUninstallOutcome::UninstalledHomebrew {
                prefix: prefix.clone(),
            })
        }
        InstallMethod::Binary { exe_path } => {
            std::fs::remove_file(exe_path).map_err(|err| {
                format!(
                    "Could not remove Pockets binary at {}: {err}",
                    exe_path.display()
                )
            })?;
            Ok(PocketsUninstallOutcome::RemovedBinary {
                exe_path: exe_path.clone(),
            })
        }
        InstallMethod::Development => Ok(PocketsUninstallOutcome::SkippedDevelopment),
    }
}

/// Uninstalls Podman using the detected package manager.
pub async fn uninstall_podman_package(kind: &PodmanInstallKind) -> Result<String, String> {
    match kind {
        PodmanInstallKind::Homebrew { brew, .. } => {
            run_bounded(brew, &["uninstall", "podman"], BREW_TIMEOUT).await?;
            Ok("brew uninstall podman".to_owned())
        }
        PodmanInstallKind::LinuxPackageManager { manager, .. } => {
            let (program, args) = manager.remove_program_and_args();
            let running_as_root = is_running_as_root().await;
            let (exec_prog, exec_args): (&str, Vec<&str>) = if running_as_root {
                (program, args.to_vec())
            } else {
                let mut combined = Vec::with_capacity(args.len() + 1);
                combined.push(program);
                combined.extend_from_slice(args);
                ("sudo", combined)
            };

            let status = Command::new(exec_prog)
                .args(&exec_args)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .await
                .map_err(|err| format!("Could not run `{exec_prog}`: {err}"))?;

            if !status.success() {
                return Err(format!(
                    "`{}` exited with {status}.",
                    manager.display_remove_command()
                ));
            }
            Ok(manager.display_remove_command().to_owned())
        }
        PodmanInstallKind::NotInstalled => Err("Podman is not installed.".to_owned()),
        PodmanInstallKind::External { reason, .. } => Err(reason.clone()),
    }
}

async fn is_running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .is_some_and(|s| s.trim() == "0")
}

async fn run_bounded(bin: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|err| format!("Could not run {}: {err}", bin.display()))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "{} {} timed out after {} seconds.",
                bin.display(),
                args.join(" "),
                timeout.as_secs()
            )
        })?
        .map_err(|err| format!("Failed waiting for {}: {err}", bin.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!(
            "`{} {}` failed: {}",
            bin.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| bin.display().to_string()),
            args.join(" "),
            if detail.is_empty() {
                output.status.to_string()
            } else {
                detail
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Classification & JSON parsing helpers
// ---------------------------------------------------------------------------

pub(crate) fn has_pockets_label(labels: &BTreeMap<String, String>) -> bool {
    labels.keys().any(|k| k.starts_with("dev.pockets."))
}

pub(crate) fn is_pockets_container(names: &[String], labels: &BTreeMap<String, String>) -> bool {
    if has_pockets_label(labels) {
        return true;
    }
    names.iter().any(|raw| {
        let name = raw.trim_start_matches('/');
        name == "pockets-hello-pocket"
            || name.starts_with("pockets-project-")
            || name.starts_with("pockets-preview-")
            || name.starts_with("pockets-egress-")
    })
}

pub(crate) fn is_pockets_volume(name: &str, labels: &BTreeMap<String, String>) -> bool {
    has_pockets_label(labels)
        || name == "pockets-hello-pocket-data"
        || name.starts_with("pockets-project-")
        || name.starts_with("pockets-preview-")
}

pub(crate) fn is_pockets_network(name: &str, labels: &BTreeMap<String, String>) -> bool {
    has_pockets_label(labels)
        || name == "pockets-preview"
        || name.starts_with("pockets-egress-net-")
}

pub(crate) fn is_pockets_secret(name: &str, labels: &BTreeMap<String, String>) -> bool {
    has_pockets_label(labels) || name.starts_with("pockets-runtime-")
}

pub(crate) fn is_pockets_image_ref(reference: &str) -> bool {
    reference.starts_with("localhost/pockets-")
}

#[derive(Deserialize)]
struct PsRow {
    #[serde(rename = "Id", alias = "id", default)]
    id: String,
    #[serde(rename = "Names", alias = "names", default)]
    names: Option<Vec<String>>,
    #[serde(rename = "Labels", alias = "labels", default)]
    labels: Option<BTreeMap<String, String>>,
}

fn parse_containers_json(bytes: &[u8]) -> Result<(Vec<String>, Vec<String>), String> {
    let trimmed = std::str::from_utf8(bytes)
        .map_err(|err| err.to_string())?
        .trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok((Vec::new(), Vec::new()));
    }
    let rows: Vec<PsRow> = serde_json::from_str(trimmed).map_err(|err| err.to_string())?;
    let mut pockets = Vec::new();
    let mut other = Vec::new();
    for row in rows {
        let names = row.names.unwrap_or_default();
        let labels = row.labels.unwrap_or_default();
        let primary = names
            .first()
            .map(|n| n.trim_start_matches('/').to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or(row.id);
        if primary.is_empty() {
            continue;
        }
        if is_pockets_container(&names, &labels) {
            pockets.push(primary);
        } else {
            other.push(primary);
        }
    }
    Ok((pockets, other))
}

#[derive(Deserialize)]
struct VolumeRow {
    #[serde(rename = "Name", alias = "name", default)]
    name: String,
    #[serde(rename = "Labels", alias = "labels", default)]
    labels: Option<BTreeMap<String, String>>,
}

fn parse_volumes_json(bytes: &[u8]) -> Result<(Vec<String>, Vec<String>), String> {
    let trimmed = std::str::from_utf8(bytes)
        .map_err(|err| err.to_string())?
        .trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok((Vec::new(), Vec::new()));
    }
    let rows: Vec<VolumeRow> = serde_json::from_str(trimmed).map_err(|err| err.to_string())?;
    let mut pockets = Vec::new();
    let mut other = Vec::new();
    for row in rows {
        if row.name.is_empty() {
            continue;
        }
        let labels = row.labels.unwrap_or_default();
        if is_pockets_volume(&row.name, &labels) {
            pockets.push(row.name);
        } else {
            other.push(row.name);
        }
    }
    Ok((pockets, other))
}

#[derive(Deserialize)]
struct NetworkRow {
    #[serde(rename = "name", alias = "Name", default)]
    name: String,
    #[serde(rename = "labels", alias = "Labels", default)]
    labels: Option<BTreeMap<String, String>>,
}

fn parse_networks_json(bytes: &[u8]) -> Result<Vec<String>, String> {
    let trimmed = std::str::from_utf8(bytes)
        .map_err(|err| err.to_string())?
        .trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }
    let rows: Vec<NetworkRow> = serde_json::from_str(trimmed).map_err(|err| err.to_string())?;
    let mut pockets = Vec::new();
    for row in rows {
        if row.name.is_empty() {
            continue;
        }
        let labels = row.labels.unwrap_or_default();
        if is_pockets_network(&row.name, &labels) {
            pockets.push(row.name);
        }
    }
    Ok(pockets)
}

#[derive(Deserialize)]
struct SecretRow {
    #[serde(rename = "Name", alias = "name", default)]
    name: Option<String>,
    #[serde(rename = "Labels", alias = "labels", default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(rename = "Spec", alias = "spec", default)]
    spec: Option<SecretSpecRow>,
}

#[derive(Deserialize)]
struct SecretSpecRow {
    #[serde(rename = "Name", alias = "name", default)]
    name: String,
    #[serde(rename = "Labels", alias = "labels", default)]
    labels: Option<BTreeMap<String, String>>,
}

fn parse_secrets_json(bytes: &[u8]) -> Result<Vec<String>, String> {
    let trimmed = std::str::from_utf8(bytes)
        .map_err(|err| err.to_string())?
        .trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }
    let rows: Vec<SecretRow> = serde_json::from_str(trimmed).map_err(|err| err.to_string())?;
    let mut pockets = Vec::new();
    for row in rows {
        let (name, labels) = match row.spec {
            Some(spec) => (spec.name, spec.labels.unwrap_or_default()),
            None => (row.name.unwrap_or_default(), row.labels.unwrap_or_default()),
        };
        if name.is_empty() {
            continue;
        }
        if is_pockets_secret(&name, &labels) {
            pockets.push(name);
        }
    }
    Ok(pockets)
}

#[derive(Deserialize)]
struct ImageRow {
    #[serde(rename = "Names", alias = "names", default)]
    names: Option<Vec<String>>,
    #[serde(rename = "RepoTags", alias = "repoTags", default)]
    repo_tags: Option<Vec<String>>,
}

fn parse_images_json(bytes: &[u8]) -> Result<Vec<String>, String> {
    let trimmed = std::str::from_utf8(bytes)
        .map_err(|err| err.to_string())?
        .trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }
    let rows: Vec<ImageRow> = serde_json::from_str(trimmed).map_err(|err| err.to_string())?;
    let mut seen = BTreeSet::new();
    let mut pockets = Vec::new();
    for row in rows {
        for reference in row
            .names
            .into_iter()
            .flatten()
            .chain(row.repo_tags.into_iter().flatten())
        {
            if is_pockets_image_ref(&reference) && seen.insert(reference.clone()) {
                pockets.push(reference);
            }
        }
    }
    Ok(pockets)
}

#[cfg(any(target_os = "macos", test))]
#[derive(Deserialize)]
struct MachineRow {
    #[serde(rename = "Name", alias = "name", default)]
    name: String,
    #[serde(rename = "Running", alias = "running", default)]
    running: bool,
    #[serde(rename = "Default", alias = "default", default)]
    is_default: bool,
}

#[cfg(any(target_os = "macos", test))]
fn parse_machines_json(bytes: &[u8]) -> Result<Vec<PodmanMachineInfo>, String> {
    let trimmed = std::str::from_utf8(bytes)
        .map_err(|err| err.to_string())?
        .trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }
    let rows: Vec<MachineRow> = serde_json::from_str(trimmed).map_err(|err| err.to_string())?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let clean_name = row.name.trim().trim_end_matches('*').to_owned();
            if clean_name.is_empty() {
                None
            } else {
                Some(PodmanMachineInfo {
                    name: clean_name,
                    running: row.running,
                    is_default: row.is_default || row.name.trim().ends_with('*'),
                })
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_pockets_and_other_containers() {
        let json = br#"[
            {"Id": "aaa", "Names": ["/pockets-project-123"], "Labels": {"dev.pockets.managed": "project-123"}},
            {"Id": "bbb", "Names": ["pockets-hello-pocket"], "Labels": null},
            {"Id": "ccc", "Names": ["my-postgres"], "Labels": {"app": "db"}}
        ]"#;
        let (pockets, other) = parse_containers_json(json).unwrap();
        assert_eq!(pockets, vec!["pockets-project-123", "pockets-hello-pocket"]);
        assert_eq!(other, vec!["my-postgres"]);
    }

    #[test]
    fn classifies_pockets_and_other_volumes() {
        let json = br#"[
            {"Name": "pockets-project-abc-data", "Labels": {"dev.pockets.managed": "project-abc"}},
            {"Name": "pockets-hello-pocket-data", "Labels": {}},
            {"Name": "pgdata", "Labels": null}
        ]"#;
        let (pockets, other) = parse_volumes_json(json).unwrap();
        assert_eq!(
            pockets,
            vec!["pockets-project-abc-data", "pockets-hello-pocket-data"]
        );
        assert_eq!(other, vec!["pgdata"]);
    }

    #[test]
    fn classifies_pockets_networks_secrets_and_images() {
        let nets = br#"[
            {"name": "podman", "labels": {}},
            {"name": "pockets-preview", "labels": {}},
            {"name": "pockets-egress-net-abc", "labels": {"dev.pockets.managed": "preview-abc"}}
        ]"#;
        assert_eq!(
            parse_networks_json(nets).unwrap(),
            vec!["pockets-preview", "pockets-egress-net-abc"]
        );

        let secrets = br#"[
            {"Spec": {"Name": "pockets-runtime-installed-abc-1", "Labels": {"dev.pockets.runtime": "installed-abc"}}},
            {"Spec": {"Name": "unrelated-secret", "Labels": {}}}
        ]"#;
        assert_eq!(
            parse_secrets_json(secrets).unwrap(),
            vec!["pockets-runtime-installed-abc-1"]
        );

        let images = br#"[
            {"Names": ["localhost/pockets-managed-abc:build"], "RepoTags": ["localhost/pockets-managed-abc:build"]},
            {"Names": ["docker.io/library/postgres:16"]}
        ]"#;
        assert_eq!(
            parse_images_json(images).unwrap(),
            vec!["localhost/pockets-managed-abc:build"]
        );
    }

    #[test]
    fn parses_podman_machines_json() {
        let json = br#"[
            {"Name": "podman-machine-default*", "Running": true, "Default": true},
            {"Name": "custom-vm", "Running": false, "Default": false}
        ]"#;
        let machines = parse_machines_json(json).unwrap();
        assert_eq!(
            machines,
            vec![
                PodmanMachineInfo {
                    name: "podman-machine-default".to_owned(),
                    running: true,
                    is_default: true,
                },
                PodmanMachineInfo {
                    name: "custom-vm".to_owned(),
                    running: false,
                    is_default: false,
                },
            ]
        );
    }

    #[test]
    fn handles_empty_and_null_podman_json() {
        assert_eq!(parse_containers_json(b"null").unwrap(), (vec![], vec![]));
        assert_eq!(parse_volumes_json(b"").unwrap(), (vec![], vec![]));
        assert_eq!(parse_networks_json(b"[]").unwrap(), Vec::<String>::new());
        assert_eq!(parse_secrets_json(b"null").unwrap(), Vec::<String>::new());
        assert_eq!(parse_images_json(b"[]").unwrap(), Vec::<String>::new());
        assert_eq!(parse_machines_json(b"null").unwrap(), vec![]);
    }

    #[test]
    fn detects_homebrew_prefix_from_podman_cellar_path() {
        assert_eq!(
            homebrew_prefix_from_podman_path(Path::new(
                "/opt/homebrew/Cellar/podman/5.4.0/bin/podman"
            )),
            Some(PathBuf::from("/opt/homebrew"))
        );
        assert_eq!(
            homebrew_prefix_from_podman_path(Path::new("/opt/podman/bin/podman")),
            None
        );
    }

    #[tokio::test]
    async fn remove_pockets_data_deletes_data_dir_and_warns_when_engine_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("pockets-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("metadata.json"), b"{}").unwrap();

        let inv = PodmanInventory {
            engine_reachable: false,
            ..Default::default()
        };
        let report = remove_pockets_data(&data_dir, Some(Path::new("/usr/bin/podman")), Some(&inv), false).await;
        assert!(report.data_dir_removed);
        assert!(!data_dir.exists());
        assert_eq!(report.warnings.len(), 1);
    }

    #[cfg(unix)]
    fn make_recording_script(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join(format!("{name}.log"));
        let bin = dir.join(name);
        let body = format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 0\n", log.display());
        std::fs::write(&bin, body).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        (bin, log)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remove_pockets_data_only_deletes_pockets_resources() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_podman, log) = make_recording_script(dir.path(), "podman");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let inv = PodmanInventory {
            engine_reachable: true,
            pockets_containers: vec!["pockets-project-1".to_owned()],
            other_containers: vec!["user-postgres".to_owned()],
            pockets_volumes: vec!["pockets-project-1-data".to_owned()],
            other_volumes: vec!["user-pgdata".to_owned()],
            pockets_networks: vec!["pockets-preview".to_owned()],
            pockets_secrets: vec!["pockets-runtime-installed-1-abc".to_owned()],
            pockets_images: vec!["localhost/pockets-managed-1:build".to_owned()],
            machines: vec![],
        };

        let report = remove_pockets_data(&data_dir, Some(&fake_podman), Some(&inv), false).await;
        assert_eq!(report.containers_removed, 1);
        assert_eq!(report.volumes_removed, 1);
        assert_eq!(report.networks_removed, 1);
        assert_eq!(report.secrets_removed, 1);
        assert_eq!(report.images_removed, 1);
        assert!(report.data_dir_removed);
        assert!(report.warnings.is_empty());

        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("rm -f pockets-project-1"));
        assert!(calls.contains("volume rm -f pockets-project-1-data"));
        assert!(calls.contains("network rm pockets-preview"));
        assert!(calls.contains("secret rm pockets-runtime-installed-1-abc"));
        assert!(calls.contains("rmi -f localhost/pockets-managed-1:build"));
        assert!(!calls.contains("user-postgres"));
        assert!(!calls.contains("user-pgdata"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_and_remove_podman_machines_stops_running_then_removes() {
        let dir = tempfile::tempdir().unwrap();
        let (fake_podman, log) = make_recording_script(dir.path(), "podman");
        let machines = vec![
            PodmanMachineInfo {
                name: "podman-machine-default".to_owned(),
                running: true,
                is_default: true,
            },
            PodmanMachineInfo {
                name: "stopped-vm".to_owned(),
                running: false,
                is_default: false,
            },
        ];

        let removed = stop_and_remove_podman_machines(&fake_podman, &machines)
            .await
            .unwrap();
        assert_eq!(removed, 2);

        let lines: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            lines,
            vec![
                "machine stop podman-machine-default",
                "machine rm -f podman-machine-default",
                "machine rm -f stopped-vm",
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn uninstall_pockets_package_handles_binary_and_development() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("pockets");
        std::fs::write(&fake_bin, b"binary").unwrap();

        let outcome = uninstall_pockets_package(&InstallMethod::Binary {
            exe_path: fake_bin.clone(),
        })
        .await
        .unwrap();
        assert_eq!(
            outcome,
            PocketsUninstallOutcome::RemovedBinary {
                exe_path: fake_bin.clone()
            }
        );
        assert!(!fake_bin.exists());

        let dev_outcome = uninstall_pockets_package(&InstallMethod::Development)
            .await
            .unwrap();
        assert_eq!(dev_outcome, PocketsUninstallOutcome::SkippedDevelopment);
    }
}
