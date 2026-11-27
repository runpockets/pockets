//! Native Tailscale discovery, Serve route management, and per-Pocket opt-in
//! persistence for viewing installed Pockets from other devices on the user's
//! tailnet.
//!
//! Security boundaries (matching `docs/architecture.md`):
//! - Containers stay strictly bound to `127.0.0.1:<port>`; Tailscale Serve
//!   proxies privately over the tailnet (`tailscale serve --bg --https=<port>
//!   http://127.0.0.1:<port>`). `tailscale funnel` (public internet) and
//!   `tailscale serve reset` (which would wipe unrelated routes) are never used.
//! - All CLI calls use fixed argument arrays via [`crate::runtime::run`] with
//!   no shell and a bounded timeout.
//! - Before enabling or removing a port, `tailscale serve status --json` is
//!   inspected so Pockets never overwrites or removes an unrelated Serve route.
//! - Per-Pocket opt-in state lives in `tailscale-shares.json` beside
//!   `local-sites.json`, using the same cross-process file lock and atomic
//!   temp-file rename pattern.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::local_sites;
use crate::projects::Store;
use crate::runtime;
use crate::tailscale_portal::{PortalEntry, PortalSnapshot, TailscalePortal, PORTAL_LOCAL_PORT};

/// Tailscale HTTPS port used for the launcher & named `/<slug>` shortcuts.
const PORTAL_HTTPS_PORT: u16 = 443;

const TAILSCALE_EXE: &str = "tailscale";

/// Well-known Tailscale CLI locations checked in addition to `PATH`.
/// On macOS, the standalone and App Store apps bundle the CLI inside the app.
const KNOWN_LOCATIONS: [&str; 3] = [
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
];

const PAUSED_MESSAGE: &str = "Paused while stopped — will share automatically when started.";

/// Availability state of the local Tailscale client for private HTTPS serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TailscaleState {
    /// No `tailscale` CLI executable was found.
    Missing,
    /// Installed, but not running, not signed in, or MagicDNS is unavailable.
    Unavailable,
    /// Running with MagicDNS, but HTTPS certificates are disabled on the tailnet.
    HttpsDisabled,
    /// Running with MagicDNS and HTTPS certificates ready for `tailscale serve`.
    Ready,
}

/// Serializable result of probing Tailscale without mutating any configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TailscaleReport {
    pub state: TailscaleState,
    pub message: String,
    pub dns_name: Option<String>,
    pub version: Option<String>,
    /// Launcher URL (`https://<node>.<tailnet>.ts.net`) when the port 443
    /// portal is actively running and served.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal_url: Option<String>,
}

impl TailscaleReport {
    fn new(state: TailscaleState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
            dns_name: None,
            version: None,
            portal_url: None,
        }
    }
}

/// Per-Pocket Tailscale sharing state returned alongside [`crate::PocketView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TailscaleShareView {
    /// Whether the user has opted this Pocket into Tailscale access.
    pub enabled: bool,
    /// Whether Tailscale Serve is actively proxying this Pocket right now.
    pub active: bool,
    /// Primary tailnet HTTPS URL (`https://<node>.<tailnet>.ts.net/<slug>` when
    /// the port 443 portal is active, otherwise `https://<node>.<tailnet>.ts.net:<port>`),
    /// present only when `active` is `true`.
    pub url: Option<String>,
    /// Direct port-addressed tailnet HTTPS URL (`https://<node>.<tailnet>.ts.net:<port>`),
    /// which remains reachable even if the Pockets daemon is stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_url: Option<String>,
    /// Launcher URL (`https://<node>.<tailnet>.ts.net`) when port 443 is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal_url: Option<String>,
    /// Honest status note when enabled but paused, disconnected, or conflicting.
    pub message: Option<String>,
}

impl TailscaleShareView {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            active: false,
            url: None,
            direct_url: None,
            portal_url: None,
            message: None,
        }
    }

    pub(crate) fn paused() -> Self {
        Self {
            enabled: true,
            active: false,
            url: None,
            direct_url: None,
            portal_url: None,
            message: Some(PAUSED_MESSAGE.to_owned()),
        }
    }

    fn active(
        dns_name: &str,
        slug: &str,
        port: u16,
        portal_active: bool,
        note: Option<String>,
    ) -> Self {
        let direct = share_url(dns_name, port);
        if portal_active {
            Self {
                enabled: true,
                active: true,
                url: Some(named_url(dns_name, slug)),
                direct_url: Some(direct),
                portal_url: Some(portal_root_url(dns_name)),
                message: note,
            }
        } else {
            Self {
                enabled: true,
                active: true,
                url: Some(direct.clone()),
                direct_url: Some(direct),
                portal_url: None,
                message: note,
            }
        }
    }

    fn inactive(message: impl Into<String>) -> Self {
        Self {
            enabled: true,
            active: false,
            url: None,
            direct_url: None,
            portal_url: None,
            message: Some(message.into()),
        }
    }
}

/// Persistent per-Pocket opt-in entry saved in `tailscale-shares.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShareRecord {
    enabled: bool,
    /// Last loopback port published via `tailscale serve`, used to tear down
    /// the exact route cleanly on Stop, Disable, or Delete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
}

/// Subset of `tailscale status --json` needed for detection.
#[derive(Debug, Default, Deserialize)]
struct StatusJson {
    #[serde(rename = "Version", default)]
    version: Option<String>,
    #[serde(rename = "BackendState", default)]
    backend_state: Option<String>,
    #[serde(rename = "Self", default)]
    self_node: Option<SelfNodeJson>,
    #[serde(rename = "CertDomains", default)]
    cert_domains: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct SelfNodeJson {
    #[serde(rename = "DNSName", default)]
    dns_name: Option<String>,
    #[serde(rename = "Capabilities", default)]
    capabilities: Option<Vec<String>>,
    #[serde(rename = "CapMap", default)]
    cap_map: Option<BTreeMap<String, serde_json::Value>>,
}

/// Subset of `tailscale serve status --json` needed to check route ownership.
#[derive(Debug, Default, Deserialize)]
struct ServeStatusJson {
    #[serde(rename = "TCP", default)]
    tcp: BTreeMap<String, TcpPortHandler>,
    #[serde(rename = "Web", default)]
    web: BTreeMap<String, WebServerConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct TcpPortHandler {
    #[serde(rename = "HTTPS", default)]
    https: bool,
    #[serde(rename = "HTTP", default)]
    http: bool,
    #[serde(rename = "TCPForward", default)]
    tcp_forward: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WebServerConfig {
    #[serde(rename = "Handlers", default)]
    handlers: BTreeMap<String, HttpHandler>,
}

#[derive(Debug, Default, Deserialize)]
struct HttpHandler {
    #[serde(rename = "Proxy", default)]
    proxy: Option<String>,
    #[serde(rename = "Path", default)]
    path: Option<String>,
    #[serde(rename = "Text", default)]
    text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteOwnership {
    Absent,
    Owned,
    Foreign,
}

/// Locate the `tailscale` executable in `PATH` plus [`KNOWN_LOCATIONS`].
pub fn find_tailscale() -> Option<PathBuf> {
    first_executable(&candidate_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ))
}

/// Probe Tailscale availability without modifying any Tailscale state.
pub(crate) async fn detect_tailscale(portal: &TailscalePortal) -> TailscaleReport {
    let Some(bin) = find_tailscale() else {
        return TailscaleReport::new(
            TailscaleState::Missing,
            "Tailscale was not found on this computer. Install Tailscale from https://tailscale.com to view Pockets from other devices.",
        );
    };
    let mut report = detect_with_bin(&bin).await;
    if report.state == TailscaleState::Ready && portal.is_running() {
        if let Some(dns_name) = report.dns_name.as_deref() {
            if let Ok(serve) = inspect_serve_with_bin(&bin).await {
                if route_ownership(&serve, dns_name, PORTAL_HTTPS_PORT, PORTAL_LOCAL_PORT)
                    == RouteOwnership::Owned
                {
                    report.portal_url = Some(portal_root_url(dns_name));
                }
            }
        }
    }
    report
}

async fn detect_with_bin(bin: &Path) -> TailscaleReport {
    let output = match runtime::run(bin, &["status", "--json"]).await {
        Ok(output) => output,
        Err(err) => {
            return TailscaleReport::new(
                TailscaleState::Unavailable,
                format!("Tailscale could not be queried. Make sure the Tailscale app is open and running. {err}"),
            );
        }
    };
    parse_status_report(&output.stdout)
}

fn parse_status_report(stdout: &[u8]) -> TailscaleReport {
    let status: StatusJson = match serde_json::from_slice(stdout) {
        Ok(parsed) => parsed,
        Err(err) => {
            return TailscaleReport::new(
                TailscaleState::Unavailable,
                format!("Could not read Tailscale status: {err}"),
            );
        }
    };
    let version = status.version.as_deref().and_then(normalize_version);
    let backend = status.backend_state.as_deref().unwrap_or("");
    if backend != "Running" {
        let message = match backend {
            "NeedsLogin" => "Tailscale is not signed in. Open Tailscale and sign in to view Pockets from other devices.".to_owned(),
            "Stopped" => "Tailscale is currently disconnected. Connect Tailscale and try again.".to_owned(),
            "" => "Tailscale is not running. Open the Tailscale app and connect to your tailnet.".to_owned(),
            other => format!("Tailscale is not ready ({other}). Open Tailscale and connect to your tailnet."),
        };
        return TailscaleReport {
            state: TailscaleState::Unavailable,
            message,
            dns_name: None,
            version,
            portal_url: None,
        };
    }

    let dns_name = status
        .self_node
        .as_ref()
        .and_then(|node| node.dns_name.as_deref())
        .and_then(validate_dns_name);
    let Some(dns_name) = dns_name else {
        return TailscaleReport {
            state: TailscaleState::Unavailable,
            message: "Tailscale MagicDNS is not enabled for this machine. Enable MagicDNS in your Tailscale admin console to use Tailscale URLs.".to_owned(),
            dns_name: None,
            version,
            portal_url: None,
        };
    };

    let cert_domains_ready = status.cert_domains.as_ref().is_some_and(|domains| {
        domains
            .iter()
            .any(|d| d.trim_end_matches('.').eq_ignore_ascii_case(&dns_name))
    });
    let https_cap = status.self_node.as_ref().is_some_and(|node| {
        node.capabilities
            .as_ref()
            .is_some_and(|caps| caps.iter().any(|cap| cap == "https"))
            || node
                .cap_map
                .as_ref()
                .is_some_and(|map| map.contains_key("https"))
    });
    if !cert_domains_ready && !https_cap {
        return TailscaleReport {
            state: TailscaleState::HttpsDisabled,
            message: "Tailscale HTTPS certificates are disabled for your tailnet. Enable HTTPS Certificates under DNS in the Tailscale admin console to share Pockets privately over HTTPS.".to_owned(),
            dns_name: Some(dns_name),
            version,
            portal_url: None,
        };
    }

    TailscaleReport {
        state: TailscaleState::Ready,
        message: match &version {
            Some(v) => format!("Tailscale {v} is ready on {dns_name}."),
            None => format!("Tailscale is ready on {dns_name}."),
        },
        dns_name: Some(dns_name),
        version,
        portal_url: None,
    }
}

/// Reconciles one Pocket's Tailscale sharing status. Fast-paths when the Pocket
/// is not opted in or stopped (zero Tailscale subprocesses invoked).
pub(crate) async fn reconcile_project_share(
    store: &Store,
    portal: &TailscalePortal,
    project_id: &str,
    slug: &str,
    running_port: Option<u16>,
) -> Result<TailscaleShareView, String> {
    let record = load_share(store, project_id)?;
    let Some(_record) = record.filter(|r| r.enabled) else {
        return Ok(TailscaleShareView::disabled());
    };
    let Some(port) = running_port else {
        return Ok(TailscaleShareView::paused());
    };
    let Some(bin) = find_tailscale() else {
        return Ok(TailscaleShareView::inactive(
            "Tailscale was not found on this computer.",
        ));
    };
    Ok(reconcile_running_with_bin(store, Some(portal), &bin, project_id, slug, port).await)
}

async fn reconcile_running_with_bin(
    store: &Store,
    portal: Option<&TailscalePortal>,
    bin: &Path,
    project_id: &str,
    slug: &str,
    port: u16,
) -> TailscaleShareView {
    let report = detect_with_bin(bin).await;
    if report.state != TailscaleState::Ready {
        return TailscaleShareView::inactive(report.message);
    }
    let Some(dns_name) = report.dns_name.as_deref() else {
        return TailscaleShareView::inactive(report.message);
    };
    match inspect_serve_with_bin(bin).await {
        Ok(serve) => match route_ownership(&serve, dns_name, port, port) {
            RouteOwnership::Owned => {
                let _ = save_share(
                    store,
                    project_id,
                    Some(ShareRecord {
                        enabled: true,
                        port: Some(port),
                    }),
                );
                let sync = sync_portal_with_bin(store, portal, Some(bin), Some(dns_name)).await;
                TailscaleShareView::active(dns_name, slug, port, sync.active, sync.note)
            }
            RouteOwnership::Absent => TailscaleShareView::inactive(
                "Tailscale route is not active. Click Reconnect to resume sharing on your tailnet.",
            ),
            RouteOwnership::Foreign => TailscaleShareView::inactive(format!(
                "Tailscale Serve port {port} is in use by another service."
            )),
        },
        Err(err) => TailscaleShareView::inactive(format!("Could not check Tailscale Serve: {err}")),
    }
}

/// Turns Tailscale sharing on or off for one installed Pocket.
pub(crate) async fn set_enabled(
    store: &Store,
    portal: &TailscalePortal,
    project_id: &str,
    slug: &str,
    enabled: bool,
    running_port: Option<u16>,
) -> Result<TailscaleShareView, String> {
    let bin = find_tailscale();
    set_enabled_with_bin(
        store,
        Some(portal),
        bin.as_deref(),
        project_id,
        slug,
        enabled,
        running_port,
    )
    .await
}

async fn set_enabled_with_bin(
    store: &Store,
    portal: Option<&TailscalePortal>,
    bin: Option<&Path>,
    project_id: &str,
    slug: &str,
    enabled: bool,
    running_port: Option<u16>,
) -> Result<TailscaleShareView, String> {
    if enabled {
        let Some(bin) = bin else {
            return Err("Tailscale was not found on this computer. Install Tailscale from https://tailscale.com to share Pockets on your tailnet.".to_owned());
        };
        let report = detect_with_bin(bin).await;
        if report.state != TailscaleState::Ready {
            return Err(report.message);
        }
        let dns_name = report.dns_name.as_deref().ok_or_else(|| {
            "Tailscale did not report a MagicDNS name for this machine.".to_owned()
        })?;
        if let Some(port) = running_port {
            enable_serve_route(bin, dns_name, port, port).await?;
            save_share(
                store,
                project_id,
                Some(ShareRecord {
                    enabled: true,
                    port: Some(port),
                }),
            )?;
            let sync = sync_portal_with_bin(store, portal, Some(bin), Some(dns_name)).await;
            Ok(TailscaleShareView::active(
                dns_name,
                slug,
                port,
                sync.active,
                sync.note,
            ))
        } else {
            save_share(
                store,
                project_id,
                Some(ShareRecord {
                    enabled: true,
                    port: None,
                }),
            )?;
            let _ = sync_portal_with_bin(store, portal, Some(bin), Some(dns_name)).await;
            Ok(TailscaleShareView::paused())
        }
    } else {
        let existing = load_share(store, project_id)?;
        let port_to_remove = existing.and_then(|r| r.port).or(running_port);
        let mut ready_dns: Option<String> = None;
        if let Some(bin) = bin {
            let report = detect_with_bin(bin).await;
            if report.state == TailscaleState::Ready {
                ready_dns = report.dns_name;
                if let (Some(dns_name), Some(port)) = (ready_dns.as_deref(), port_to_remove) {
                    disable_serve_route(bin, dns_name, port, port).await?;
                }
            }
        }
        save_share(store, project_id, None)?;
        let _ = sync_portal_with_bin(store, portal, bin, ready_dns.as_deref()).await;
        Ok(TailscaleShareView::disabled())
    }
}

/// Restores an opted-in Pocket's Tailscale Serve route after the Pocket starts
/// and passes readiness. Never fails the local Pocket start if Tailscale is
/// disconnected or unavailable; instead returns an honest inactive view.
pub(crate) async fn restore_on_start(
    store: &Store,
    portal: &TailscalePortal,
    project_id: &str,
    slug: &str,
    running_port: u16,
) -> TailscaleShareView {
    let bin = find_tailscale();
    restore_on_start_with_bin(
        store,
        Some(portal),
        bin.as_deref(),
        project_id,
        slug,
        running_port,
    )
    .await
}

async fn restore_on_start_with_bin(
    store: &Store,
    portal: Option<&TailscalePortal>,
    bin: Option<&Path>,
    project_id: &str,
    slug: &str,
    running_port: u16,
) -> TailscaleShareView {
    let record = match load_share(store, project_id) {
        Ok(Some(record)) if record.enabled => record,
        Ok(_) => return TailscaleShareView::disabled(),
        Err(err) => {
            return TailscaleShareView::inactive(format!(
                "Could not read Tailscale sharing preference: {err}"
            ))
        }
    };
    let Some(bin) = bin else {
        return TailscaleShareView::inactive(
            "Tailscale was not found on this computer; sharing could not be restored.",
        );
    };
    let report = detect_with_bin(bin).await;
    if report.state != TailscaleState::Ready {
        return TailscaleShareView::inactive(format!(
            "Could not restore Tailscale sharing: {}",
            report.message
        ));
    }
    let Some(dns_name) = report.dns_name.as_deref() else {
        return TailscaleShareView::inactive(
            "Could not restore Tailscale sharing: missing MagicDNS name.",
        );
    };
    // If the Pocket somehow moved to a new port, clean up any old Pockets-owned route first.
    if let Some(old_port) = record.port.filter(|p| *p != running_port) {
        let _ = disable_serve_route(bin, dns_name, old_port, old_port).await;
    }
    if let Err(err) = enable_serve_route(bin, dns_name, running_port, running_port).await {
        return TailscaleShareView::inactive(format!("Could not restore Tailscale sharing: {err}"));
    }
    let _ = save_share(
        store,
        project_id,
        Some(ShareRecord {
            enabled: true,
            port: Some(running_port),
        }),
    );
    let sync = sync_portal_with_bin(store, portal, Some(bin), Some(dns_name)).await;
    TailscaleShareView::active(dns_name, slug, running_port, sync.active, sync.note)
}

/// Pauses an opted-in Pocket's Tailscale Serve route when the Pocket stops,
/// while preserving the user's opt-in so the next Start restores it.
pub(crate) async fn pause_on_stop(
    store: &Store,
    portal: &TailscalePortal,
    project_id: &str,
) -> TailscaleShareView {
    let bin = find_tailscale();
    pause_on_stop_with_bin(store, Some(portal), bin.as_deref(), project_id).await
}

async fn pause_on_stop_with_bin(
    store: &Store,
    portal: Option<&TailscalePortal>,
    bin: Option<&Path>,
    project_id: &str,
) -> TailscaleShareView {
    let record = match load_share(store, project_id) {
        Ok(Some(record)) => record,
        Ok(None) => return TailscaleShareView::disabled(),
        Err(_) => return TailscaleShareView::disabled(),
    };
    let mut ready_dns: Option<String> = None;
    if let Some(bin) = bin {
        let report = detect_with_bin(bin).await;
        if report.state == TailscaleState::Ready {
            ready_dns = report.dns_name;
            if let (Some(dns_name), Some(port)) = (ready_dns.as_deref(), record.port) {
                if let Err(err) = disable_serve_route(bin, dns_name, port, port).await {
                    eprintln!("tailscale pause_on_stop ({project_id}, port {port}): {err}");
                }
            }
        }
    }
    if record.enabled {
        let _ = save_share(
            store,
            project_id,
            Some(ShareRecord {
                enabled: true,
                port: None,
            }),
        );
        let _ = sync_portal_with_bin(store, portal, bin, ready_dns.as_deref()).await;
        TailscaleShareView::paused()
    } else {
        let _ = save_share(store, project_id, None);
        let _ = sync_portal_with_bin(store, portal, bin, ready_dns.as_deref()).await;
        TailscaleShareView::disabled()
    }
}

/// Best-effort cleanup when a Pocket is permanently deleted: removes any owned
/// Tailscale Serve route, deletes its registry entry, and updates/stops the portal.
pub(crate) async fn cleanup_on_delete(store: &Store, portal: &TailscalePortal, project_id: &str) {
    let bin = find_tailscale();
    cleanup_on_delete_with_bin(store, Some(portal), bin.as_deref(), project_id).await;
}

async fn cleanup_on_delete_with_bin(
    store: &Store,
    portal: Option<&TailscalePortal>,
    bin: Option<&Path>,
    project_id: &str,
) {
    let mut ready_dns: Option<String> = None;
    if let Ok(Some(record)) = load_share(store, project_id) {
        if let Some(bin) = bin {
            let report = detect_with_bin(bin).await;
            if report.state == TailscaleState::Ready {
                ready_dns = report.dns_name;
                if let (Some(dns_name), Some(port)) = (ready_dns.as_deref(), record.port) {
                    if let Err(err) = disable_serve_route(bin, dns_name, port, port).await {
                        eprintln!("tailscale cleanup_on_delete ({project_id}, port {port}): {err}");
                    }
                }
            }
        }
    }
    if let Err(err) = save_share(store, project_id, None) {
        eprintln!("tailscale cleanup_on_delete registry ({project_id}): {err}");
    }
    let _ = sync_portal_with_bin(store, portal, bin, ready_dns.as_deref()).await;
}

/// Starts the port 443 launcher portal on app launch when at least one Pocket
/// has Tailscale sharing enabled. No-ops with zero subprocesses otherwise.
pub(crate) async fn startup_sync_portal(store: &Store, portal: &TailscalePortal) {
    let Ok(shares) = load_all_shares(store) else {
        return;
    };
    if !shares.values().any(|record| record.enabled) {
        return;
    }
    let bin = find_tailscale();
    let _ = sync_portal_with_bin(store, Some(portal), bin.as_deref(), None).await;
}

struct PortalSync {
    active: bool,
    note: Option<String>,
}

async fn sync_portal_with_bin(
    store: &Store,
    portal: Option<&TailscalePortal>,
    bin: Option<&Path>,
    known_dns: Option<&str>,
) -> PortalSync {
    let shares = match load_all_shares(store) {
        Ok(map) => map,
        Err(_) => {
            return PortalSync {
                active: false,
                note: None,
            }
        }
    };
    let enabled_rows: Vec<(String, ShareRecord)> = shares
        .into_iter()
        .filter(|(_, record)| record.enabled)
        .collect();

    if enabled_rows.is_empty() {
        if let Some(portal) = portal {
            portal.stop().await;
        }
        if let Some(bin) = bin {
            let resolved_dns = match known_dns {
                Some(dns) => Some(dns.to_owned()),
                None => {
                    let report = detect_with_bin(bin).await;
                    if report.state == TailscaleState::Ready {
                        report.dns_name
                    } else {
                        None
                    }
                }
            };
            if let Some(dns) = resolved_dns.as_deref() {
                let _ = disable_serve_route(bin, dns, PORTAL_HTTPS_PORT, PORTAL_LOCAL_PORT).await;
            }
        }
        return PortalSync {
            active: false,
            note: None,
        };
    }

    let resolved_dns = match known_dns {
        Some(dns) => Some(dns.to_owned()),
        None => match bin {
            Some(bin) => {
                let report = detect_with_bin(bin).await;
                if report.state == TailscaleState::Ready {
                    report.dns_name
                } else {
                    None
                }
            }
            None => None,
        },
    };
    let Some(dns) = resolved_dns.as_deref() else {
        return PortalSync {
            active: false,
            note: None,
        };
    };

    let mut entries = Vec::new();
    for (project_id, record) in enabled_rows {
        if let Ok(project) = store.load(&project_id) {
            let manifest = project.installed_manifest();
            entries.push(PortalEntry {
                id: project.id.clone(),
                slug: local_sites::slug_for_project(store, &project),
                name: manifest.metadata.name.clone(),
                description: manifest.metadata.description.clone(),
                port: record.port,
            });
        }
    }
    entries.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.slug.cmp(&b.slug))
    });

    if let Some(portal) = portal {
        let snapshot = PortalSnapshot {
            dns_name: dns.to_owned(),
            entries,
        };
        if let Err(err) = portal.ensure_running(snapshot).await {
            return PortalSync {
                active: false,
                note: Some(format!(
                    "Named shortcut portal unavailable ({err}); using direct port URL."
                )),
            };
        }
    }

    let Some(bin) = bin else {
        return PortalSync {
            active: false,
            note: None,
        };
    };

    match inspect_serve_with_bin(bin).await {
        Ok(serve) => match route_ownership(&serve, dns, PORTAL_HTTPS_PORT, PORTAL_LOCAL_PORT) {
            RouteOwnership::Owned => PortalSync {
                active: true,
                note: None,
            },
            RouteOwnership::Foreign => PortalSync {
                active: false,
                note: Some(
                    "Port 443 is already used by another Tailscale Serve route; using direct port URL."
                        .to_owned(),
                ),
            },
            RouteOwnership::Absent => {
                match enable_serve_route(bin, dns, PORTAL_HTTPS_PORT, PORTAL_LOCAL_PORT).await {
                    Ok(()) => PortalSync {
                        active: true,
                        note: None,
                    },
                    Err(err) => PortalSync {
                        active: false,
                        note: Some(format!(
                            "Could not enable named shortcut on port 443 ({err}); using direct port URL."
                        )),
                    },
                }
            }
        },
        Err(err) => PortalSync {
            active: false,
            note: Some(format!(
                "Could not check Tailscale Serve for port 443 ({err}); using direct port URL."
            )),
        },
    }
}

/// Cached Tailscale Serve routes for active live development previews, keyed by
/// project ID (`project_id -> (local_port, remote_https_url)`). Keeping this
/// cache avoids shelling out to `tailscale` on every status poll and guarantees
/// preview routes are torn down cleanly when a preview stops or moves ports.
#[derive(Clone, Default)]
pub struct PreviewShares {
    active: Arc<Mutex<BTreeMap<String, (u16, String)>>>,
}

impl PreviewShares {
    /// Ensures the given loopback preview port is published privately over
    /// Tailscale Serve, reusing the cached route when the port has not changed.
    pub async fn ensure(&self, project_id: &str, local_port: u16) -> Result<String, String> {
        let previous_port = {
            let guard = self.active.lock().await;
            if let Some((port, url)) = guard.get(project_id) {
                if *port == local_port {
                    return Ok(url.clone());
                }
                Some(*port)
            } else {
                None
            }
        };
        if let Some(old_port) = previous_port {
            let _ = disable_port_serve(old_port).await;
        }
        let url = enable_port_serve(local_port).await?;
        let mut guard = self.active.lock().await;
        guard.insert(project_id.to_owned(), (local_port, url.clone()));
        Ok(url)
    }

    /// Tears down any Tailscale Serve route created for `project_id`'s preview.
    pub async fn clear(&self, project_id: &str) {
        let removed = {
            let mut guard = self.active.lock().await;
            guard.remove(project_id)
        };
        if let Some((port, _)) = removed {
            let _ = disable_port_serve(port).await;
        }
    }
}

/// Publishes a local loopback HTTP port (`127.0.0.1:<port>`) privately over
/// Tailscale Serve on `https://<node>.<tailnet>.ts.net:<port>`, reusing
/// Pockets' route-ownership checks so unrelated Serve routes are never touched.
pub async fn enable_port_serve(port: u16) -> Result<String, String> {
    let bin = find_tailscale().ok_or_else(|| {
        "Tailscale was not found on this machine. Install Tailscale from https://tailscale.com to access Pockets over your tailnet.".to_owned()
    })?;
    enable_port_serve_with_bin(&bin, port).await
}

async fn enable_port_serve_with_bin(bin: &Path, port: u16) -> Result<String, String> {
    let report = detect_with_bin(bin).await;
    if report.state != TailscaleState::Ready {
        return Err(report.message);
    }
    let dns_name = report.dns_name.as_deref().ok_or_else(|| {
        "Tailscale did not report a MagicDNS name for this machine.".to_owned()
    })?;
    enable_serve_route(bin, dns_name, port, port).await?;
    Ok(share_url(dns_name, port))
}

/// Removes a Pockets-owned Tailscale Serve route for `127.0.0.1:<port>` if
/// present; never touches absent or unrelated routes.
pub async fn disable_port_serve(port: u16) -> Result<(), String> {
    let Some(bin) = find_tailscale() else {
        return Ok(());
    };
    disable_port_serve_with_bin(&bin, port).await
}

async fn disable_port_serve_with_bin(bin: &Path, port: u16) -> Result<(), String> {
    let report = detect_with_bin(bin).await;
    if report.state != TailscaleState::Ready {
        return Ok(());
    }
    let Some(dns_name) = report.dns_name.as_deref() else {
        return Ok(());
    };
    disable_serve_route(bin, dns_name, port, port).await
}

/// Returns `Some("https://<node>.<tailnet>.ts.net:<port>")` when Tailscale
/// Serve currently owns the route from HTTPS `<port>` to `127.0.0.1:<port>`.
pub async fn active_port_serve_url(port: u16) -> Option<String> {
    let bin = find_tailscale()?;
    let report = detect_with_bin(&bin).await;
    if report.state != TailscaleState::Ready {
        return None;
    }
    let dns_name = report.dns_name.as_deref()?;
    let serve = inspect_serve_with_bin(&bin).await.ok()?;
    if route_ownership(&serve, dns_name, port, port) == RouteOwnership::Owned {
        Some(share_url(dns_name, port))
    } else {
        None
    }
}

/// Removes all Tailscale Serve routes owned by Pockets: the dashboard daemon
/// port(s), the port 443 shortcut portal, and every per-Pocket share recorded
/// in `tailscale-shares.json`. Never touches foreign or unrelated routes.
/// Returns the number of owned routes removed.
pub async fn disable_all_pockets_routes(
    store: &Store,
    daemon_ports: &[u16],
) -> Result<usize, String> {
    let Some(bin) = find_tailscale() else {
        return Ok(0);
    };
    disable_all_pockets_routes_with_bin(&bin, store, daemon_ports).await
}

async fn disable_all_pockets_routes_with_bin(
    bin: &Path,
    store: &Store,
    daemon_ports: &[u16],
) -> Result<usize, String> {
    let report = detect_with_bin(bin).await;
    if report.state != TailscaleState::Ready {
        return Ok(0);
    }
    let Some(dns_name) = report.dns_name.as_deref() else {
        return Ok(0);
    };
    let serve = inspect_serve_with_bin(bin).await?;

    let mut targets: Vec<(u16, u16)> = Vec::new();
    for &port in daemon_ports {
        if !targets.contains(&(port, port)) {
            targets.push((port, port));
        }
    }
    if !targets.contains(&(PORTAL_HTTPS_PORT, PORTAL_LOCAL_PORT)) {
        targets.push((PORTAL_HTTPS_PORT, PORTAL_LOCAL_PORT));
    }
    if let Ok(shares) = load_all_shares(store) {
        for record in shares.values() {
            if let Some(port) = record.port {
                if !targets.contains(&(port, port)) {
                    targets.push((port, port));
                }
            }
        }
    }

    let mut removed = 0usize;
    let mut errors = Vec::new();
    for (external_port, target_port) in targets {
        if route_ownership(&serve, dns_name, external_port, target_port) == RouteOwnership::Owned {
            let https_flag = format!("--https={external_port}");
            match runtime::run(bin, &["serve", &https_flag, "off"]).await {
                Ok(_) => removed += 1,
                Err(err) => errors.push(format_serve_error(err)),
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(removed)
}

/// Appends an actionable Linux operator hint when `tailscale serve` fails due
/// to non-root permissions on `tailscaled`.
fn format_serve_error(err: String) -> String {
    let lower = err.to_ascii_lowercase();
    let is_permission_error = lower.contains("access denied")
        || lower.contains("serve config denied")
        || lower.contains("permission denied")
        || lower.contains("must be run as root")
        || lower.contains("requires root")
        || lower.contains("operator");
    if is_permission_error && !lower.contains("tailscale set --operator") {
        let user = std::env::var("USER")
            .ok()
            .filter(|u| {
                !u.is_empty()
                    && u.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            })
            .unwrap_or_else(|| "$USER".to_owned());
        format!(
            "{err}. On Linux, grant your user permission once with: sudo tailscale set --operator={user}"
        )
    } else {
        err
    }
}

async fn inspect_serve_with_bin(bin: &Path) -> Result<ServeStatusJson, String> {
    let output = runtime::run(bin, &["serve", "status", "--json"]).await?;
    if output.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(ServeStatusJson::default());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse `tailscale serve status --json`: {err}"))
}

async fn enable_serve_route(
    bin: &Path,
    dns_name: &str,
    external_port: u16,
    target_port: u16,
) -> Result<(), String> {
    let before = inspect_serve_with_bin(bin).await?;
    match route_ownership(&before, dns_name, external_port, target_port) {
        RouteOwnership::Owned => return Ok(()),
        RouteOwnership::Foreign => {
            return Err(format!(
                "Tailscale Serve port {external_port} is already configured for another service. \
                 Remove that route in Tailscale before sharing this Pocket."
            ));
        }
        RouteOwnership::Absent => {}
    }

    let https_flag = format!("--https={external_port}");
    let target = format!("http://127.0.0.1:{target_port}");
    runtime::run(bin, &["serve", "--bg", "--yes", &https_flag, &target])
        .await
        .map_err(format_serve_error)?;

    let after = inspect_serve_with_bin(bin).await?;
    if route_ownership(&after, dns_name, external_port, target_port) != RouteOwnership::Owned {
        return Err(format!(
            "Tailscale accepted the serve command for port {external_port}, but the HTTPS route did not appear in `tailscale serve status`."
        ));
    }
    Ok(())
}

async fn disable_serve_route(
    bin: &Path,
    dns_name: &str,
    external_port: u16,
    target_port: u16,
) -> Result<(), String> {
    let current = inspect_serve_with_bin(bin).await?;
    match route_ownership(&current, dns_name, external_port, target_port) {
        RouteOwnership::Absent | RouteOwnership::Foreign => return Ok(()),
        RouteOwnership::Owned => {}
    }
    let https_flag = format!("--https={external_port}");
    runtime::run(bin, &["serve", &https_flag, "off"])
        .await
        .map_err(format_serve_error)?;
    Ok(())
}

fn route_ownership(
    serve: &ServeStatusJson,
    dns_name: &str,
    external_port: u16,
    target_port: u16,
) -> RouteOwnership {
    let port_key = external_port.to_string();
    let tcp_entry = serve.tcp.get(&port_key);
    let expected_host_port = format!("{dns_name}:{external_port}");
    let suffix = format!(":{external_port}");

    let matching_web: Vec<(&String, &WebServerConfig)> = serve
        .web
        .iter()
        .filter(|(host_port, _)| {
            host_port.eq_ignore_ascii_case(&expected_host_port) || host_port.ends_with(&suffix)
        })
        .collect();

    if tcp_entry.is_none() && matching_web.is_empty() {
        return RouteOwnership::Absent;
    }

    let Some(tcp) = tcp_entry else {
        return RouteOwnership::Foreign;
    };
    if !tcp.https || tcp.http || tcp.tcp_forward.is_some() {
        return RouteOwnership::Foreign;
    }
    if matching_web.len() != 1 {
        return RouteOwnership::Foreign;
    }
    let (host_port, web) = matching_web[0];
    if !host_port.eq_ignore_ascii_case(&expected_host_port) {
        return RouteOwnership::Foreign;
    }
    if web.handlers.len() != 1 {
        return RouteOwnership::Foreign;
    }
    let Some(root) = web.handlers.get("/") else {
        return RouteOwnership::Foreign;
    };
    if root.path.is_some() || root.text.is_some() {
        return RouteOwnership::Foreign;
    }
    let Some(proxy) = root.proxy.as_deref() else {
        return RouteOwnership::Foreign;
    };
    if is_expected_loopback_proxy(proxy, target_port) {
        RouteOwnership::Owned
    } else {
        RouteOwnership::Foreign
    }
}

fn is_expected_loopback_proxy(proxy: &str, port: u16) -> bool {
    let trimmed = proxy.trim_end_matches('/');
    trimmed == format!("http://127.0.0.1:{port}")
        || trimmed == format!("http://localhost:{port}")
        || trimmed == format!("http://[::1]:{port}")
}

fn share_url(dns_name: &str, port: u16) -> String {
    format!("https://{dns_name}:{port}")
}

fn portal_root_url(dns_name: &str) -> String {
    format!("https://{dns_name}")
}

fn named_url(dns_name: &str, slug: &str) -> String {
    format!("https://{dns_name}/{slug}")
}

fn normalize_version(raw: &str) -> Option<String> {
    let token = raw.trim().split('-').next()?.trim();
    if token.is_empty() || !token.starts_with(|c: char| c.is_ascii_digit()) {
        None
    } else {
        Some(token.to_owned())
    }
}

fn validate_dns_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if trimmed.is_empty() || trimmed.len() > 253 || !trimmed.contains('.') {
        return None;
    }
    for label in trimmed.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return None;
        }
    }
    Some(trimmed)
}

fn registry_path(store: &Store) -> PathBuf {
    store.root().with_file_name("tailscale-shares.json")
}

fn load_all_shares(store: &Store) -> Result<BTreeMap<String, ShareRecord>, String> {
    let path = registry_path(store);
    let io_error = |err| format!("Could not read Tailscale sharing preferences: {err}");
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|err| format!("Invalid Tailscale sharing registry: {err}")),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(err) => Err(io_error(err)),
    }
}

fn load_share(store: &Store, project_id: &str) -> Result<Option<ShareRecord>, String> {
    Ok(load_all_shares(store)?.remove(project_id))
}

fn save_share(store: &Store, project_id: &str, record: Option<ShareRecord>) -> Result<(), String> {
    let path = registry_path(store);
    let io_error = |err| format!("Could not update Tailscale sharing preferences: {err}");
    fs::create_dir_all(path.parent().ok_or("Missing app data directory.")?).map_err(io_error)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))
        .map_err(io_error)?;
    lock.lock().map_err(|err| {
        format!("Cannot lock Tailscale sharing registry. Retry the operation. {err}")
    })?;

    let mut map: BTreeMap<String, ShareRecord> = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|err| format!("Invalid Tailscale sharing registry: {err}"))?,
        Err(err) if err.kind() == ErrorKind::NotFound => BTreeMap::new(),
        Err(err) => return Err(io_error(err)),
    };

    match record {
        Some(entry) => {
            map.insert(project_id.to_owned(), entry);
        }
        None => {
            map.remove(project_id);
        }
    }

    let staged = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec(&map).map_err(|err| err.to_string())?;
    if let Err(err) = fs::write(&staged, bytes).and_then(|_| fs::rename(&staged, &path)) {
        let _ = fs::remove_file(staged);
        return Err(io_error(err));
    }
    Ok(())
}

fn candidate_paths(path_env: &OsStr) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(path_env) {
        let candidate = dir.join(TAILSCALE_EXE);
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    for known in KNOWN_LOCATIONS {
        let candidate = PathBuf::from(known);
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

fn first_executable(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|path| is_executable_file(path))
        .cloned()
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_json(
        backend: &str,
        dns: Option<&str>,
        cert_domains: Option<Vec<&str>>,
        caps: Vec<&str>,
    ) -> Vec<u8> {
        serde_json::json!({
            "Version": "1.104.6-t3b273a64c",
            "BackendState": backend,
            "Self": {
                "DNSName": dns,
                "Capabilities": caps,
            },
            "CertDomains": cert_domains,
        })
        .to_string()
        .into_bytes()
    }

    fn fake_tailscale(dir: &Path, body: &str) -> (PathBuf, PathBuf) {
        let record = dir.join("ts-calls.txt");
        let script = format!(
            r#"#!/bin/sh
{{ printf '%s ' "$@"; printf '\n'; }} >> '{}'
{body}
exit 0
"#,
            record.display()
        );
        let bin = dir.join("fake-tailscale");
        fs::write(&bin, script).expect("write fake tailscale");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))
                .expect("chmod fake tailscale");
        }
        (bin, record)
    }

    fn recorded_calls(record: &Path) -> Vec<String> {
        fs::read_to_string(record)
            .unwrap_or_default()
            .lines()
            .map(|line| line.trim_end().to_owned())
            .collect()
    }

    #[test]
    fn parses_ready_stopped_missing_dns_and_https_disabled_reports() {
        let ready = parse_status_report(&status_json(
            "Running",
            Some("macbook-pro.tail00000.ts.net."),
            Some(vec!["macbook-pro.tail00000.ts.net"]),
            vec!["https"],
        ));
        assert_eq!(ready.state, TailscaleState::Ready);
        assert_eq!(
            ready.dns_name.as_deref(),
            Some("macbook-pro.tail00000.ts.net")
        );
        assert_eq!(ready.version.as_deref(), Some("1.104.6"));

        let stopped = parse_status_report(&status_json(
            "Stopped",
            Some("macbook-pro.tail00000.ts.net."),
            Some(vec!["macbook-pro.tail00000.ts.net"]),
            vec!["https"],
        ));
        assert_eq!(stopped.state, TailscaleState::Unavailable);
        assert!(stopped.message.contains("disconnected"));

        let needs_login = parse_status_report(&status_json("NeedsLogin", None, None, vec![]));
        assert_eq!(needs_login.state, TailscaleState::Unavailable);
        assert!(needs_login.message.contains("signed in"));

        let no_dns = parse_status_report(&status_json("Running", None, None, vec!["https"]));
        assert_eq!(no_dns.state, TailscaleState::Unavailable);
        assert!(no_dns.message.contains("MagicDNS"));

        let no_https = parse_status_report(&status_json(
            "Running",
            Some("macbook-pro.tail00000.ts.net."),
            Some(vec![]),
            vec![],
        ));
        assert_eq!(no_https.state, TailscaleState::HttpsDisabled);
        assert_eq!(
            no_https.dns_name.as_deref(),
            Some("macbook-pro.tail00000.ts.net")
        );
    }

    #[test]
    fn dns_validation_rejects_hostile_names() {
        assert_eq!(
            validate_dns_name("MacBook-Pro.tail00000.ts.net."),
            Some("macbook-pro.tail00000.ts.net".to_owned())
        );
        assert_eq!(validate_dns_name("localhost"), None);
        assert_eq!(validate_dns_name("bad host.ts.net"), None);
        assert_eq!(validate_dns_name("bad/path.ts.net"), None);
        assert_eq!(validate_dns_name("-bad.ts.net"), None);
        assert_eq!(validate_dns_name("bad..ts.net"), None);
    }

    #[test]
    fn route_ownership_distinguishes_absent_owned_and_foreign_routes() {
        let dns = "macbook-pro.tail00000.ts.net";
        let empty: ServeStatusJson = serde_json::from_str("{}").unwrap();
        assert_eq!(
            route_ownership(&empty, dns, 18788, 18788),
            RouteOwnership::Absent
        );

        let owned: ServeStatusJson = serde_json::from_value(serde_json::json!({
            "TCP": { "18788": { "HTTPS": true }, "443": { "HTTPS": true } },
            "Web": {
                "macbook-pro.tail00000.ts.net:18788": {
                    "Handlers": {
                        "/": { "Proxy": "http://127.0.0.1:18788" }
                    }
                },
                "macbook-pro.tail00000.ts.net:443": {
                    "Handlers": {
                        "/": { "Proxy": "http://127.0.0.1:18787" }
                    }
                }
            }
        }))
        .unwrap();
        assert_eq!(
            route_ownership(&owned, dns, 18788, 18788),
            RouteOwnership::Owned
        );
        assert_eq!(
            route_ownership(&owned, dns, 443, PORTAL_LOCAL_PORT),
            RouteOwnership::Owned
        );

        let foreign_target: ServeStatusJson = serde_json::from_value(serde_json::json!({
            "TCP": { "18788": { "HTTPS": true } },
            "Web": {
                "macbook-pro.tail00000.ts.net:18788": {
                    "Handlers": {
                        "/": { "Proxy": "http://127.0.0.1:3000" }
                    }
                }
            }
        }))
        .unwrap();
        assert_eq!(
            route_ownership(&foreign_target, dns, 18788, 18788),
            RouteOwnership::Foreign
        );

        let foreign_http: ServeStatusJson = serde_json::from_value(serde_json::json!({
            "TCP": { "18788": { "HTTP": true } },
            "Web": {
                "macbook-pro.tail00000.ts.net:18788": {
                    "Handlers": {
                        "/": { "Proxy": "http://127.0.0.1:18788" }
                    }
                }
            }
        }))
        .unwrap();
        assert_eq!(
            route_ownership(&foreign_http, dns, 18788, 18788),
            RouteOwnership::Foreign
        );
    }

    #[tokio::test]
    async fn enable_pause_restore_and_delete_manage_pocket_and_portal_routes() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("projects"));
        let pocket_state = temp.path().join("pocket-route.flag");
        let portal_state = temp.path().join("portal-route.flag");
        let status_body = String::from_utf8(status_json(
            "Running",
            Some("macbook-pro.tail00000.ts.net."),
            Some(vec!["macbook-pro.tail00000.ts.net"]),
            vec!["https"],
        ))
        .unwrap();

        let script = format!(
            r#"
POCKET_FLAG='{}'
PORTAL_FLAG='{}'
if [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    printf '%s' '{status_body}'
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    if [ -f "$POCKET_FLAG" ] && [ -f "$PORTAL_FLAG" ]; then
        printf '%s' '{{"TCP":{{"18788":{{"HTTPS":true}},"443":{{"HTTPS":true}}}},"Web":{{"macbook-pro.tail00000.ts.net:18788":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:18788"}}}}}},"macbook-pro.tail00000.ts.net:443":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:18787"}}}}}}}}}}'
    elif [ -f "$POCKET_FLAG" ]; then
        printf '%s' '{{"TCP":{{"18788":{{"HTTPS":true}}}},"Web":{{"macbook-pro.tail00000.ts.net:18788":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:18788"}}}}}}}}}}'
    elif [ -f "$PORTAL_FLAG" ]; then
        printf '%s' '{{"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"macbook-pro.tail00000.ts.net:443":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:18787"}}}}}}}}}}'
    else
        printf '{{}}'
    fi
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "--bg" ] && [ "$3" = "--yes" ] && [ "$4" = "--https=18788" ] && [ "$5" = "http://127.0.0.1:18788" ]; then
    touch "$POCKET_FLAG"
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "--bg" ] && [ "$3" = "--yes" ] && [ "$4" = "--https=443" ] && [ "$5" = "http://127.0.0.1:18787" ]; then
    touch "$PORTAL_FLAG"
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "--https=18788" ] && [ "$3" = "off" ]; then
    rm -f "$POCKET_FLAG"
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "--https=443" ] && [ "$3" = "off" ]; then
    rm -f "$PORTAL_FLAG"
    exit 0
fi
echo "unexpected args: $@" >&2
exit 1
"#,
            pocket_state.display(),
            portal_state.display()
        );
        let (bin, calls) = fake_tailscale(temp.path(), &script);
        let project_id = "a".repeat(32);

        // Enable while running -> activates both port 18788 and port 443 portal -> named URL!
        let view = set_enabled_with_bin(
            &store,
            None,
            Some(&bin),
            &project_id,
            "notes",
            true,
            Some(18788),
        )
        .await
        .unwrap();
        assert_eq!(
            view,
            TailscaleShareView {
                enabled: true,
                active: true,
                url: Some("https://macbook-pro.tail00000.ts.net/notes".to_owned()),
                direct_url: Some("https://macbook-pro.tail00000.ts.net:18788".to_owned()),
                portal_url: Some("https://macbook-pro.tail00000.ts.net".to_owned()),
                message: None,
            }
        );

        // Stop -> pauses Pocket port 18788, keeps portal 443 up showing paused state.
        let paused = pause_on_stop_with_bin(&store, None, Some(&bin), &project_id).await;
        assert!(paused.enabled);
        assert!(!paused.active);
        assert!(!pocket_state.exists());
        assert!(portal_state.exists());

        // Start -> restores port 18788 and returns named URL.
        let restored =
            restore_on_start_with_bin(&store, None, Some(&bin), &project_id, "notes", 18788).await;
        assert!(restored.active);
        assert_eq!(
            restored.url.as_deref(),
            Some("https://macbook-pro.tail00000.ts.net/notes")
        );

        // Delete last shared Pocket -> tears down BOTH port 18788 and port 443.
        cleanup_on_delete_with_bin(&store, None, Some(&bin), &project_id).await;
        assert!(!pocket_state.exists());
        assert!(!portal_state.exists());
        assert!(load_share(&store, &project_id).unwrap().is_none());

        let log = recorded_calls(&calls);
        assert!(log
            .iter()
            .any(|c| c == "serve --bg --yes --https=18788 http://127.0.0.1:18788"));
        assert!(log
            .iter()
            .any(|c| c == "serve --bg --yes --https=443 http://127.0.0.1:18787"));
        assert!(log.iter().any(|c| c == "serve --https=18788 off"));
        assert!(log.iter().any(|c| c == "serve --https=443 off"));
        assert!(!log
            .iter()
            .any(|c| c.contains("funnel") || c.contains("reset")));
    }

    #[tokio::test]
    async fn falls_back_to_direct_port_url_when_port_443_is_used_by_another_service() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("projects"));
        let pocket_state = temp.path().join("pocket.flag");
        let status_body = String::from_utf8(status_json(
            "Running",
            Some("macbook-pro.tail00000.ts.net."),
            Some(vec!["macbook-pro.tail00000.ts.net"]),
            vec!["https"],
        ))
        .unwrap();
        let script = format!(
            r#"
POCKET_FLAG='{}'
if [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    printf '%s' '{status_body}'
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    if [ -f "$POCKET_FLAG" ]; then
        printf '%s' '{{"TCP":{{"443":{{"HTTPS":true}},"18788":{{"HTTPS":true}}}},"Web":{{"macbook-pro.tail00000.ts.net:443":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:5000"}}}}}},"macbook-pro.tail00000.ts.net:18788":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:18788"}}}}}}}}}}'
    else
        printf '%s' '{{"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"macbook-pro.tail00000.ts.net:443":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:5000"}}}}}}}}}}'
    fi
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "--bg" ] && [ "$3" = "--yes" ] && [ "$4" = "--https=18788" ]; then
    touch "$POCKET_FLAG"
    exit 0
fi
echo "must not touch foreign 443: $@" >&2
exit 1
"#,
            pocket_state.display()
        );
        let (bin, calls) = fake_tailscale(temp.path(), &script);
        let project_id = "e".repeat(32);

        let view = set_enabled_with_bin(
            &store,
            None,
            Some(&bin),
            &project_id,
            "notes",
            true,
            Some(18788),
        )
        .await
        .unwrap();
        assert!(view.active);
        assert_eq!(
            view.url.as_deref(),
            Some("https://macbook-pro.tail00000.ts.net:18788")
        );
        assert_eq!(view.portal_url, None);
        assert!(view.message.unwrap().contains("Port 443"));
        assert!(!recorded_calls(&calls)
            .iter()
            .any(|c| c.contains("--https=443")));
    }

    #[tokio::test]
    async fn refuses_to_clobber_foreign_serve_route_and_never_removes_it() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("projects"));
        let status_body = String::from_utf8(status_json(
            "Running",
            Some("macbook-pro.tail00000.ts.net."),
            Some(vec!["macbook-pro.tail00000.ts.net"]),
            vec!["https"],
        ))
        .unwrap();
        let foreign_serve = serde_json::json!({
            "TCP": { "18788": { "HTTPS": true } },
            "Web": {
                "macbook-pro.tail00000.ts.net:18788": {
                    "Handlers": { "/": { "Proxy": "http://127.0.0.1:9999" } }
                }
            }
        })
        .to_string();
        let script = format!(
            r#"
if [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    printf '%s' '{status_body}'
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    printf '%s' '{foreign_serve}'
    exit 0
fi
echo "should not mutate foreign route: $@" >&2
exit 1
"#
        );
        let (bin, calls) = fake_tailscale(temp.path(), &script);
        let project_id = "b".repeat(32);

        let err = set_enabled_with_bin(
            &store,
            None,
            Some(&bin),
            &project_id,
            "notes",
            true,
            Some(18788),
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("already configured for another service"),
            "got {err}"
        );
        assert!(load_share(&store, &project_id).unwrap().is_none());

        // Even if a stale record existed, disable must never run `off` on a foreign route.
        save_share(
            &store,
            &project_id,
            Some(ShareRecord {
                enabled: true,
                port: Some(18788),
            }),
        )
        .unwrap();
        let disabled = set_enabled_with_bin(
            &store,
            None,
            Some(&bin),
            &project_id,
            "notes",
            false,
            Some(18788),
        )
        .await
        .unwrap();
        assert!(!disabled.enabled);
        let log = recorded_calls(&calls);
        assert!(!log.iter().any(|c| c.contains("off")));
    }

    #[tokio::test]
    async fn enabling_while_stopped_remembers_opt_in_without_serving_pocket_port_until_start() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("projects"));
        let portal_state = temp.path().join("portal.flag");
        let status_body = String::from_utf8(status_json(
            "Running",
            Some("macbook-pro.tail00000.ts.net."),
            Some(vec!["macbook-pro.tail00000.ts.net"]),
            vec!["https"],
        ))
        .unwrap();
        let script = format!(
            r#"
PORTAL_FLAG='{}'
if [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    printf '%s' '{status_body}'
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    if [ -f "$PORTAL_FLAG" ]; then
        printf '%s' '{{"TCP":{{"443":{{"HTTPS":true}}}},"Web":{{"macbook-pro.tail00000.ts.net:443":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:18787"}}}}}}}}}}'
    else
        printf '{{}}'
    fi
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "--bg" ] && [ "$3" = "--yes" ] && [ "$4" = "--https=443" ]; then
    touch "$PORTAL_FLAG"
    exit 0
fi
echo "should not open a pocket port while stopped: $@" >&2
exit 1
"#,
            portal_state.display()
        );
        let (bin, calls) = fake_tailscale(temp.path(), &script);
        let project_id = "c".repeat(32);

        let view = set_enabled_with_bin(&store, None, Some(&bin), &project_id, "notes", true, None)
            .await
            .unwrap();
        assert_eq!(view, TailscaleShareView::paused());
        let saved = load_share(&store, &project_id).unwrap().unwrap();
        assert!(saved.enabled);
        assert_eq!(saved.port, None);
        assert!(!recorded_calls(&calls)
            .iter()
            .any(|c| c.contains("--https=18788")));
    }

    #[tokio::test]
    async fn restore_on_start_never_fails_when_tailscale_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("projects"));
        let project_id = "d".repeat(32);
        save_share(
            &store,
            &project_id,
            Some(ShareRecord {
                enabled: true,
                port: None,
            }),
        )
        .unwrap();

        let view = restore_on_start_with_bin(&store, None, None, &project_id, "notes", 18788).await;
        assert!(view.enabled);
        assert!(!view.active);
        assert_eq!(view.url, None);
        assert!(view.message.unwrap().contains("Tailscale was not found"));
    }

    #[test]
    fn format_serve_error_appends_linux_operator_hint_on_access_denied() {
        let formatted = format_serve_error("Access denied: serve config denied".to_owned());
        assert!(
            formatted.contains("sudo tailscale set --operator="),
            "got {formatted}"
        );

        let unrelated = format_serve_error("network timeout".to_owned());
        assert_eq!(unrelated, "network timeout");
    }

    #[tokio::test]
    async fn enable_and_disable_port_serve_manages_dashboard_route_safely() {
        let temp = tempfile::tempdir().unwrap();
        let dash_flag = temp.path().join("dash-route.flag");
        let status_body = String::from_utf8(status_json(
            "Running",
            Some("cloud-vm.tail00000.ts.net."),
            Some(vec!["cloud-vm.tail00000.ts.net"]),
            vec!["https"],
        ))
        .unwrap();
        let script = format!(
            r#"
DASH_FLAG='{}'
if [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    printf '%s' '{status_body}'
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    if [ -f "$DASH_FLAG" ]; then
        printf '%s' '{{"TCP":{{"18786":{{"HTTPS":true}}}},"Web":{{"cloud-vm.tail00000.ts.net:18786":{{"Handlers":{{"/":{{"Proxy":"http://127.0.0.1:18786"}}}}}}}}}}'
    else
        printf '{{}}'
    fi
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "--bg" ] && [ "$3" = "--yes" ] && [ "$4" = "--https=18786" ] && [ "$5" = "http://127.0.0.1:18786" ]; then
    touch "$DASH_FLAG"
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "--https=18786" ] && [ "$3" = "off" ]; then
    rm -f "$DASH_FLAG"
    exit 0
fi
echo "unexpected args: $@" >&2
exit 1
"#,
            dash_flag.display()
        );
        let (bin, calls) = fake_tailscale(temp.path(), &script);

        let url = enable_port_serve_with_bin(&bin, 18786).await.unwrap();
        assert_eq!(url, "https://cloud-vm.tail00000.ts.net:18786");
        assert!(dash_flag.exists());

        disable_port_serve_with_bin(&bin, 18786).await.unwrap();
        assert!(!dash_flag.exists());

        let log = recorded_calls(&calls);
        assert!(log
            .iter()
            .any(|c| c == "serve --bg --yes --https=18786 http://127.0.0.1:18786"));
        assert!(log.iter().any(|c| c == "serve --https=18786 off"));
    }

    #[tokio::test]
    async fn disable_all_pockets_routes_removes_only_owned_routes() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("projects"));
        let project_id = "e".repeat(32);
        save_share(
            &store,
            &project_id,
            Some(ShareRecord {
                enabled: true,
                port: Some(18788),
            }),
        )
        .unwrap();
        let foreign_id = "f".repeat(32);
        save_share(
            &store,
            &foreign_id,
            Some(ShareRecord {
                enabled: true,
                port: Some(18789),
            }),
        )
        .unwrap();

        let status_body = String::from_utf8(status_json(
            "Running",
            Some("macbook.tail00000.ts.net."),
            Some(vec!["macbook.tail00000.ts.net"]),
            vec!["https"],
        ))
        .unwrap();
        // Port 18786 (daemon), 443 (portal -> 18787), and 18788 (pocket) are owned.
        // Port 18789 proxies to a foreign backend (http://127.0.0.1:9999) so must NOT be removed.
        let serve_body = r#"{"TCP":{"443":{"HTTPS":true},"18786":{"HTTPS":true},"18788":{"HTTPS":true},"18789":{"HTTPS":true}},"Web":{"macbook.tail00000.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:18787"}}},"macbook.tail00000.ts.net:18786":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:18786"}}},"macbook.tail00000.ts.net:18788":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:18788"}}},"macbook.tail00000.ts.net:18789":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:9999"}}}}}"#;
        let script = format!(
            r#"
if [ "$1" = "status" ] && [ "$2" = "--json" ]; then
    printf '%s' '{status_body}'
    exit 0
fi
if [ "$1" = "serve" ] && [ "$2" = "status" ] && [ "$3" = "--json" ]; then
    printf '%s' '{serve_body}'
    exit 0
fi
if [ "$1" = "serve" ] && [ "$3" = "off" ]; then
    exit 0
fi
echo "unexpected args: $@" >&2
exit 1
"#
        );
        let (bin, calls) = fake_tailscale(temp.path(), &script);

        let removed = disable_all_pockets_routes_with_bin(&bin, &store, &[18786])
            .await
            .unwrap();
        assert_eq!(removed, 3);
        let log = recorded_calls(&calls);
        assert!(log.iter().any(|c| c == "serve --https=18786 off"));
        assert!(log.iter().any(|c| c == "serve --https=443 off"));
        assert!(log.iter().any(|c| c == "serve --https=18788 off"));
        assert!(!log.iter().any(|c| c.contains("18789")));
    }
}
