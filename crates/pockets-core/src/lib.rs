//! Cross-platform core engine for Pockets: Podman orchestration, project store,
//! live development preview, Tailscale Serve integration, runtime secrets,
//! backups, version history, and resource monitoring.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

pub mod app_update;
pub mod builder;
pub mod creation;
pub mod development;
pub mod development_commands;
pub mod favicon;
pub mod fs_browser;
pub mod git;
pub mod github_install;
pub mod health;
pub mod hub;
pub mod local_sites;
#[cfg(test)]
mod managed_live;
pub mod manifest;
pub mod pocket;
pub mod preview;
pub mod preview_egress;
pub mod preview_proxy;
pub mod preview_source;
pub mod project_lifecycle;
pub mod project_preview;
pub mod project_updates;
pub mod projects;
pub mod resource_monitor;
pub mod revision_commands;
pub mod revisions;
pub mod runtime;
pub mod runtime_env;
pub mod runtime_settings;
pub mod scaffold;
pub mod source;
pub mod tailscale;
pub mod tailscale_portal;
pub mod upstream;
pub mod versions;

/// Semantic version baked into this Pockets binary (set from the release tag
/// via `POCKETS_VERSION`, falling back to `CARGO_PKG_VERSION` in local builds).
pub const APP_VERSION: &str = env!("POCKETS_APP_VERSION");

/// Shared lifecycle lock serializing mutating Podman operations across Pockets.
#[derive(Clone, Default)]
pub struct Controller {
    lifecycle: Arc<tokio::sync::RwLock<()>>,
}

impl Controller {
    pub(crate) async fn lifecycle_guard(&self) -> tokio::sync::RwLockWriteGuard<'_, ()> {
        self.lifecycle.write().await
    }
}

/// Process-wide Pockets runtime context shared by the CLI and HTTP daemon.
#[derive(Clone)]
pub struct AppContext {
    data_dir: PathBuf,
    pub controller: Controller,
    pub runtime_settings: Arc<runtime_settings::RuntimeSettings>,
    pub build_jobs: Arc<creation::BuildJobs>,
    pub previews: preview::Previews,
    pub project_previews: project_preview::ProjectPreviews,
    pub tailscale_portal: Arc<tailscale_portal::TailscalePortal>,
    pub tailscale_preview_shares: tailscale::PreviewShares,
}

impl AppContext {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            controller: Controller::default(),
            runtime_settings: Arc::new(runtime_settings::RuntimeSettings::default()),
            build_jobs: Arc::new(creation::BuildJobs::default()),
            previews: preview::Previews::default(),
            project_previews: project_preview::ProjectPreviews::default(),
            tailscale_portal: Arc::new(tailscale_portal::TailscalePortal::default()),
            tailscale_preview_shares: tailscale::PreviewShares::default(),
        }
    }

    pub fn from_default_data_dir() -> Result<Self, String> {
        Ok(Self::new(fs_browser::default_data_dir()?))
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn store(&self) -> projects::Store {
        projects::Store::new(self.data_dir.join("projects"))
    }

    /// Runs non-fatal startup maintenance: sweeps orphaned preview containers
    /// from earlier runs and restores the Tailscale port 443 launcher if any
    /// installed Pocket is opted in.
    pub async fn startup_housekeeping(&self) {
        for problem in preview::sweep_at_launch(self).await {
            eprintln!("preview orphan sweep: {problem}");
        }
        let store = self.store();
        tailscale::startup_sync_portal(&store, &self.tailscale_portal).await;
    }
}

/// Command-facing Pocket status: Podman-reconciled status flattened with HTTP
/// readiness and optional Tailscale sharing status.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PocketView {
    #[serde(flatten)]
    pub status: pocket::PocketStatus,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tailscale: Option<tailscale::TailscaleShareView>,
}

impl PocketView {
    fn for_site(
        mut self,
        store: &projects::Store,
        site: local_sites::Site<'_>,
    ) -> Result<Self, String> {
        self.status.url = if self.ready {
            Some(
                site.running_address(store, self.status.url.as_deref())?
                    .url(),
            )
        } else {
            None
        };
        Ok(self)
    }
}

/// Locates Podman or returns an OS-specific install instruction.
pub fn require_podman() -> Result<PathBuf, String> {
    runtime::find_podman().ok_or_else(runtime::missing_podman_message)
}

/// Reconciles the bundled Pocket status from Podman and probes `/health`.
pub(crate) async fn read_status(bin: &Path) -> Result<PocketView, String> {
    let status = pocket::inspect(bin).await?;
    let ready = match &status.url {
        Some(url) if status.state == pocket::PocketState::Running => {
            let manifest = manifest::bundled_manifest()?;
            health::is_ready(url, &manifest.web.healthcheck).await?
        }
        _ => false,
    };
    Ok(PocketView {
        status,
        ready,
        tailscale: None,
    })
}

pub fn get_manifest() -> Result<manifest::Manifest, String> {
    let manifest = manifest::bundled_manifest()?;
    manifest.validate_host()?;
    Ok(manifest)
}

pub async fn detect_runtime() -> runtime::DetectionReport {
    runtime::detect_runtime().await
}

pub async fn bootstrap_runtime() -> runtime::DetectionReport {
    runtime::bootstrap_runtime().await
}

pub async fn detect_tailscale(ctx: &AppContext) -> Result<tailscale::TailscaleReport, String> {
    Ok(tailscale::detect_tailscale(&ctx.tailscale_portal).await)
}

pub async fn pocket_status(ctx: &AppContext) -> Result<PocketView, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let bin = require_podman()?;
    let store = ctx.store();
    read_status(&bin)
        .await?
        .for_site(&store, local_sites::Site::Bundled)
}

pub async fn start_pocket(ctx: &AppContext) -> Result<PocketView, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let bin = require_podman()?;
    let manifest = manifest::bundled_manifest()?;
    let store = ctx.store();
    let status = pocket::start(&bin, &store).await?;
    let url = match &status.url {
        Some(url) if status.state == pocket::PocketState::Running => url.clone(),
        _ => return Err(format!("The Pocket did not start. {}", status.message)),
    };
    health::wait_ready(&url, &manifest.web.healthcheck).await?;
    read_status(&bin)
        .await?
        .for_site(&store, local_sites::Site::Bundled)
}

pub async fn stop_pocket(ctx: &AppContext) -> Result<PocketView, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let bin = require_podman()?;
    let status = pocket::stop(&bin).await?;
    Ok(PocketView {
        status,
        ready: false,
        tailscale: None,
    })
}

pub async fn open_pocket(
    ctx: &AppContext,
    open_host_browser: bool,
    prefer_remote: bool,
) -> Result<String, String> {
    let _serializing = ctx.controller.lifecycle_guard().await;
    let bin = require_podman()?;
    let view = read_status(&bin).await?;
    if !view.ready {
        return Err(
            "The Pocket is not reachable yet. Start it and wait until it reports ready.".to_owned(),
        );
    }
    let store = ctx.store();
    let address = local_sites::Site::Bundled.running_address(&store, view.status.url.as_deref())?;
    let url = if prefer_remote {
        tailscale::enable_port_serve(address.port).await.map_err(|err| {
            format!(
                "Could not open this Pocket remotely over Tailscale ({err}). \
                 Local address on the host is {} (forward port {} over SSH if not using Tailscale).",
                address.url(),
                address.port
            )
        })?
    } else {
        address.url()
    };
    if open_host_browser {
        open_in_browser(&url).await?;
    }
    Ok(url)
}

/// Opens a validated local URL in the host OS's default browser without a shell.
pub async fn open_in_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        runtime::run(Path::new("/usr/bin/open"), &[url])
            .await
            .map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        runtime::run(Path::new("xdg-open"), &[url])
            .await
            .map(|_| ())
    }
    #[cfg(windows)]
    {
        runtime::run(Path::new("cmd"), &["/C", "start", "", url])
            .await
            .map(|_| ())
    }
}

/// Opens a local directory in the host OS's file manager without a shell.
pub async fn open_path_in_os(path: &Path) -> Result<(), String> {
    let path_str = path
        .to_str()
        .ok_or("The folder path must be valid UTF-8.")?;
    #[cfg(target_os = "macos")]
    {
        runtime::run(Path::new("/usr/bin/open"), &[path_str])
            .await
            .map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        runtime::run(Path::new("xdg-open"), &[path_str])
            .await
            .map(|_| ())
    }
    #[cfg(windows)]
    {
        runtime::run(Path::new("explorer"), &[path_str])
            .await
            .map(|_| ())
    }
}
