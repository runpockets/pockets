//! The native preview lifecycle for linked projects. Project commands resolve
//! the registered source, and the runtime mounts a private read-only mirror.
//! Preview containers and disposable data volumes use a separate ownership
//! namespace from installed apps. The proxy stays on loopback and reloads the
//! webview once the container sees a source change and passes its health check.

use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::manifest::{self, Manifest};
use crate::pocket;
use crate::preview_egress;
use crate::preview_proxy::{self, LiveSignal, ProxyHandle};
use crate::preview_source::PreviewSource;
use crate::runtime;

/// Owned container name prefix, prefixed to the validated session id:
/// `pockets-preview-<id>`. Distinct from the managed prefix so preview
/// lifecycle never names an installed app's container, and vice versa.
pub(crate) const PREVIEW_NAME_PREFIX: &str = "pockets-preview-";

/// Ownership label value prefix: `dev.pockets.managed=preview-<id>` marks a
/// preview container or volume, exactly the way `project-<id>` marks a built
/// Pocket (see [`pocket::OWNERSHIP_LABEL`]).
const PREVIEW_OWNERSHIP_PREFIX: &str = "preview-";

/// The single shared Pockets-owned internal network. Created on demand with
/// `podman network create --internal`: host-published loopback ports still
/// reach the app while container egress is denied (spike 2). One network
/// serves all previews because previews never talk to each other either way.
const PREVIEW_NETWORK: &str = "pockets-preview";

/// Node image used by the current preview profile. It must already be cached.
const PREVIEW_IMAGE: &str = "docker.io/library/node:24-alpine";

#[derive(Default)]
pub(crate) struct LaunchOptions {
    pub variables: crate::runtime_env::Values,
    pub allowed_hosts: Vec<String>,
}

#[cfg(test)]
#[path = "preview_reload_live_tests.rs"]
mod reload_live_tests;

/// Container mount of the workspace, the same destination the isolated AI
/// checks use (`CONTAINER_WORKDIR` in `crate::ai::tools`), so generated code
/// sees one consistent layout in checks and in preview.
const CONTAINER_WORKDIR: &str = "/work";

/// Memory bound for a preview container, mirroring the check container.
const PREVIEW_MEMORY: &str = "512m";

/// The only host address a preview port may be published on, exactly as
/// `pocket.rs` pins for installed Pockets.
const LOOPBACK_IP: &str = "127.0.0.1";

/// Upper bound on retained transition events per preview. Older events are
/// evicted; cursors are revision numbers, so eviction never corrupts a
/// poller's position — it only means very old transitions are no longer
/// re-readable (same bounded spirit as the generation transcript cap).
const MAX_PREVIEW_EVENTS: usize = 32;

/// Pause between container-visibility checks while gating a refresh. Spike 1
/// polled content at 150 ms and saw in-place edits in 0.35–0.52 s; the same
/// cadence answers "is the rename visible yet?" without hammering `exec`.
const VISIBILITY_POLL: Duration = Duration::from_millis(150);

/// Total window [`refresh`] waits for container-visible propagation before it
/// refuses to restart. Rename-based edits measured 3.5–4.6 s into the
/// container; 5 s covers the observed maximum with margin, and beyond it a
/// restart is forbidden (it would crash into the rename trap).
const VISIBILITY_BUDGET: Duration = Duration::from_secs(5);

/// Lifecycle state of one session's preview. Serializes lowercase to the exact
/// status names `docs/live-preview.md` puts on the proxy SSE and the poll
/// surface: `preparing | updating | ready | failed | stopped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewState {
    /// Provisioning is running or has failed preconditions; no app is live.
    Preparing,
    /// A restart cycle is running: visibility gate, restart, then re-probe.
    Updating,
    /// The owned container serves a healthy, inspected loopback URL.
    Ready,
    /// The last operation failed; `error` carries the actionable text and the
    /// failed event carries its timestamp.
    Failed,
    /// Stopped and removed by an explicit stop or the launch sweep.
    Stopped,
}

/// One state transition, timestamped, carried in the bounded ring so a
/// poller can render a truthful timeline. `revision` is both the event's
/// monotonic position and the cursor unit: the ring may evict, but revisions
/// never repeat or reorder.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewEvent {
    pub revision: u64,
    pub state: PreviewState,
    /// Unix seconds at which the transition was recorded.
    pub at_seconds: i64,
    /// Failure or status detail when the transition carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What one `ai_preview_status` poll answers, plus what start/refresh return.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewStatus {
    pub state: PreviewState,
    pub revision: u64,
    /// Loopback URL of a healthy preview; `None` unless [`PreviewState::Ready`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Transitions with `revision > cursor`, oldest first, bounded by
    /// [`MAX_PREVIEW_EVENTS`].
    pub events_since: Vec<PreviewEvent>,
    /// Revision to pass as the next poll's cursor.
    pub next_cursor: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The per-session registry entry: the authoritative in-memory state machine.
/// Previews are deliberately not persisted — they never survive a Desktop
/// restart — so this entry plus the owned engine objects is the whole truth.
#[derive(Debug)]
struct PreviewEntry {
    state: PreviewState,
    revision: u64,
    url: Option<String>,
    error: Option<String>,
    events: VecDeque<PreviewEvent>,
    /// Raw loopback URL of the owned container mapping — internal only. The
    /// user-facing `url` above is the proxy URL in front of it
    /// (`docs/live-preview.md`: the pane and the escape hatch load the same
    /// proxy URL), and `stop` needs this to reason about the container.
    container_url: Option<String>,
    /// Live-update fanout for this session's proxy. Created fresh by
    /// [`Previews::begin`]; dropped by `stop` so every open SSE stream sees
    /// `Closed`.
    live: Option<broadcast::Sender<LiveSignal>>,
    /// The host-side proxy listener serving this preview, owned by the entry
    /// so `stop` (or a re-`start`) can shut it down.
    proxy: Option<ProxyHandle>,
}

impl PreviewEntry {
    fn new() -> PreviewEntry {
        PreviewEntry {
            state: PreviewState::Stopped,
            revision: 0,
            url: None,
            error: None,
            events: VecDeque::new(),
            container_url: None,
            live: None,
            proxy: None,
        }
    }

    /// Record one transition: bump the revision, resolve url/error, append
    /// the event, and evict the oldest event past the ring bound. Every state
    /// change in this module goes through here, which is what makes the
    /// revision a faithful cursor.
    ///
    /// URL lifecycle: the user-facing (proxy) URL never churns. `Preparing`
    /// and `Stopped` (re)begin or tear down the lifecycle and clear it; the
    /// other states keep the previous URL unless the transition supplies a
    /// new one — a refresh restart must not blank the pane URL mid-edit
    /// (`begin` and `stop` are the only clearing transitions by design).
    fn transition(
        &mut self,
        state: PreviewState,
        url: Option<String>,
        error: Option<String>,
        at_seconds: i64,
    ) {
        self.revision += 1;
        self.state = state;
        self.url = match state {
            PreviewState::Preparing | PreviewState::Stopped => None,
            _ => url.or_else(|| self.url.clone()),
        };
        self.error = error.clone();
        self.events.push_back(PreviewEvent {
            revision: self.revision,
            state,
            at_seconds,
            message: error,
        });
        if self.events.len() > MAX_PREVIEW_EVENTS {
            self.events.pop_front();
        }
    }

    /// Appends a message to the entry's current state without moving it:
    /// the revision bumps and the text rides the event exactly like a
    /// transition's, but state, URL, and error all stay as they are. This is
    /// how a refresh refused before any restart stays truthful — the running
    /// preview is untouched, so the entry must not claim `failed`.
    fn note(&mut self, message: &str, at_seconds: i64) {
        self.revision += 1;
        self.events.push_back(PreviewEvent {
            revision: self.revision,
            state: self.state,
            at_seconds,
            message: Some(message.to_owned()),
        });
        if self.events.len() > MAX_PREVIEW_EVENTS {
            self.events.pop_front();
        }
    }

    fn status(&self, cursor: u64) -> PreviewStatus {
        PreviewStatus {
            state: self.state,
            revision: self.revision,
            url: self.url.clone(),
            events_since: self
                .events
                .iter()
                .filter(|event| event.revision > cursor)
                .cloned()
                .collect(),
            next_cursor: self.revision,
            error: self.error.clone(),
        }
    }
}

/// Process-wide preview state injected through `tauri::State`. Entries stay
/// after a stop (as `stopped`) so a late poll still finds its history; that
/// is bounded per app run by the number of generation sessions.
#[derive(Default, Clone)]
pub struct Previews {
    entries: Arc<Mutex<HashMap<String, PreviewEntry>>>,
}

impl Previews {
    /// Poisoned locks are recoverable: a panic in one command cannot corrupt
    /// the map itself (same reasoning as `crate::ai::agent::Sessions`).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PreviewEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Open (or re-open) a provisioning cycle: a fresh or `stopped` entry
    /// enters `preparing`. An entry already active in this process is also
    /// reset to `preparing`, because mutations hold the lifecycle lock and no
    /// engine work can be in flight when a start begins. Returns the new
    /// session fanout sender to hand to [`preview_proxy::spawn`]; a previous
    /// start's proxy listener is shut down and its fanout replaced, so no
    /// browser can keep talking to a stale upstream through a re-start.
    fn begin(&self, session_id: &str) -> broadcast::Sender<LiveSignal> {
        let at = now_seconds();
        let mut entries = self.lock();
        let entry = entries
            .entry(session_id.to_owned())
            .or_insert_with(PreviewEntry::new);
        if let Some(stale) = entry.proxy.take() {
            stale.stop();
        }
        let (sender, _receiver) = broadcast::channel(preview_proxy::LIVE_CHANNEL_CAPACITY);
        entry.live = Some(sender.clone());
        entry.container_url = None;
        entry.transition(PreviewState::Preparing, None, None, at);
        sender
    }

    /// Record a transition on an existing entry, creating one if a command
    /// raced past [`Previews::begin`] (defensive: never lose a state change).
    /// Every transition is also published on the session's live fanout, so
    /// the proxy's SSE relays `preparing | updating | ready | failed |
    /// stopped` without the frontend having to poll for it.
    fn transition(
        &self,
        session_id: &str,
        state: PreviewState,
        url: Option<String>,
        error: Option<String>,
    ) -> PreviewStatus {
        let at = now_seconds();
        let (status, live) = {
            let mut entries = self.lock();
            let entry = entries
                .entry(session_id.to_owned())
                .or_insert_with(PreviewEntry::new);
            entry.transition(state, url, error, at);
            (entry.status(0), entry.live.clone())
        };
        if let Some(sender) = live {
            let _ = sender.send(LiveSignal::State(state));
        }
        status
    }

    /// Records a healthy start's two URLs: `container_url` stays internal
    /// (the proxy forwards to it), while the entry's user-facing `url`
    /// becomes the proxy URL — the documented surface
    /// (`docs/live-preview.md`, Mechanism: the pane and the escape-hatch
    /// browser load the same proxy URL). The handle is retained so `stop`
    /// can shut the listener down, and the `ready` transition publishes on
    /// the live fanout like every other transition.
    fn attach_proxy(
        &self,
        session_id: &str,
        container_url: String,
        proxy: ProxyHandle,
    ) -> PreviewStatus {
        let at = now_seconds();
        let (status, live) = {
            let mut entries = self.lock();
            let entry = entries
                .entry(session_id.to_owned())
                .or_insert_with(PreviewEntry::new);
            let user_url = Some(proxy.url().to_owned());
            entry.container_url = Some(container_url);
            entry.proxy = Some(proxy);
            entry.transition(PreviewState::Ready, user_url, None, at);
            (entry.status(0), entry.live.clone())
        };
        if let Some(sender) = live {
            let _ = sender.send(LiveSignal::State(PreviewState::Ready));
        }
        status
    }

    /// Publishes the changed workspace-relative paths on the session's live
    /// fanout; the proxy tiers them into client messages. Best effort by
    /// design: with no channel or no subscriber the polled status surface
    /// still reports the transition, and the next connection re-syncs.
    fn publish_changed(&self, session_id: &str, paths: Vec<String>) {
        let live = self
            .lock()
            .get(session_id)
            .and_then(|entry| entry.live.clone());
        if let Some(sender) = live {
            let _ = sender.send(LiveSignal::Changed(paths));
        }
    }

    pub(crate) fn fail(&self, id: &str, error: &str) {
        if self.contains(id) {
            self.transition(id, PreviewState::Failed, None, Some(error.into()));
        }
    }

    pub(crate) fn prepare(&self, id: &str) {
        let _ = self.reuse_live(id);
        if self
            .status(id, 0)
            .is_some_and(|status| status.state != PreviewState::Preparing)
        {
            self.transition(id, PreviewState::Updating, None, None);
        }
    }

    fn reuse_live(&self, id: &str) -> broadcast::Sender<LiveSignal> {
        if let Some(sender) = self.lock().get(id).and_then(|entry| entry.live.clone()) {
            return sender;
        }
        self.begin(id)
    }

    fn retarget(&self, id: &str, url: &str) -> Result<bool, String> {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(id) else {
            return Ok(false);
        };
        let Some(proxy) = &entry.proxy else {
            return Ok(false);
        };
        proxy.retarget(url)?;
        entry.container_url = Some(url.into());
        Ok(true)
    }

    fn publish_reload(&self, id: &str) {
        if let Some(sender) = self.lock().get(id).and_then(|entry| entry.live.clone()) {
            let _ = sender.send(LiveSignal::Reload);
        }
    }

    /// Records a pre-restart refresh refusal as an actionable event on the
    /// entry's current state, leaving state, URL, and error untouched: the
    /// old process was never touched, so the entry must not claim `failed`
    /// or `stopped`, and the next turn can refresh again. Nothing publishes
    /// on the live fanout — the state did not move, so the browser's page is
    /// still the truth.
    fn note_refusal(&self, session_id: &str, message: &str) {
        let at = now_seconds();
        if let Some(entry) = self.lock().get_mut(session_id) {
            entry.note(message, at);
        }
    }

    /// Detaches the entry's proxy listener and fanout sender. The caller
    /// stops the listener; dropping the registry's sender ends every open
    /// SSE stream once the accept loop drops its own clone.
    fn detach_proxy(&self, session_id: &str) -> Option<ProxyHandle> {
        let mut entries = self.lock();
        let entry = entries.get_mut(session_id)?;
        entry.live = None;
        entry.container_url = None;
        entry.proxy.take()
    }

    /// The entry's current user-facing (proxy) URL. `refresh` keeps it
    /// stable across the restart: the name, port mapping, and volume survive,
    /// and the proxy is not re-created, so the pane URL never churns.
    fn user_url(&self, session_id: &str) -> Option<String> {
        self.lock()
            .get(session_id)
            .and_then(|entry| entry.url.clone())
    }

    /// Registry snapshot for one session without touching the engine. Also
    /// read by the preview pane (`crate::preview_pane`) in one atomic call to
    /// get the state plus the stable user-facing URL — `pub(crate)` for that
    /// consumer only; the engine is never touched from there either.
    pub(crate) fn status(&self, session_id: &str, cursor: u64) -> Option<PreviewStatus> {
        self.lock()
            .get(session_id)
            .map(|entry| entry.status(cursor))
    }

    /// Whether an entry exists at all (refresh requires one; stop answers
    /// `false` for a session that never had a preview).
    fn contains(&self, session_id: &str) -> bool {
        self.lock().contains_key(session_id)
    }

    /// Previews that still hold engine objects: everything except `stopped`.
    /// The shared network is removed only when this reaches zero.
    fn active_count(&self) -> usize {
        self.lock()
            .values()
            .filter(|entry| entry.state != PreviewState::Stopped)
            .count()
    }
}

/// Reports one session's preview state and the transitions since `cursor`.
///
/// A pure registry read — no lifecycle lock, no engine call — so polling
/// stays responsive while a provision or restart runs. An unknown session is
/// an actionable error, not a fabricated `stopped`: the frontend must not
/// invent previews for sessions that never started one.
pub(crate) fn status(
    previews: &Previews,
    session_id: &str,
    cursor: u64,
) -> Result<PreviewStatus, String> {
    validate_session_id(session_id)?;
    previews.status(session_id, cursor).ok_or_else(|| {
        format!(
            "session {session_id:?} has no preview yet. Start it once the workspace is runnable."
        )
    })
}

/// Whether `url` has the exact shape `http://127.0.0.1:<port>` — the only
/// form a preview's user-facing proxy URL can take (the proxy always binds
/// the literal loopback address). Checked byte-wise, never parsed loosely:
/// any other scheme, host, path, or non-digit port is rejected so a
/// corrupted registry entry can never name a non-loopback or crafted URL.
fn is_loopback_proxy_url(url: &str) -> bool {
    let Some(port) = url.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
}

/// Opens one ready preview's user-facing URL in the system browser — the
/// documented escape hatch from the in-app pane (`docs/live-preview.md`).
///
/// A pure registry read plus a fixed-binary launch, never engine work, so
/// like [`status`] it deliberately takes no lifecycle lock. Refusals come
/// first: the session id must be well-formed, the entry must exist (same
/// actionable error as [`status`]), the state must be `ready`, and a user
/// URL must be recorded. The URL is then re-validated against the exact
/// proxy shape by [`is_loopback_proxy_url`] before [`crate::open_in_browser`]
/// ever sees it — defense in depth: even a corrupted registry entry can
/// never send a non-loopback URL to the browser. On non-macOS targets the
/// helper itself carries the deferred-platform error; nothing is spawned.
pub(crate) async fn open(previews: &Previews, session_id: &str) -> Result<(), String> {
    let status = status(previews, session_id, 0)?;
    if status.state != PreviewState::Ready {
        return Err(format!(
            "session {session_id:?} has no ready preview to open. Start the preview and wait until it reports ready."
        ));
    }
    let url = status.url.ok_or_else(|| {
        format!("the ready preview for session {session_id:?} has no url to open.")
    })?;
    if !is_loopback_proxy_url(&url) {
        return Err(format!(
            "the preview url for session {session_id:?} is not a loopback address and was not opened."
        ));
    }
    crate::open_in_browser(&url).await
}

/// Provisions (idempotently) the session's preview and waits for readiness.
///
/// Preconditions are checked before any engine mutation: the workspace must
/// open and carry a valid host-compatible manifest, and the pinned image must
/// already be present (`--pull=never` keeps provisioning bounded; the
/// generation flow pulled it during `run_project_checks`, and a standalone
/// miss names the exact pull command). Then the three owned objects are
/// ensured (network on demand, labelled data volume, container), a running
/// owned container is adopted rather than recreated, and a stopped one is
/// removed and re-run from the fixed arguments — a saved configuration is
/// never restarted, exactly like installed Pockets. Readiness rides the
/// manifest's own healthcheck through `crate::health`; once healthy, the
/// session's [`preview_proxy`] is spawned in front of the container mapping
/// and the reported `url` is the proxy URL, never the raw container URL. A
/// failed wait (or a proxy that cannot bind its loopback port) leaves the
/// container intact for inspection and lands the entry in `failed`.
#[cfg(test)]
pub(crate) async fn start(
    ai_root: &Path,
    bin: &Path,
    session_id: &str,
    previews: &Previews,
) -> Result<PreviewStatus, String> {
    validate_session_id(session_id)?;
    let workspace = Workspace::open(ai_root, session_id)?;
    start_source(bin, session_id, &workspace, previews, &Default::default()).await
}

/// Shared preview runtime; external projects supply an owned mirror and need
/// no AI session. Callers serialize container operations through Controller.
pub(crate) async fn start_source(
    bin: &Path,
    session_id: &str,
    workspace: &dyn PreviewSource,
    previews: &Previews,
    options: &LaunchOptions,
) -> Result<PreviewStatus, String> {
    validate_session_id(session_id)?;
    let manifest = runnable_manifest(workspace)?;
    let live = if workspace.supervised() {
        previews.reuse_live(session_id)
    } else {
        previews.begin(session_id)
    };
    let outcome = match provision(bin, session_id, workspace, &manifest, options).await {
        Ok(container_url) => {
            let ready = async {
                if workspace.supervised() {
                    let owned = lookup_owned(bin, session_id, true)
                        .await?
                        .ok_or("Preview container is missing.")?;
                    wait_visible(
                        bin,
                        &owned.id,
                        &gate_targets(workspace, &workspace.files())?,
                        VISIBILITY_POLL,
                        VISIBILITY_BUDGET,
                    )
                    .await?;
                    runtime::run(bin, &["restart", "--time", "0", &owned.id]).await?;
                }
                ready_for_source(workspace, &container_url, &manifest.web.healthcheck).await
            }
            .await;
            match ready {
                Ok(()) => {
                    if workspace.supervised() && previews.retarget(session_id, &container_url)? {
                        let url = previews.user_url(session_id);
                        let status =
                            previews.transition(session_id, PreviewState::Ready, url, None);
                        previews.publish_reload(session_id);
                        return Ok(status);
                    }
                    preview_proxy::spawn(&container_url, live)
                        .await
                        .map(|proxy| (container_url, proxy))
                }
                Err(err) => Err(format!("the preview did not become healthy: {err}")),
            }
        }
        Err(err) => Err(err),
    };
    Ok(match outcome {
        Ok((container_url, proxy)) => previews.attach_proxy(session_id, container_url, proxy),
        Err(err) => {
            previews.transition(session_id, PreviewState::Failed, None, Some(err.clone()));
            return Err(err);
        }
    })
}

async fn ready_for_source(
    source: &dyn PreviewSource,
    url: &str,
    health: &str,
) -> Result<(), String> {
    if source.supervised() {
        tokio::time::timeout(Duration::from_secs(4), crate::health::wait_ready(url, health))
            .await.map_err(|_| "The development server did not become ready. Fix its code; preview will retry after the next edit.".to_owned())?
    } else {
        crate::health::wait_ready(url, health).await
    }
}

/// Recreate only this project's owned development container, retaining its
/// disposable data volume and proxy origin. Installed objects never enter it.
pub(crate) async fn reconfigure_source(
    bin: &Path,
    id: &str,
    source: &dyn PreviewSource,
    previews: &Previews,
    options: &LaunchOptions,
) -> Result<PreviewStatus, String> {
    if let Some(owned) = lookup_owned(bin, id, false).await? {
        if owned.running {
            runtime::run(bin, &["stop", "--time", "0", &owned.id]).await?;
        }
        runtime::run(bin, &["rm", &owned.id]).await?;
    }
    start_source(bin, id, source, previews, options).await
}

/// Restarts the running preview so it picks up the given workspace files.
///
/// Controller-driven by construction: this is called natively when a
/// generation turn writes files, never by the model. The order is the safety
/// story from `spikes/runtime/RESULTS.md`: FIRST gate every changed path on
/// container visibility (bounded [`VISIBILITY_BUDGET`], never sleeping past
/// it; a path present on the host is hash-compared, a path already gone —
/// a restore's removal — must read absent inside the container too); THEN,
/// unless every path is a stylesheet — the hot-reload contract
/// serves fresh static CSS without touching a healthy server — transition
/// `updating` and run `restart --time 0` (plain restart pays the 10.2 s
/// SIGTERM tax; `-t 0` measured 0.187 s); THEN re-probe the manifest
/// healthcheck. Failures are reported by stage: a refusal BEFORE any restart
/// was attempted — the lookup, manifest validation, or the visibility gate
/// timing out (restarting an unpropagated rename edit reproduces the spike's
/// `MODULE_NOT_FOUND` crash) — records its actionable text as an event and
/// returns the error while the entry stays `ready` with its URL, because the
/// old process is untouched and a later turn can refresh again. Only a
/// failure after the restart actually ran lands `failed`. The name, port
/// mapping, and data volume survive the restart, so the entry keeps its URL
/// across `updating`.
///
/// The engine entry point for a restart cycle. Not a command surface: its
/// only caller is [`refresh_if_running`], the gated helper the agent turn
/// loop drives — callers must go through it so the ready-state policy and
/// the shared lifecycle lock apply, never straight here.
#[cfg(test)]
pub(crate) async fn refresh(
    ai_root: &Path,
    bin: &Path,
    session_id: &str,
    changed_files: &[String],
    previews: &Previews,
) -> Result<PreviewStatus, String> {
    refresh_with(
        ai_root,
        bin,
        session_id,
        changed_files,
        previews,
        VISIBILITY_POLL,
        VISIBILITY_BUDGET,
    )
    .await
}

/// Hot-reload entry point for the agent turn loop: called natively once per
/// model turn with the workspace-relative paths that turn wrote, so a running
/// preview picks them up through its existing read-only bind mount — no image
/// rebuild, no re-provision, and nothing from the frontend re-enters the call.
///
/// Deliberately narrower than [`refresh`]: this is policy plus the lifecycle
/// lock, never a preview start in disguise.
///
/// * An empty change set returns `Ok(())` before anything is touched; a turn
///   that wrote nothing never restarts a container or queues on the lock.
/// * A malformed session id is refused like every other boundary here.
/// * Only a `ready` preview is refreshed. No entry, or `preparing`,
///   `updating`, `failed`, or `stopped` answers a silent `Ok(())`: refresh
///   must never create or start a preview implicitly, a `preparing` start
///   owns its own provisioning, `updating` already has a restart in flight,
///   and a `failed` or `stopped` session is a user decision, not something a
///   background turn resurrects. [`refresh_requested`] is that gate.
/// * The gate and the restart run under the shared [`Controller`] lifecycle
///   lock — the same one `ai_preview_start` and `ai_preview_stop` hold — so
///   a stop can never interleave between "is it ready?" and the restart.
///
/// Everything else is the existing [`refresh`] contract: the trusted AI root
/// and Podman binary resolve through the existing native helpers, the changed
/// files gate on container visibility before `restart --time 0` (a
/// stylesheet-only set stops at the gate and only publishes), a successful
/// cycle publishes the paths on the proxy SSE for style/morph/reload, and a
/// pre-restart refusal returns actionable text while the ready preview keeps
/// serving, untouched. A failure is returned so the turn loop can surface it;
/// deciding to ignore it stays the caller's call.
#[cfg(test)]
fn refresh_requested(previews: &Previews, session_id: &str, changed_files: &[String]) -> bool {
    !changed_files.is_empty()
        && previews
            .status(session_id, u64::MAX)
            .is_some_and(|snapshot| snapshot.state == PreviewState::Ready)
}

/// [`refresh`] with the gate cadence injected so unit tests drive the
/// bounded loop with millisecond budgets instead of sleeping five seconds.
#[cfg(test)]
async fn refresh_with(
    ai_root: &Path,
    bin: &Path,
    session_id: &str,
    changed_files: &[String],
    previews: &Previews,
    poll: Duration,
    budget: Duration,
) -> Result<PreviewStatus, String> {
    validate_session_id(session_id)?;
    let no_preview =
        || format!("session {session_id:?} has no preview to refresh. Start the preview first.");
    if !previews.contains(session_id) {
        return Err(no_preview());
    }
    let outcome = async {
        let workspace = Workspace::open(ai_root, session_id).map_err(RefreshFailure::Preflight)?;
        let manifest = runnable_manifest(&workspace).map_err(RefreshFailure::Preflight)?;
        refresh_cycle(
            previews,
            bin,
            session_id,
            &workspace,
            &manifest,
            changed_files,
            poll,
            budget,
        )
        .await
    }
    .await;
    finish_refresh(previews, session_id, changed_files, outcome, false)
}

pub(crate) async fn refresh_source(
    bin: &Path,
    id: &str,
    source: &dyn PreviewSource,
    paths: &[String],
    previews: &Previews,
    options: &LaunchOptions,
) -> Result<PreviewStatus, String> {
    validate_session_id(id)?;
    if previews
        .status(id, 0)
        .is_some_and(|status| status.state == PreviewState::Stopped)
    {
        return Err("Start the preview before refreshing it.".into());
    }
    if previews.user_url(id).is_none() {
        return start_source(bin, id, source, previews, options).await;
    }
    let manifest = runnable_manifest(source)?;
    let outcome = refresh_cycle(
        previews,
        bin,
        id,
        source,
        &manifest,
        paths,
        VISIBILITY_POLL,
        VISIBILITY_BUDGET,
    )
    .await;
    finish_refresh(previews, id, paths, outcome, source.supervised())
}

fn finish_refresh(
    previews: &Previews,
    session_id: &str,
    changed_files: &[String],
    outcome: Result<RefreshOutcome, RefreshFailure>,
    reload_server: bool,
) -> Result<PreviewStatus, String> {
    let no_preview = || format!("Preview {session_id} is not registered.");
    Ok(match outcome {
        Ok(RefreshOutcome::StyleOnly) => {
            // The gate proved the container already reads every changed
            // stylesheet and static CSS is served fresh off the read-only
            // mount: no restart, no health re-probe, and no transition — the
            // entry never left `ready` and the proxy URL never moved. The
            // paths publish now, and the proxy renders them as style frames.
            previews.publish_changed(session_id, changed_files.to_vec());
            previews
                .status(session_id, u64::MAX)
                .ok_or_else(no_preview)?
        }
        Ok(RefreshOutcome::Restarted) => {
            // The restart keeps the container mapping and the entry keeps its
            // user-facing proxy URL (the pane URL never churns); the changed
            // paths publish only now, once the restarted app is healthy, and
            // the proxy tiers them into style/morph/reload frames.
            let user_url = previews.user_url(session_id);
            let status = previews.transition(session_id, PreviewState::Ready, user_url, None);
            if reload_server {
                previews.publish_reload(session_id);
            } else {
                previews.publish_changed(session_id, changed_files.to_vec());
            }
            status
        }
        Err(RefreshFailure::Preflight(err)) => {
            // Nothing ran: keep the live preview exactly as it is — state,
            // URL, proxy — and record why this turn's refresh was refused.
            // The next turn can try again with the same command.
            previews.note_refusal(session_id, &err);
            return Err(err);
        }
        Err(RefreshFailure::PostRestart(err)) => {
            // The restart was attempted, so the old process is gone and
            // `failed` is the truth. Keep the proxy origin and the page
            // already displayed in the native view; do not navigate that
            // view to an error page or replace it with a blank pane.
            previews.transition(session_id, PreviewState::Failed, None, Some(err.clone()));
            return Err(err);
        }
    })
}

/// What a completed refresh cycle did to the running preview.
enum RefreshOutcome {
    /// Every changed path is a [`preview_proxy::Tier::Style`] stylesheet and
    /// the visibility gate proved the container already reads the new bytes:
    /// static CSS is served fresh from the read-only mount on the next
    /// request, so a healthy server keeps running — no restart, no health
    /// re-probe, no state change.
    StyleOnly,
    /// The restart ran against the inspected ID and the restarted app
    /// re-probed healthy through the manifest healthcheck.
    Restarted,
}

/// Why a refresh cycle failed, tagged by the stage that failed so the entry
/// can stay truthful about the old process.
enum RefreshFailure {
    /// Refused before any restart was attempted: the lookup, manifest
    /// validation, or the visibility gate. The container was never touched,
    /// the proxy still serves the previous page, so the entry must not claim
    /// `failed` — it stays as it was and a later turn can refresh again.
    Preflight(String),
    /// The restart was attempted and something after it failed — the restart
    /// command itself or the health re-probe. The old process is gone, so
    /// `failed` is truthful; the proxy and last-good page survive untouched.
    PostRestart(String),
}

/// One gate → restart → probe cycle. The failure stage decides what the
/// entry may truthfully claim afterwards (see [`RefreshFailure`]); the
/// messages are actionable either way.
async fn refresh_cycle(
    previews: &Previews,
    bin: &Path,
    session_id: &str,
    workspace: &dyn PreviewSource,
    manifest: &Manifest,
    changed_files: &[String],
    poll: Duration,
    budget: Duration,
) -> Result<RefreshOutcome, RefreshFailure> {
    // The restart keeps the container's port mapping (spike 1: "name, port
    // mapping, and volume survive"), so one lookup feeds both the exec gate
    // and the post-restart probe.
    let owned = lookup_owned(bin, session_id, true)
        .await
        .map_err(RefreshFailure::Preflight)?
        .ok_or_else(|| {
            RefreshFailure::Preflight(format!(
                "session {session_id:?} has no preview container to refresh."
            ))
        })?;
    let url = owned.url.clone().ok_or_else(|| {
        RefreshFailure::Preflight(
            "the preview container is running without a loopback mapping".to_owned(),
        )
    })?;

    let targets = gate_targets(workspace, changed_files).map_err(RefreshFailure::Preflight)?;
    wait_visible(bin, &owned.id, &targets, poll, budget)
        .await
        .map_err(RefreshFailure::Preflight)?;

    // The hot-reload contract (docs/live-preview.md): when every changed
    // path is a stylesheet, the just-passed gate means the container already
    // reads the new bytes, and static CSS is served fresh off the read-only
    // mount — restarting would only interrupt a healthy server. An empty
    // change set still restarts: `messages_for` treats it as "something
    // changed but nothing specific was reported" and morphs, which needs the
    // server-side re-render only a restart provides.
    let style_only = !changed_files.is_empty()
        && changed_files
            .iter()
            .all(|path| preview_proxy::tier_of(path) == preview_proxy::Tier::Style);
    if style_only
        && (!workspace.supervised()
            || previews
                .status(session_id, 0)
                .is_some_and(|status| status.state == PreviewState::Ready))
    {
        return Ok(RefreshOutcome::StyleOnly);
    }

    // Preflight and the gate are behind us and the restart is the very next
    // command: only now does the entry earn `updating`, so a refusal above
    // never painted a still-serving preview as restarting.
    previews.transition(session_id, PreviewState::Updating, None, None);
    runtime::run(bin, &["restart", "--time", "0", &owned.id])
        .await
        .map_err(RefreshFailure::PostRestart)?;
    ready_for_source(workspace, &url, &manifest.web.healthcheck)
        .await
        .map_err(|err| {
            RefreshFailure::PostRestart(format!(
                "the restarted preview did not become healthy: {err}"
            ))
        })?;
    Ok(RefreshOutcome::Restarted)
}

/// Host file + container path pairs the visibility gate compares, one per
/// changed workspace file. Paths go through the workspace's own resolver, so
/// an escape attempt is refused before anything reaches `podman exec`.
///
/// A path already gone from the host — the removal side of a history
/// restore — is accepted as an expected-removed target: the gate then proves
/// the container has forgotten it too. Anything the host reports that is a
/// directory, symlink, or special file stays refused (the command layer
/// replaces a former-file-turned-directory with the restored files under
/// it, never the gate).
fn gate_targets(
    workspace: &dyn PreviewSource,
    changed_files: &[String],
) -> Result<Vec<(String, PathBuf, String)>, String> {
    let mut targets = Vec::with_capacity(changed_files.len());
    for rel in changed_files {
        let host = workspace.resolve(rel)?;
        match std::fs::symlink_metadata(&host) {
            Ok(meta) if meta.is_file() => {}
            // The removal side of a restore: nothing left to hash on the
            // host, so the gate must confirm the container's absence of it.
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(format!("the changed path {rel:?} is not a regular file"));
            }
            Err(err) => {
                return Err(format!(
                    "the changed file {rel:?} could not be read on the host: {err}"
                ));
            }
        }
        targets.push((rel.clone(), host, format!("{CONTAINER_WORKDIR}/{rel}")));
    }
    Ok(targets)
}

/// Which kind of propagation a gate target is still waiting on, so an
/// expired budget can name the path and say truthfully what never happened
/// — a lagging write, a lagging removal, or a host path that stopped being
/// the regular file preflight validated.
enum Pending<'a> {
    Edit(&'a str),
    Removal(&'a str),
    NotAFile(&'a str),
}

/// Bounded container-visibility gate: no restart may run until EVERY
/// changed path reads on the container exactly as the host reports it, or
/// the budget expires. The host is re-inspected every round:
///
/// * regular host file → the existing SHA-256 compare against
///   `podman exec <container> sha256sum <path>`;
/// * host `NotFound` → an expected removal: the container must confirm the
///   absence through `podman exec <container> test ! -e <path>` — fixed
///   argv, no shell, and the container path built only from the validated
///   workspace-relative path, never a frontend one;
/// * anything else on the host (a symlink or special file swapped in after
///   preflight) is never hashed or followed; it simply keeps the gate shut.
///
/// Transient failures inside the window are *not* errors — the spike logged
/// `No such file or directory` from inside exactly this propagation window —
/// they keep the loop polling until the deadline. A host file that cannot be
/// hashed, or a container file missing while the host still has one, is
/// likewise treated as "not visible yet" and re-checked next round. An empty
/// target set passes immediately, as before.
async fn wait_visible(
    bin: &Path,
    container_id: &str,
    targets: &[(String, PathBuf, String)],
    poll: Duration,
    budget: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let mut pending: Option<Pending<'_>> = None;
        for (rel, host, container_path) in targets {
            match std::fs::symlink_metadata(host) {
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    // Expected removal: the container still seeing the file
                    // is the same propagation lag a stale hash is for a
                    // write, so a failed `test` keeps the loop polling.
                    let gone = runtime::run(
                        bin,
                        &["exec", container_id, "test", "!", "-e", container_path],
                    )
                    .await
                    .is_ok();
                    if !gone {
                        pending = Some(Pending::Removal(rel));
                        break;
                    }
                }
                Err(_) => {
                    pending = Some(Pending::Edit(rel));
                    break;
                }
                Ok(meta) if meta.is_file() => {
                    let host_hash = match sha256_hex(host) {
                        Ok(hex) => hex,
                        Err(_) => {
                            pending = Some(Pending::Edit(rel));
                            break;
                        }
                    };
                    let reported =
                        runtime::run(bin, &["exec", container_id, "sha256sum", container_path])
                            .await
                            .ok()
                            .and_then(|output| first_hash_token(&output.stdout));
                    if reported.as_deref() != Some(host_hash.as_str()) {
                        pending = Some(Pending::Edit(rel));
                        break;
                    }
                }
                // Swapped for a symlink, directory, or special file after
                // preflight: refuse to hash or follow it, and let the gate
                // expire rather than restart onto it.
                Ok(_) => {
                    pending = Some(Pending::NotAFile(rel));
                    break;
                }
            }
        }
        let Some(waiting) = pending else {
            return Ok(());
        };
        if tokio::time::Instant::now() >= deadline {
            let secs = budget.as_secs();
            return Err(match waiting {
                Pending::Edit(rel) => format!(
                    "the edit to {rel:?} never became visible inside the preview container within \
                     {secs} seconds, so the preview was not restarted. Restarting before a host \
                     edit propagates through the Podman machine mount crashes the app; try \
                     refreshing again, and if it keeps failing, stop the preview and start it \
                     again."
                ),
                Pending::Removal(rel) => format!(
                    "the removal of {rel:?} never became visible inside the preview container \
                     within {secs} seconds, so the preview was not restarted. Restarting before a \
                     host removal propagates through the Podman machine mount can crash the app \
                     against the deleted file; try refreshing again, and if it keeps failing, \
                     stop the preview and start it again."
                ),
                Pending::NotAFile(rel) => format!(
                    "the changed path {rel:?} is not a regular file on the host anymore, so the \
                     preview was not restarted. It is never hashed or followed; try refreshing \
                     again, and if it keeps failing, stop the preview and start it again."
                ),
            });
        }
        tokio::time::sleep(poll).await;
    }
}

/// The 64-hex digest of a file, the same value `sha256sum` prints (lowercase).
fn sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|err| format!("{} could not be read: {err}", path.display()))?;
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Ok(hex)
}

/// First whitespace-delimited token of `sha256sum` output: the digest, with
/// the `<hash>  <path>` (or binary `<hash> *<path>`) tail discarded.
fn first_hash_token(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

/// Read the last output from an owned preview, bounded in size and time.
pub(crate) async fn logs(bin: &Path, id: &str) -> Result<String, String> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    let Some(container) = lookup_owned(bin, id, false).await? else {
        return Ok(String::new());
    };
    let mut child = tokio::process::Command::new(bin)
        .args(["logs", "--tail", "100", &container.id])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Could not read preview logs: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("Preview log output is missing.")?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or("Preview log errors are missing.")?;
    let collect = async {
        let mut log = crate::builder::BoundedLog::default();
        let mut out = [0; 4096];
        let mut err = [0; 4096];
        let mut reading_out = true;
        let mut reading_err = true;
        while reading_out || reading_err {
            tokio::select! {
                read = stdout.read(&mut out), if reading_out => {
                    let count = read?;
                    reading_out = count > 0;
                    log.extend(&out[..count]);
                }
                read = stderr.read(&mut err), if reading_err => {
                    let count = read?;
                    reading_err = count > 0;
                    log.extend(&err[..count]);
                }
            }
        }
        Ok::<_, std::io::Error>((child.wait().await?, log))
    };
    let (status, log) = tokio::time::timeout(Duration::from_secs(5), collect)
        .await
        .map_err(|_| "Reading preview logs timed out.")?
        .map_err(|e| format!("Could not read preview logs: {e}"))?;
    if !status.success() {
        return Err("The container runtime could not read the preview logs.".into());
    }
    Ok(log
        .text()
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect())
}

/// Stops and removes the session's preview, then tears down shared objects
/// when nothing is left.
///
/// Answers `false` (not an error) when the session never had a preview. The
/// container is stopped with `--time 0` — the spike measured a steady 10.2 s
/// for a default stop because PID-1 Node ignores SIGTERM, and a preview
/// process is disposable by design — then removed by its inspected ID, never
/// its name. The labelled data volume goes with it (disposable by contract:
/// preview data is never promoted and never shared with an installed app).
/// The shared network is removed only when no active preview remains in the
/// registry, and an engine refusal (a foreign container attached, or already
/// absent) is expected and tolerated, never fatal to the stop.
pub(crate) async fn stop(
    bin: &Path,
    session_id: &str,
    previews: &Previews,
) -> Result<bool, String> {
    validate_session_id(session_id)?;
    let had_entry = previews.contains(session_id);

    let existing = lookup_owned(bin, session_id, false).await?;
    if let Some(container) = existing {
        if container.running {
            runtime::run(bin, &["stop", "--time", "0", &container.id]).await?;
        }
        runtime::run(bin, &["rm", &container.id]).await?;
    }
    preview_egress::stop(bin, session_id).await?;
    let scope =
        crate::runtime_env::Scope::new(session_id, crate::runtime_settings::Environment::Preview)?;
    crate::runtime_env::clear(bin, &scope).await?;
    remove_data_volume(bin, session_id).await?;

    // A stopped entry with nothing left in the engine is already `stopped`:
    // a repeated stop must not manufacture a second transition.
    let already_stopped = previews
        .status(session_id, u64::MAX)
        .is_some_and(|snapshot| snapshot.state == PreviewState::Stopped);
    if had_entry && !already_stopped {
        previews.transition(session_id, PreviewState::Stopped, None, None);
    }
    // Shut the proxy listener down with the preview: a stopped session must
    // not keep serving (or holding) a loopback port. Detaching the fanout
    // sender ends every open SSE stream, so the browser's EventSource
    // finishes instead of reconnecting against a dead upstream. (The launch
    // sweep never sees an in-process proxy: previews do not survive a
    // Desktop restart, so `sweep_orphans` runs before any listener exists.)
    if let Some(proxy) = previews.detach_proxy(session_id) {
        proxy.stop();
    }
    if previews.active_count() == 0 {
        // Best effort only: `in use by container` means someone else's
        // container (never a Pockets preview) is attached, and absence means
        // the sweep already got here first. Neither is a stop failure.
        let _ = runtime::run(bin, &["network", "rm", PREVIEW_NETWORK]).await;
    }
    Ok(had_entry)
}

/// Reclaims stale owned preview objects left by a Desktop quit or crash.
///
/// Previews never survive a restart (docs/live-preview.md), so anything
/// matching the owned spelling is Pockets' own garbage. Selection is exact,
/// never a prefix guess: a container is removed only when its reported name is
/// `pockets-preview-<32 hex>` **and** its inspected identity confirms the
/// matching `dev.pockets.managed=preview-<same id>` label; a volume only when
/// spelled `pockets-preview-<32 hex>-data` with the matching label. Anything
/// else under the prefix — a foreign squat on the owned name included — is
/// left untouched, there is never a prune, and installed
/// `pockets-project-*`/`pockets-hello-pocket` objects cannot match the filter.
/// Returns problems to report; a launch never fails because of them.
pub(crate) async fn sweep_orphans(
    bin: &Path,
    protected: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut removed_any = false;

    let name_filter = format!("name=^{PREVIEW_NAME_PREFIX}");
    let output = match runtime::run(
        bin,
        &["ps", "--all", "--filter", &name_filter, "--format", "json"],
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            problems.push(format!(
                "preview container sweep could not list containers: {err}"
            ));
            return problems;
        }
    };
    let entries: Vec<PsEntry> = match serde_json::from_slice(&output.stdout) {
        Ok(entries) => entries,
        Err(err) => {
            problems.push(format!("could not parse `podman ps` output: {err}"));
            return problems;
        }
    };
    for entry in &entries {
        // Cheap shape gate first: only a row whose name completes the owned
        // spelling earns an inspect at all.
        let Some(session_key) = entries_owned_key(entry) else {
            continue;
        };
        if protected.contains(session_key) {
            continue;
        }
        let name = format!("{PREVIEW_NAME_PREFIX}{session_key}");
        match owned_container(bin, &entry.id, &name).await {
            Err(err) => problems.push(format!("preview container sweep: {err}")),
            Ok(None) => {}
            Ok(Some(container)) => {
                // Mutations act on the ps-reported, safety-checked ID, the
                // same identity rule every other lifecycle call here uses.
                if container.running {
                    if let Err(err) = runtime::run(bin, &["stop", "--time", "0", &entry.id]).await {
                        problems.push(format!("stop preview container {}: {err}", entry.id));
                        continue;
                    }
                }
                if let Err(err) = runtime::run(bin, &["rm", &entry.id]).await {
                    problems.push(format!("remove preview container {}: {err}", entry.id));
                } else {
                    removed_any = true;
                }
            }
        }
    }

    let volume_filter = format!("name=^{PREVIEW_NAME_PREFIX}");
    let output = match runtime::run(
        bin,
        &[
            "volume",
            "ls",
            "--filter",
            &volume_filter,
            "--format",
            "json",
        ],
    )
    .await
    {
        Ok(output) => output,
        Err(err) => {
            problems.push(format!(
                "preview volume sweep could not list volumes: {err}"
            ));
            return problems;
        }
    };
    let entries: Vec<VolumeListEntry> = match serde_json::from_slice(&output.stdout) {
        Ok(entries) => entries,
        Err(err) => {
            problems.push(format!("could not parse `podman volume ls` output: {err}"));
            return problems;
        }
    };
    for entry in &entries {
        let Some(session_key) = owned_volume_key(&entry.name) else {
            continue;
        };
        if protected.contains(session_key) {
            continue;
        }
        match owned_volume(bin, &entry.name, session_key).await {
            Err(err) => problems.push(format!("preview volume sweep: {err}")),
            Ok(false) => {}
            // The proof passed, so the volume is ours and still present —
            // the sweep is exactly where it dies. `remove_data_volume` is
            // the stop path's prover+remover; here the proof is one call
            // away, so the removal stands on its own.
            Ok(true) => {
                if let Err(err) = runtime::run(bin, &["volume", "rm", &entry.name]).await {
                    problems.push(format!("remove preview volume {}: {err}", entry.name));
                } else {
                    removed_any = true;
                }
            }
        }
    }

    // The shared network is only reclaimed when this launch actually found
    // Pockets-owned garbage under the owned spelling: an untouched engine must
    // never have a foreign `pockets-preview` network deleted. An "in use"
    // refusal means a container we do not own is attached — leave it and say
    // so; the network is inert without a Pockets container publishing into it.
    if removed_any {
        if let Err(err) = runtime::run(bin, &["network", "rm", PREVIEW_NETWORK]).await {
            problems.push(format!(
                "preview network {PREVIEW_NETWORK} was left in place: {err}"
            ));
        }
    }
    problems
}

/// Launch entry for the orphan sweep: finds the engine, takes the shared
/// lifecycle lock (a preview started before the sweep finished is serialized
/// behind it, never raced), and reports problems to the caller. A missing
/// Podman means no engine state exists to reclaim, so silence is correct.
pub(crate) async fn sweep_at_launch(ctx: &crate::AppContext) -> Vec<String> {
    let Some(bin) = runtime::find_podman() else {
        return Vec::new();
    };
    let _serializing = ctx.controller.lifecycle_guard().await;
    // A user may start a project before this asynchronous startup task gets
    // the lifecycle lock. Never sweep a preview registered by this process.
    let protected = ctx
        .previews
        .lock()
        .iter()
        .filter(|(_, entry)| entry.state != PreviewState::Stopped)
        .map(|(id, _)| id.clone())
        .collect();
    let mut problems = sweep_orphans(&bin, &protected).await;
    problems.extend(preview_egress::sweep(&bin, &protected).await);
    problems.extend(crate::runtime_env::sweep_preview_orphans(&bin).await);
    problems
}

/// Ensures the network and volume, adopts or recreates the container, and
/// returns its validated loopback URL.
async fn provision(
    bin: &Path,
    session_id: &str,
    workspace: &dyn PreviewSource,
    manifest: &Manifest,
    options: &LaunchOptions,
) -> Result<String, String> {
    if runtime::run(bin, &["image", "exists", PREVIEW_IMAGE])
        .await
        .is_err()
    {
        return Err(format!(
            "the preview image {PREVIEW_IMAGE} is not present. Pull it with \
             `podman pull {PREVIEW_IMAGE}`, \
             then start the preview again."
        ));
    }

    if options.allowed_hosts.is_empty() {
        ensure_network(bin).await?;
    }
    ensure_data_volume(bin, session_id).await?;

    let existing = lookup_owned(bin, session_id, true).await?;
    if existing.as_ref().is_some_and(|container| container.running) {
        // Idempotent re-entry: the owned container already serves; adopting
        // it is the whole point of `start` being safe to repeat.
        let url = existing
            .as_ref()
            .and_then(|container| container.url.clone())
            .ok_or_else(|| {
                "the preview container is running without a loopback mapping".to_owned()
            })?;
        return Ok(url);
    }
    if let Some(container) = existing {
        runtime::run(bin, &["rm", &container.id]).await?;
    }

    let gateway = if workspace.supervised() {
        preview_egress::stop(bin, session_id).await?;
        if options.allowed_hosts.is_empty() {
            None
        } else {
            Some(
                preview_egress::start(bin, session_id, &options.allowed_hosts, PREVIEW_IMAGE)
                    .await?,
            )
        }
    } else {
        None
    };
    let network = gateway
        .as_ref()
        .map_or(PREVIEW_NETWORK, |gateway| gateway.network.as_str());
    let mut args = preview_run_args(session_id, workspace.root(), manifest, network);
    if let Some(gateway) = &gateway {
        args.insert(1, format!("--env=POCKETS_HTTPS_GATEWAY={}", gateway.url));
    }
    if workspace.supervised() {
        // Keep the trusted parent alive when generated server code crashes.
        // The visibility gate can then inspect files before retrying an edit.
        args.pop();
        args.push("-e".into());
        args.push("const {spawn}=require('node:child_process'); const child=spawn(process.execPath,['server.js'],{stdio:'inherit'}); child.on('error',e=>console.error(e.message)); setInterval(()=>{},1000);".into());
    }
    let scope =
        crate::runtime_env::Scope::new(session_id, crate::runtime_settings::Environment::Preview)?;
    let started = async {
        let bindings = if workspace.supervised() {
            crate::runtime_env::clear(bin, &scope).await?;
            crate::runtime_env::prepare(bin, &scope, &options.variables).await?
        } else {
            Default::default()
        };
        bindings.run(bin, args).await
    }
    .await;
    if let Err(error) = started {
        if matches!(lookup_owned(bin, session_id, false).await, Ok(None)) {
            if gateway.is_some() {
                if let Err(cleanup) = preview_egress::stop(bin, session_id).await {
                    return Err(format!("{error} {cleanup}"));
                }
            }
            if !options.variables.is_empty() {
                if let Err(cleanup) = crate::runtime_env::clear(bin, &scope).await {
                    return Err(format!("{error} {cleanup}"));
                }
            }
        }
        return Err(with_pull_hint(error));
    }

    // Reconcile from Podman rather than trusting the run invocation, exactly
    // like `pocket::start_target`.
    let owned = lookup_owned(bin, session_id, true)
        .await?
        .ok_or_else(|| "the preview container vanished right after creation".to_owned())?;
    owned
        .url
        .clone()
        .ok_or_else(|| "the preview container is running without a loopback mapping".to_owned())
}

/// Creates the shared internal network, tolerating the already-exists race
/// (another start, or a previous run that left it — the launch sweep removes
/// it only alongside owned garbage).
async fn ensure_network(bin: &Path) -> Result<(), String> {
    match runtime::run(bin, &["network", "create", "--internal", PREVIEW_NETWORK]).await {
        Ok(_) => Ok(()),
        Err(err) if err.to_lowercase().contains("already exists") => {
            let inspected = runtime::run(
                bin,
                &[
                    "network",
                    "inspect",
                    "--format",
                    "{{.Name}} {{.Driver}} {{.Internal}}",
                    PREVIEW_NETWORK,
                ],
            )
            .await?;
            if String::from_utf8_lossy(&inspected.stdout).trim()
                != format!("{PREVIEW_NETWORK} bridge true")
            {
                return Err(format!("Podman network {PREVIEW_NETWORK} must be an internal bridge before Pockets can start a preview."));
            }
            Ok(())
        }
        Err(err) => Err(format!(
            "the preview network {PREVIEW_NETWORK} could not be created: {err}"
        )),
    }
}

/// Creates the session's labelled data volume, refusing a foreign volume
/// claiming the owned name (never adopted, never removed here — `stop` owns
/// removal). Mirrors `pocket::ensure_data_volume` for the preview spelling.
async fn ensure_data_volume(bin: &Path, session_id: &str) -> Result<(), String> {
    let volume = data_volume_name(session_id);
    let exists = {
        let name_filter = format!("name=^{volume}$");
        let output = runtime::run(
            bin,
            &["volume", "ls", "--filter", &name_filter, "--format", "json"],
        )
        .await?;
        let entries: Vec<VolumeListEntry> = serde_json::from_slice(&output.stdout)
            .map_err(|err| format!("could not parse `podman volume ls` output: {err}"))?;
        entries.iter().any(|entry| entry.name == volume)
    };
    if exists {
        owned_volume(bin, &volume, session_id).await?;
        return Ok(());
    }
    runtime::run(
        bin,
        &[
            "volume",
            "create",
            "--label",
            &ownership_label_arg(session_id),
            &volume,
        ],
    )
    .await
    .map(|_| ())
}

/// Removes the session's data volume after the same label proof; an absent
/// volume is success, a foreign one is a refusal.
async fn remove_data_volume(bin: &Path, session_id: &str) -> Result<(), String> {
    let volume = data_volume_name(session_id);
    if owned_volume(bin, &volume, session_id).await? {
        runtime::run(bin, &["volume", "rm", &volume]).await?;
    }
    Ok(())
}

/// Whether the named volume exists and is Pockets' preview volume for this
/// session. `Ok(false)` means it does not exist at all; a volume under the
/// owned name without this session's ownership label is a refusal, exactly
/// the `pocket::volume_exists` discipline, so `stop` can never destroy an
/// installed app's or a stranger's data.
async fn owned_volume(bin: &Path, volume: &str, session_id: &str) -> Result<bool, String> {
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
    let output = runtime::run(bin, &["volume", "inspect", volume]).await?;
    let entries: Vec<VolumeInspectEntry> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse `podman volume inspect` output: {err}"))?;
    let [entry] = entries.as_slice() else {
        return Err(format!(
            "`podman volume inspect` returned {} results for {volume}, expected exactly one",
            entries.len()
        ));
    };
    let ownership = format!("{PREVIEW_OWNERSHIP_PREFIX}{session_id}");
    let owned = entry
        .labels
        .as_ref()
        .and_then(|labels| labels.get(pocket::OWNERSHIP_LABEL))
        .is_some_and(|value| *value == ownership);
    if !owned {
        return Err(format!(
            "volume {volume} exists but is not managed by Pockets (expected label {}={ownership}); \
             refusing to touch it",
            pocket::OWNERSHIP_LABEL
        ));
    }
    Ok(true)
}

/// The session's owned container, or `None` when absent. Name filter plus
/// exact inspected identity (name and ownership label) mirror
/// `pocket::lookup_owned`; the returned ID is the only identity later
/// `stop`/`rm`/`exec`/`restart` calls ever use.
async fn lookup_owned(
    bin: &Path,
    session_id: &str,
    require_mapping: bool,
) -> Result<Option<OwnedPreview>, String> {
    let name = container_name(session_id);
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
    if !pocket::is_safe_container_id(&entry.id) {
        return Err(format!(
            "`podman ps` reported an unusable container id for {name}"
        ));
    }
    let container = inspect_container(bin, &entry.id).await?;
    let running = container.running;
    let owned = container.ownership.as_deref()
        == Some(format!("{PREVIEW_OWNERSHIP_PREFIX}{session_id}").as_str());
    if !owned || container.name != name {
        return Err(format!(
            "container {name} exists but is not this session's Pockets preview \
             (expected label {}={PREVIEW_OWNERSHIP_PREFIX}{session_id}); refusing to control it",
            pocket::OWNERSHIP_LABEL
        ));
    }
    let require = |value: Option<u16>| -> Result<Option<String>, String> {
        match value {
            Some(port) => Ok(Some(format!("http://{LOOPBACK_IP}:{port}"))),
            None if running && require_mapping => Err(format!(
                "container {name} is running but publishes no single {LOOPBACK_IP} mapping; \
                 refusing to guess an address"
            )),
            None => Ok(None),
        }
    };
    let url = require(container.loopback_port)?;
    Ok(Some(OwnedPreview {
        id: entry.id.clone(),
        running,
        url,
    }))
}

/// The inspected identity of one container by ID: exact name, ownership
/// label, running flag, and the sole safe loopback mapping of the container
/// port — the `pocket.rs` inspect discipline on raw JSON.
async fn inspect_container(bin: &Path, id: &str) -> Result<InspectedPreview, String> {
    let output = runtime::run(bin, &["inspect", id]).await?;
    let entries: Vec<InspectEntry> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse `podman inspect` output: {err}"))?;
    let [entry] = entries.as_slice() else {
        return Err(format!(
            "`podman inspect` returned {} results for {id}, expected exactly one",
            entries.len()
        ));
    };
    let name = entry
        .name
        .strip_prefix('/')
        .unwrap_or(&entry.name)
        .to_owned();
    let running = entry.state.as_ref().is_some_and(|state| state.running);
    let ownership = entry
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get(pocket::OWNERSHIP_LABEL))
        .cloned();
    Ok(InspectedPreview {
        name,
        running,
        ownership,
        loopback_port: loopback_port(
            entry
                .network_settings
                .as_ref()
                .and_then(|settings| settings.ports.as_ref()),
        ),
    })
}

/// Exactly one binding, `HostIp` equal to [`LOOPBACK_IP`], and a nonzero
/// parseable `HostPort` — anything else is `None`, never a guess (the rule
/// `pocket::loopback_port` enforces for installed Pockets).
fn loopback_port(ports: Option<&HashMap<String, Option<Vec<PortBinding>>>>) -> Option<u16> {
    let mut bindings: Vec<&PortBinding> = Vec::new();
    for list in ports?.values().filter_map(Option::as_ref) {
        bindings.extend(list.iter());
    }
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

/// The fixed `podman run` invocation for one preview: detached, internal
/// network, loopback-only engine-allocated publishing on the manifest port,
/// bounded memory, all capabilities dropped, no privilege escalation, the
/// workspace read-only at `/work`, and the disposable labelled volume at the
/// manifest storage path. `HOST`/`PORT`/`DATA_DIR` are the only environment a
/// starter app may read (docs/ai-connector.md contract), so the fixed values
/// bind the app to the manifest's own port and to the preview volume. The
/// command is the pinned image running `node server.js` with **no watch
/// flags**: Node 24 removed `--watch-poll` and in-container watch is deaf
/// through the mount (`spikes/runtime/RESULTS.md`); reloads are controller
/// restarts. Never privileged, never host networking, never a host path
/// besides the read-only workspace, never an engine socket, never a shell.
fn preview_run_args(
    session_id: &str,
    workspace_root: &Path,
    manifest: &Manifest,
    network: &str,
) -> Vec<String> {
    vec![
        "run".to_owned(),
        "--detach".to_owned(),
        "--name".to_owned(),
        container_name(session_id),
        "--label".to_owned(),
        ownership_label_arg(session_id),
        "--pull=never".to_owned(),
        "--network".to_owned(),
        network.to_owned(),
        "--memory".to_owned(),
        PREVIEW_MEMORY.to_owned(),
        "--cap-drop=ALL".to_owned(),
        "--security-opt=no-new-privileges".to_owned(),
        // The starter binds `process.env.HOST`; inside the container it must
        // accept the engine's forwarder, so it binds all interfaces of the
        // isolated internal network — the host side stays loopback-only via
        // `--publish` below.
        "--env=HOST=0.0.0.0".to_owned(),
        format!("--env=PORT={}", manifest.web.port),
        format!("--env=DATA_DIR={}", manifest.storage.path),
        "--publish".to_owned(),
        format!("{LOOPBACK_IP}::{}", manifest.web.port),
        // The same bind the AI checks use, but read-only: generated code must
        // never write to the workspace from inside the preview (docs:
        // "bind-mounts the session workspace read-only").
        "--volume".to_owned(),
        format!("{}:{CONTAINER_WORKDIR}:ro", workspace_root.display()),
        "--volume".to_owned(),
        format!("{}:{}", data_volume_name(session_id), manifest.storage.path),
        "--workdir".to_owned(),
        CONTAINER_WORKDIR.to_owned(),
        PREVIEW_IMAGE.to_owned(),
        "node".to_owned(),
        "server.js".to_owned(),
    ]
}

/// Appends the exact pull command when a `--pull=never` run failed on a
/// missing image — the `pocket.rs` missing-image-hint pattern.
fn with_pull_hint(err: String) -> String {
    let lower = err.to_lowercase();
    let missing = ["image not known", "no such image", "unable to find image"]
        .iter()
        .any(|marker| lower.contains(marker));
    if missing {
        format!("{err} Pull the pinned preview image first: podman pull {PREVIEW_IMAGE}")
    } else {
        err
    }
}

/// Reads and validates `pocket.yaml` as the preview's contract: the internal
/// port, healthcheck path, and storage path all come from it and nowhere
/// else. An invalid or host-incompatible manifest is an actionable refusal
/// before any engine call.
fn runnable_manifest(workspace: &dyn PreviewSource) -> Result<Manifest, String> {
    let text = workspace.read("pocket.yaml")?;
    let manifest = manifest::parse_manifest(&text)?;
    manifest.validate_host()?;
    Ok(manifest)
}

/// The owned container name for a validated session id.
fn container_name(session_id: &str) -> String {
    format!("{PREVIEW_NAME_PREFIX}{session_id}")
}

/// The owned data volume name for a validated session id.
fn data_volume_name(session_id: &str) -> String {
    format!("{PREVIEW_NAME_PREFIX}{session_id}-data")
}

/// The `--label` argument asserting preview ownership of this session's
/// objects.
fn ownership_label_arg(session_id: &str) -> String {
    format!(
        "{}={}",
        pocket::OWNERSHIP_LABEL,
        ownership_value(session_id)
    )
}

/// The required [`pocket::OWNERSHIP_LABEL`] value for one session.
fn ownership_value(session_id: &str) -> String {
    format!("{PREVIEW_OWNERSHIP_PREFIX}{session_id}")
}

/// The session key a `ps` row's names carry under `prefix`, or `None` unless
/// a reported name is exactly `prefix` plus 32 lowercase hex characters.
/// Leading slashes are stripped the way every other name comparison here
/// strips them.
fn entries_owned_key(entry: &PsEntry) -> Option<&str> {
    let candidates: Vec<&str> = match &entry.names {
        PsNames::One(one) => vec![one.as_str()],
        PsNames::Many(many) => many.iter().map(String::as_str).collect(),
    };
    candidates.into_iter().find_map(|candidate| {
        let candidate = candidate.strip_prefix('/').unwrap_or(candidate);
        let key = candidate.strip_prefix(PREVIEW_NAME_PREFIX)?;
        is_lower_hex(key, SESSION_ID_LEN).then_some(key)
    })
}

/// The session key an owned data-volume name carries.
fn owned_volume_key(name: &str) -> Option<&str> {
    let rest = name
        .strip_prefix(PREVIEW_NAME_PREFIX)?
        .strip_suffix("-data")?;
    is_lower_hex(rest, SESSION_ID_LEN).then_some(rest)
}

/// Sweeps one container row down to an owned, inspected container ready for
/// removal, or `None` when it is not ours (refusals are `Err`).
async fn owned_container(
    bin: &Path,
    id: &str,
    name: &str,
) -> Result<Option<InspectedPreview>, String> {
    if !pocket::is_safe_container_id(id) {
        return Err(format!(
            "`podman ps` reported an unusable container id for {name}"
        ));
    }
    let container = inspect_container(bin, id).await?;
    let session_key = name
        .strip_prefix(PREVIEW_NAME_PREFIX)
        .filter(|key| is_lower_hex(key, SESSION_ID_LEN));
    let owned = session_key.is_some_and(|key| {
        container.name == *name
            && container.ownership.as_deref() == Some(ownership_value(key).as_str())
    });
    Ok(owned.then_some(container))
}

/// Exactly `len` lowercase hexadecimal ASCII characters (`pocket.rs` rule).
fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Required length of a generation session id: 32 lowercase hex characters,
/// the simple-UUID-v4 shape `crate::ai::agent` generates.
const SESSION_ID_LEN: usize = 32;

/// Shared preview identifier check used by the engine and native pane.
pub(crate) fn validate_session_id(session_id: &str) -> Result<(), String> {
    if is_lower_hex(session_id, SESSION_ID_LEN) {
        Ok(())
    } else {
        Err(format!(
            "invalid session id {session_id:?}: exactly {SESSION_ID_LEN} lowercase hexadecimal \
             characters are required"
        ))
    }
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// An owned preview container: inspected ID (the only identity mutations
/// target), running flag, and validated loopback URL when one is required.
#[derive(Debug, Clone)]
struct OwnedPreview {
    id: String,
    running: bool,
    url: Option<String>,
}

/// Inspected identity fields of a container, parsed for the preview checks.
#[derive(Debug)]
struct InspectedPreview {
    name: String,
    running: bool,
    ownership: Option<String>,
    loopback_port: Option<u16>,
}

/// One row of `podman ps --format json`; Podman marshals `Names` as an array,
/// Docker-compatible engines as one string (`pocket.rs` handling).
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

/// The inspected fields this module needs; pointers stay `Option` because
/// Podman can emit nulls.
#[derive(Debug, Deserialize)]
struct InspectEntry {
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
    #[serde(rename = "Ports")]
    ports: Option<HashMap<String, Option<Vec<PortBinding>>>>,
}

#[derive(Debug, Deserialize)]
struct PortBinding {
    #[serde(rename = "HostIp", default)]
    host_ip: Option<String>,
    #[serde(rename = "HostPort", default)]
    host_port: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VolumeListEntry {
    #[serde(rename = "Name")]
    name: String,
}

#[derive(Debug, Deserialize)]
struct VolumeInspectEntry {
    #[serde(rename = "Labels")]
    labels: Option<HashMap<String, String>>,
}

// Test adapter for exercising the runtime against real paths and read-only
// fake engine fixtures. Production always provisions the private mirror.
#[cfg(test)]
struct Workspace(PathBuf);

#[cfg(test)]
impl Workspace {
    fn create(root: &Path, id: &str) -> Result<Self, String> {
        validate_session_id(id)?;
        let directory = root.join(id);
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        std::fs::create_dir(&directory).map_err(|e| e.to_string())?;
        let workspace = Self(std::fs::canonicalize(directory).map_err(|e| e.to_string())?);
        let files = crate::scaffold::render("Preview Fixture", "preview-fixture", "")?;
        for (name, text) in files {
            workspace.write(&name, &text)?;
        }
        Ok(workspace)
    }

    fn open(root: &Path, id: &str) -> Result<Self, String> {
        validate_session_id(id)?;
        let directory = root.join(id);
        if !std::fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
            return Err("The preview fixture is missing.".into());
        }
        Ok(Self(
            std::fs::canonicalize(directory).map_err(|e| e.to_string())?,
        ))
    }

    fn write(&self, name: &str, text: &str) -> Result<(), String> {
        let destination = self.resolve(name)?;
        let parent = destination.parent().ok_or("Missing fixture parent.")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temporary = parent.join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::write(&temporary, text)
            .and_then(|()| std::fs::rename(&temporary, destination))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
impl PreviewSource for Workspace {
    fn root(&self) -> &Path {
        &self.0
    }

    fn resolve(&self, name: &str) -> Result<PathBuf, String> {
        crate::preview_source::resolve(&self.0, name)
    }

    fn read(&self, name: &str) -> Result<String, String> {
        std::fs::read_to_string(self.resolve(name)?).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Session id in the exact 32-hex shape the agent loop generates.
    const SESSION: &str = "0123456789abcdef0123456789abcdef";

    /// Podman's reported container ID: lowercase hex, the only form
    /// [`pocket::is_safe_container_id`] accepts.
    const CID: &str = "6e1f2c3d4b5a69788796a5b4c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4";

    /// Stand-in for the proxy URL a completed `start` records as the
    /// user-facing url. Production allocates the port at spawn time, so
    /// state-only tests seed this fixed loopback address instead.
    const PROXY_URL: &str = "http://127.0.0.1:49999";

    fn container_name_of() -> String {
        format!("{PREVIEW_NAME_PREFIX}{SESSION}")
    }

    fn volume_of() -> String {
        format!("{PREVIEW_NAME_PREFIX}{SESSION}-data")
    }

    /// Writes an executable stand-in for `podman` that appends every
    /// invocation to `record` as one space-joined line, prints whatever `body`
    /// prints for the subcommand in `$1`, and exits 0. Only shell builtins are
    /// used: the script must not need any variable or external tool.
    fn fake_podman(dir: &Path, body: &str) -> (PathBuf, PathBuf) {
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

    /// A seeded starter workspace under a fresh AI root: a real
    /// `pocket.yaml` (port 8080, healthcheck `/health`, storage `/data`)
    /// exactly as the agent loop creates it.
    fn starter(ai_root: &Path) -> Workspace {
        Workspace::create(ai_root, SESSION).expect("create starter workspace")
    }

    /// A loopback listener that answers every request with 200, standing in
    /// for the previewed app so `health::wait_ready` passes against the port
    /// the fake inspect reports.
    async fn ready_app() -> (Arc<TcpListener>, u16) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback probe port");
        let port = listener.local_addr().expect("local addr").port();
        let listener = Arc::new(listener);
        let server = listener.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = server.accept().await else {
                    return;
                };
                // Drain the HTTP request before writing and closing; on Linux,
                // closing a TCP socket with unread request bytes triggers RST
                // and makes reqwest treat the probe as a connection reset.
                let mut req = [0u8; 2048];
                let _ = socket.read(&mut req).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });
        (listener, port)
    }

    /// One `podman ps --format json` row naming the session's container.
    fn ps_row(name: &str, id: &str) -> String {
        format!(r#"[{{"Id":"{id}","Names":["{name}"]}}]"#)
    }

    /// One `podman inspect` element for the session's container. `owned`
    /// decides whether it carries `dev.pockets.managed=preview-<id>`;
    /// `host_port` is the single loopback mapping, or null.
    fn owned_inspect(owned: bool, running: bool, host_port: Option<u16>) -> String {
        let config = if owned {
            format!(
                r#""Config":{{"Labels":{{"{}":"{PREVIEW_OWNERSHIP_PREFIX}{SESSION}"}}}}"#,
                pocket::OWNERSHIP_LABEL
            )
        } else {
            r#""Config":{}"#.to_owned()
        };
        let ports = match host_port {
            Some(port) => {
                format!(r#"{{"8080/tcp":[{{"HostIp":"127.0.0.1","HostPort":"{port}"}}]}}"#)
            }
            None => r#"{"8080/tcp":null}"#.to_owned(),
        };
        format!(
            r#"[{{"Id":"{CID}","Name":"{}","State":{{"Running":{running}}},{config},"NetworkSettings":{{"Ports":{ports}}}}}]"#,
            container_name_of()
        )
    }

    /// One `podman volume ls --format json` row for the session's volume.
    fn volume_ls_row(name: &str) -> String {
        format!(r#"[{{"Name":"{name}"}}]"#)
    }

    /// One `podman volume inspect` result: ours when `owned`, otherwise
    /// carrying another tool's label under the same name.
    fn volume_inspect(owned: bool, session_key: &str) -> String {
        let labels = if owned {
            format!(
                r#"{{"{}":"{PREVIEW_OWNERSHIP_PREFIX}{session_key}"}}"#,
                pocket::OWNERSHIP_LABEL
            )
        } else {
            r#"{"Owner":"someone-else"}"#.to_owned()
        };
        format!(r#"[{{"Labels":{labels}}}]"#)
    }

    #[test]
    fn preview_run_args_are_loopback_internal_and_privilege_free() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ws = starter(&dir.path().join("ai"));
        let manifest = runnable_manifest(&ws).expect("starter manifest");
        let args = preview_run_args(SESSION, ws.root(), &manifest, PREVIEW_NETWORK);
        let value_after = |flag: &str| {
            args.iter()
                .position(|arg| arg == flag)
                .and_then(|index| args.get(index + 1))
                .map(String::as_str)
        };

        assert_eq!(args.first().map(String::as_str), Some("run"));
        assert!(args.iter().any(|arg| arg == "--detach"));
        assert_eq!(value_after("--name"), Some(container_name_of().as_str()));
        assert_eq!(
            value_after("--label"),
            Some("dev.pockets.managed=preview-0123456789abcdef0123456789abcdef")
        );
        assert!(args.iter().any(|arg| arg == "--pull=never"));
        assert_eq!(value_after("--network"), Some(PREVIEW_NETWORK));
        assert_eq!(value_after("--memory"), Some("512m"));
        assert!(args.iter().any(|arg| arg == "--cap-drop=ALL"));
        assert!(args
            .iter()
            .any(|arg| arg == "--security-opt=no-new-privileges"));
        // Loopback-only engine-allocated publish on the manifest port, the
        // exact `pocket.rs` shape.
        assert_eq!(value_after("--publish"), Some("127.0.0.1::8080"));
        assert_eq!(value_after("--workdir"), Some("/work"));
        let bind = format!("{}:/work:ro", ws.root().display());
        let data = format!("{}:{}", volume_of(), manifest.storage.path);
        let mounted: Vec<&str> = args
            .iter()
            .enumerate()
            .filter(|(_, arg)| arg.as_str() == "--volume")
            .filter_map(|(index, _)| args.get(index + 1).map(String::as_str))
            .collect();
        assert_eq!(
            mounted,
            vec![bind.as_str(), data.as_str()],
            "the read-only workspace bind then the labelled data volume: {args:?}"
        );

        // A fixed entrypoint on the pinned image, with no watch flags:
        // Node 24 removed --watch-poll and in-container watch is deaf
        // (spikes/runtime/RESULTS.md), so reloads are controller restarts.
        assert_eq!(
            args.last().map(String::as_str),
            Some("server.js"),
            "the command is `node server.js`"
        );
        assert_eq!(args.iter().rev().nth(1).map(String::as_str), Some("node"));
        assert_eq!(
            args.iter().rev().nth(2).map(String::as_str),
            Some(PREVIEW_IMAGE)
        );
        for token in &args {
            assert!(!token.contains("watch"), "no watch flags: {token}");
        }

        // Only the contract's three fixed environment values; the host
        // environment never enters the container.
        let envs: Vec<&str> = args
            .iter()
            .filter(|arg| arg.starts_with("--env"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            envs,
            vec![
                "--env=HOST=0.0.0.0",
                "--env=PORT=8080",
                "--env=DATA_DIR=/data",
            ],
            "exactly the fixed starter contract"
        );

        // The safety surface: no privilege, no escape, no engine socket, and
        // the internal network is the only network.
        for forbidden in ["--privileged", "--device", "--mount", "--net="] {
            assert!(
                !args.iter().any(|arg| arg.starts_with(forbidden)),
                "unexpected {forbidden} in {args:?}"
            );
        }
        assert!(
            !args
                .iter()
                .any(|arg| arg == "-v" || arg == "-p" || arg == "-e" || arg == "--env"),
            "shorthands must not appear: {args:?}"
        );
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "--network")
                .count(),
            1,
            "never a second network: {args:?}"
        );
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "--publish")
                .count(),
            1,
            "one loopback-bound publish: {args:?}"
        );
        let joined = args.join(" ");
        for escape in ["podman.sock", "/run/podman", ".local/share/containers"] {
            assert!(!joined.contains(escape), "no engine socket: {joined}");
        }
    }

    #[tokio::test]
    async fn start_provisions_the_owned_preview_and_reports_ready() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        let ws = starter(&ai_root);
        let manifest = runnable_manifest(&ws).expect("starter manifest");
        let (_server, port) = ready_app().await;
        let ran_marker = dir.path().join("already-ran");

        // The first `ps` finds no container, the reconcile lookup does — the
        // only shell-builtin way to script that sequence in the fake.
        let script = format!(
            r#"if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '[]'; fi
if [ "$1" = "ps" ]; then
  if [ -e '{marker}' ]; then printf '{row}'; else : > '{marker}'; printf '[]'; fi
fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi"#,
            marker = ran_marker.display(),
            row = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(port)),
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();

        let status = start(&ai_root, &bin, SESSION, &previews)
            .await
            .expect("the scripted preview becomes ready");
        assert_eq!(status.state, PreviewState::Ready);
        // Deliberate surface change (M4 proxy slice): the user-facing url is
        // the proxy listener; the engine-reported container mapping stays
        // internal to the entry.
        let url = status.url.expect("a ready preview reports its proxy url");
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert_ne!(
            url,
            format!("http://127.0.0.1:{port}"),
            "never the raw container mapping"
        );
        let container_url = previews
            .lock()
            .get(SESSION)
            .and_then(|entry| entry.container_url.clone())
            .expect("the raw loopback mapping stays internal");
        assert_eq!(container_url, format!("http://127.0.0.1:{port}"));
        assert_eq!(status.revision, 2, "preparing then ready");
        let states: Vec<PreviewState> = status.events_since.iter().map(|e| e.state).collect();
        assert_eq!(states, vec![PreviewState::Preparing, PreviewState::Ready]);

        let expected_run =
            preview_run_args(SESSION, ws.root(), &manifest, PREVIEW_NETWORK).join(" ");
        assert_eq!(
            invocations(&record),
            vec![
                format!("image exists {PREVIEW_IMAGE}"),
                format!("network create --internal {PREVIEW_NETWORK}"),
                format!("volume ls --filter name=^{}$ --format json", volume_of()),
                format!(
                    "volume create --label {}=preview-{SESSION} {}",
                    pocket::OWNERSHIP_LABEL,
                    volume_of()
                ),
                format!(
                    "ps --all --filter name=^{}$ --format json",
                    container_name_of()
                ),
                expected_run,
                format!(
                    "ps --all --filter name=^{}$ --format json",
                    container_name_of()
                ),
                format!("inspect {CID}"),
            ],
            "network, volume, container, then reconcile from Podman"
        );
    }

    #[tokio::test]
    async fn a_preview_refuses_an_existing_network_with_outbound_access() {
        let dir = tempfile::tempdir().unwrap();
        let script = format!(
            r#"if [ "$2" = "create" ]; then printf 'already exists\n' >&2; exit 125; fi
printf '{PREVIEW_NETWORK} bridge false\n'"#
        );
        let (bin, _) = fake_podman(dir.path(), &script);
        assert!(ensure_network(&bin)
            .await
            .unwrap_err()
            .contains("must be an internal bridge"));
    }

    #[tokio::test]
    async fn start_is_idempotent_adopting_running_objects_and_tolerating_the_race() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        starter(&ai_root);
        let (_server, port) = ready_app().await;

        let script = format!(
            r#"if [ "$1" = "network" ] && [ "$2" = "create" ]; then printf 'network with name {net} already exists\n' >&2; exit 125; fi
if [ "$1" = "network" ] && [ "$2" = "inspect" ]; then printf '{net} bridge true\n'; fi
if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{vrow}'; fi
if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{vinspect}'; fi
if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi"#,
            net = PREVIEW_NETWORK,
            vrow = volume_ls_row(&volume_of()),
            vinspect = volume_inspect(true, SESSION),
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(port)),
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();

        let status = start(&ai_root, &bin, SESSION, &previews)
            .await
            .expect("repeated start adopts the running preview");
        assert_eq!(status.state, PreviewState::Ready);
        // Deliberate surface change (M4 proxy slice): the user-facing url is
        // the proxy listener; the engine-reported container mapping stays
        // internal to the entry.
        let url = status.url.expect("a ready preview reports its proxy url");
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert_ne!(
            url,
            format!("http://127.0.0.1:{port}"),
            "never the raw container mapping"
        );
        let container_url = previews
            .lock()
            .get(SESSION)
            .and_then(|entry| entry.container_url.clone())
            .expect("the raw loopback mapping stays internal");
        assert_eq!(container_url, format!("http://127.0.0.1:{port}"));

        let calls = invocations(&record);
        assert!(
            !calls.iter().any(|line| line.starts_with("run ")),
            "a running owned container is never recreated: {calls:?}"
        );
        assert!(
            !calls.iter().any(|line| line.starts_with("rm ")),
            "nothing is removed on the adopt path: {calls:?}"
        );
        assert!(
            !calls.iter().any(|line| line.starts_with("volume create")),
            "the existing labelled volume is reused: {calls:?}"
        );
        // The already-exists race kept the network ensure non-fatal.
        assert!(calls
            .iter()
            .any(|line| *line == format!("network create --internal {PREVIEW_NETWORK}")));
    }

    #[tokio::test]
    async fn start_fails_actionably_when_the_pinned_image_is_absent() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        starter(&ai_root);

        let script =
            r#"if [ "$1" = "image" ]; then printf 'Error: image not known\n' >&2; exit 1; fi"#;
        let (bin, record) = fake_podman(dir.path(), script);
        let previews = Previews::default();

        let err = start(&ai_root, &bin, SESSION, &previews)
            .await
            .expect_err("a missing pinned image must stop provisioning");
        assert!(err.contains(PREVIEW_IMAGE), "{err}");
        assert!(
            err.contains("podman pull"),
            "must name the pull command: {err}"
        );

        let calls = invocations(&record);
        assert_eq!(
            calls,
            vec![format!("image exists {PREVIEW_IMAGE}")],
            "no network, volume, or container work before the precondition passes"
        );
        let status = status(&previews, SESSION, 0).expect("entry exists");
        assert_eq!(status.state, PreviewState::Failed);
        assert!(
            status.error.is_some(),
            "the failure text is timestamped in status"
        );
    }

    /// Seeds the registry the way a completed `start` leaves it: `container_url`
    /// internal, `proxy_url` as the user-facing url. The proxy url is a
    /// stand-in for the OS-allocated listener address (state-only tests must
    /// not need a live listener); `attach_proxy` is what records this shape
    /// in production.
    fn seeded_ready(previews: &Previews, container_url: &str, proxy_url: &str) {
        let _sender = previews.begin(SESSION);
        previews.transition(
            SESSION,
            PreviewState::Ready,
            Some(proxy_url.to_owned()),
            None,
        );
        previews
            .lock()
            .get_mut(SESSION)
            .expect("seeded entry")
            .container_url = Some(container_url.to_owned());
    }

    #[tokio::test]
    async fn refresh_gates_visibility_before_restarting() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        let ws = starter(&ai_root);
        std::fs::write(ws.root().join("server.js"), b"preview gate").expect("edit server.js");
        let host_hash = sha256_hex(&ws.root().join("server.js")).expect("host hash");
        let wrong_hash = "0".repeat(64);
        let (_server, port) = ready_app().await;
        let seen_marker = dir.path().join("first-exec");

        // The first exec still sees the stale content (the rename window);
        // the second sees the edit — the restart must wait for it.
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi
if [ "$1" = "exec" ]; then
  if [ -e '{marker}' ]; then printf '%s  /work/server.js\n' '{good}'; else : > '{marker}'; printf '%s  /work/server.js\n' '{bad}'; fi
fi"#,
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(port)),
            marker = seen_marker.display(),
            good = host_hash,
            bad = wrong_hash,
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(&previews, &format!("http://127.0.0.1:{port}"), PROXY_URL);

        let status = refresh_with(
            &ai_root,
            &bin,
            SESSION,
            &["server.js".to_owned()],
            &previews,
            Duration::from_millis(1),
            Duration::from_secs(2),
        )
        .await
        .expect("the gated restart completes");
        assert_eq!(status.state, PreviewState::Ready);
        // Deliberate surface change (M4 proxy slice): the user-facing url is
        // the proxy's, and it must survive the restart untouched — never the
        // raw container mapping.
        assert_eq!(
            status.url.as_deref(),
            Some(PROXY_URL),
            "the refresh keeps the user-facing proxy url stable"
        );

        let calls = invocations(&record);
        let execs: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with("exec "))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(execs.len(), 2, "stale round then visible round: {calls:?}");
        assert_eq!(
            calls[execs[0]],
            format!("exec {CID} sha256sum /work/server.js"),
            "the gate compares the container-side hash by inspected ID"
        );
        let restart_at = calls
            .iter()
            .position(|line| line == &format!("restart --time 0 {CID}"))
            .expect("a plain-time-zero restart of the inspected ID");
        assert!(
            restart_at > execs[execs.len() - 1],
            "the restart may only follow the successful visibility round: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|line| line == "restart -t 10" || line.contains("restart --time 10")),
            "never a SIGTERM-timeout restart: {calls:?}"
        );
    }

    /// A gate timeout refuses the restart before anything runs, so the entry
    /// stays `ready` with its URL (the old process still serves): the refusal
    /// is an actionable event plus a returned error, not a `failed`. A later
    /// turn can refresh again. (Deliberate contract change: the pre-style-tier
    /// cycle used to land `failed` even though nothing was touched.)
    #[tokio::test]
    async fn refresh_refuses_to_restart_when_visibility_never_arrives() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        let ws = starter(&ai_root);
        std::fs::write(ws.root().join("server.js"), b"never propagates").expect("edit");
        let (_server, port) = ready_app().await;

        // The container stays deaf to the rename for the whole window.
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi
if [ "$1" = "exec" ]; then printf '%s  /work/server.js\n' '{bad}'; fi"#,
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(port)),
            bad = "0".repeat(64),
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(&previews, &format!("http://127.0.0.1:{port}"), PROXY_URL);

        let err = refresh_with(
            &ai_root,
            &bin,
            SESSION,
            &["server.js".to_owned()],
            &previews,
            Duration::from_millis(1),
            Duration::from_millis(5),
        )
        .await
        .expect_err("an invisible edit must not be restarted on");
        assert!(
            err.contains("server.js"),
            "the message names the file: {err}"
        );
        assert!(err.contains("never became visible"), "{err}");

        let calls = invocations(&record);
        assert!(
            !calls.iter().any(|line| line.starts_with("restart")),
            "a timed-out gate forbids the restart entirely: {calls:?}"
        );
        let status = status(&previews, SESSION, 0).expect("entry exists");
        assert_eq!(
            status.state,
            PreviewState::Ready,
            "a refusal before any restart leaves the untouched preview ready"
        );
        assert_eq!(
            status.url.as_deref(),
            Some(PROXY_URL),
            "the refused refresh keeps serving the same proxy url"
        );
        assert!(
            status.error.is_none(),
            "the live preview carries no failure"
        );
        let last = status.events_since.last().expect("the refusal is recorded");
        assert_eq!(last.state, PreviewState::Ready, "the note moves nothing");
        assert_eq!(
            last.message.as_deref(),
            Some(err.as_str()),
            "the event carries the actionable refusal text"
        );
    }

    /// The removal side of a history restore: a changed path already gone
    /// from the host is still a gate target, and the container must prove it
    /// has forgotten the file before any restart. The first `test ! -e`
    /// fails — the deleted file is still there through the lagging mount —
    /// the second confirms absence, and only then does the restart run: the
    /// removal gates the restart exactly like a lagging write.
    #[tokio::test]
    async fn refresh_gates_a_lagging_removal_before_restarting() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        let ws = starter(&ai_root);
        std::fs::write(ws.root().join("server.js"), b"restored source").expect("edit server.js");
        let host_hash = sha256_hex(&ws.root().join("server.js")).expect("host hash");
        assert!(
            !ws.root().join("gone.js").exists(),
            "the restore already removed the file from the host"
        );
        let (_server, port) = ready_app().await;
        let absence_marker = dir.path().join("absence-checked");

        // `server.js` hash-matches on every round, so the removal alone
        // holds the gate: the first absence check reports the file still
        // present inside the container, later ones confirm it is gone.
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi
if [ "$1" = "exec" ]; then
  if [ "$3" = "sha256sum" ]; then printf '%s  /work/server.js\n' '{good}'; exit 0; fi
  if [ "$3" = "test" ]; then
    if [ -e '{marker}' ]; then exit 0; else : > '{marker}'; exit 1; fi
  fi
  printf 'unexpected exec argv\n' >&2; exit 64;
fi"#,
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(port)),
            marker = absence_marker.display(),
            good = host_hash,
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(&previews, &format!("http://127.0.0.1:{port}"), PROXY_URL);

        let status = refresh_with(
            &ai_root,
            &bin,
            SESSION,
            &["server.js".to_owned(), "gone.js".to_owned()],
            &previews,
            Duration::from_millis(1),
            Duration::from_secs(2),
        )
        .await
        .expect("the gated restart completes once the removal is visible");
        assert_eq!(status.state, PreviewState::Ready);
        assert_eq!(status.url.as_deref(), Some(PROXY_URL));

        let calls = invocations(&record);
        let absence = format!("exec {CID} test ! -e /work/gone.js");
        let checks: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, line)| **line == absence)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            checks.len(),
            2,
            "stale absence round then confirmed round, over the inspected ID with no shell: \
             {calls:?}"
        );
        assert!(
            calls.contains(&format!("exec {CID} sha256sum /work/server.js")),
            "the surviving write is still hash-compared: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|line| line.contains("sha256sum /work/gone.js")),
            "a path already gone from the host is never hash-compared: {calls:?}"
        );
        let restart_at = calls
            .iter()
            .position(|line| line == &format!("restart --time 0 {CID}"))
            .expect("a plain-time-zero restart of the inspected ID");
        assert!(
            restart_at > *checks.last().expect("absence checks were recorded"),
            "the restart may only follow the confirmed absence: {calls:?}"
        );
    }

    /// A permanent removal lag is refused exactly like a permanent write
    /// lag: returned error naming the removed path, zero restart commands,
    /// and the entry stays `ready` with its proxy URL — nothing was touched,
    /// so the refusal is an event note, not a `failed`.
    #[tokio::test]
    async fn refresh_refuses_to_restart_when_a_removal_never_propagates() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        let ws = starter(&ai_root);
        assert!(
            !ws.root().join("gone.js").exists(),
            "the host already reflects the removal"
        );
        let (_server, port) = ready_app().await;

        // The container keeps seeing the deleted file for the whole window.
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi
if [ "$1" = "exec" ]; then printf 'the file is still mounted\n' >&2; exit 1; fi"#,
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(port)),
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(&previews, &format!("http://127.0.0.1:{port}"), PROXY_URL);

        let err = refresh_with(
            &ai_root,
            &bin,
            SESSION,
            &["gone.js".to_owned()],
            &previews,
            Duration::from_millis(1),
            Duration::from_millis(5),
        )
        .await
        .expect_err("an invisible removal must not be restarted on");
        assert!(err.contains("gone.js"), "the message names the file: {err}");
        assert!(
            err.contains("removal"),
            "the message says a removal lagged: {err}"
        );
        assert!(err.contains("never became visible"), "{err}");

        let calls = invocations(&record);
        assert!(
            calls.contains(&format!("exec {CID} test ! -e /work/gone.js")),
            "the fixed absence argv ran against the inspected ID: {calls:?}"
        );
        assert!(
            !calls.iter().any(|line| line.starts_with("restart")),
            "a timed-out gate forbids the restart entirely: {calls:?}"
        );
        let status = status(&previews, SESSION, 0).expect("entry exists");
        assert_eq!(
            status.state,
            PreviewState::Ready,
            "a refusal before any restart leaves the untouched preview ready"
        );
        assert_eq!(
            status.url.as_deref(),
            Some(PROXY_URL),
            "the refused refresh keeps serving the same proxy url"
        );
        assert!(
            status.error.is_none(),
            "the live preview carries no failure"
        );
        let last = status.events_since.last().expect("the refusal is recorded");
        assert_eq!(last.state, PreviewState::Ready, "the note moves nothing");
        assert_eq!(
            last.message.as_deref(),
            Some(err.as_str()),
            "the event carries the actionable refusal text"
        );
    }

    /// The gate's preflight contract, driven directly: a regular file and an
    /// already-removed path both become targets (a restore reports the union
    /// of old and new paths), while directories, symlinks, and escapes stay
    /// refused — a former file that is now a directory is replaced by the
    /// tree under it at the command layer, never hashed here.
    #[test]
    fn gate_targets_accepts_regular_and_removed_paths_and_refuses_the_rest() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ws = starter(&dir.path().join("ai"));
        std::fs::write(ws.root().join("app.js"), b"kept").expect("write app.js");
        std::fs::create_dir(ws.root().join("assets")).expect("seed former-file directory");
        #[cfg(unix)]
        std::os::unix::fs::symlink("app.js", ws.root().join("link.js")).expect("seed symlink");

        let targets = gate_targets(&ws, &["app.js".to_owned(), "gone.js".to_owned()])
            .expect("a regular file and an expected removal are both gate targets");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].2, format!("{CONTAINER_WORKDIR}/app.js"));
        assert_eq!(targets[1].0, "gone.js");
        assert_eq!(targets[1].2, format!("{CONTAINER_WORKDIR}/gone.js"));

        let dir_err = gate_targets(&ws, &["assets".to_owned()])
            .expect_err("a directory is the command layer's tree expansion, not a target");
        assert!(dir_err.contains("not a regular file"), "{dir_err}");
        let escape_err = gate_targets(&ws, &["../outside".to_owned()])
            .expect_err("an escape is refused before anything reaches the engine");
        assert!(escape_err.contains("inside the workspace"), "{escape_err}");
        #[cfg(unix)]
        {
            let link_err = gate_targets(&ws, &["link.js".to_owned()])
                .expect_err("a symlink is never a gate target");
            assert!(link_err.contains("symbolic link"), "{link_err}");
        }
    }

    /// A stylesheet-only change set stops at the visibility gate: the
    /// container already reads the new bytes and static CSS is served fresh
    /// off the read-only mount, so there is no restart, no health re-probe,
    /// and no state change — just the `Changed` signal for the proxy's style
    /// frame. The inspected loopback mapping points at a CLOSED port, so any
    /// re-probe attempt would burn the whole health window and land `failed`;
    /// the entry staying `ready` proves the probe never ran.
    #[tokio::test]
    async fn style_only_refresh_skips_the_restart_and_the_health_probe() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        let ws = starter(&ai_root);
        std::fs::write(ws.root().join("site.css"), b".app { color: green }").expect("edit css");
        let host_hash = sha256_hex(&ws.root().join("site.css")).expect("host hash");

        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a loopback port to hand back");
            let port = listener.local_addr().expect("local addr").port();
            drop(listener);
            port
        };
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi
if [ "$1" = "exec" ]; then printf '%s  /work/site.css\n' '{good}'; fi"#,
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(dead_port)),
            good = host_hash,
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(
            &previews,
            &format!("http://127.0.0.1:{dead_port}"),
            PROXY_URL,
        );
        let live = previews
            .lock()
            .get(SESSION)
            .and_then(|entry| entry.live.clone())
            .expect("seeded fanout");
        let mut frames = live.subscribe();

        let status = refresh_with(
            &ai_root,
            &bin,
            SESSION,
            &["site.css".to_owned()],
            &previews,
            Duration::from_millis(1),
            Duration::from_secs(2),
        )
        .await
        .expect("a style-only refresh completes at the gate");
        assert_eq!(status.state, PreviewState::Ready);
        assert_eq!(
            status.url.as_deref(),
            Some(PROXY_URL),
            "the pane url never moves for a stylesheet"
        );
        assert_eq!(
            status.revision, 2,
            "preparing then ready, and nothing else: a style swap never leaves ready"
        );

        let calls = invocations(&record);
        assert!(
            calls.contains(&format!("exec {CID} sha256sum /work/site.css")),
            "the gate still proves host-to-container visibility: {calls:?}"
        );
        assert!(
            !calls.iter().any(|line| line.starts_with("restart")),
            "a stylesheet never restarts the server: {calls:?}"
        );

        match frames.try_recv() {
            Ok(LiveSignal::Changed(paths)) => assert_eq!(paths, vec!["site.css".to_owned()]),
            other => panic!("expected one Changed signal, got {other:?}"),
        }
        assert!(
            matches!(
                frames.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "a refresh that stayed ready publishes no state frame either"
        );
    }

    /// A stylesheet whose bytes never reach the container is refused exactly
    /// like any other pre-restart failure: returned error, entry stays
    /// `ready`, and nothing is published — the browser keeps the page whose
    /// stylesheet has not propagated yet.
    #[tokio::test]
    async fn style_only_refresh_refuses_and_publishes_nothing_when_visibility_times_out() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        let ws = starter(&ai_root);
        std::fs::write(ws.root().join("site.css"), b".app { color: green }").expect("edit");
        let (_server, port) = ready_app().await;

        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi
if [ "$1" = "exec" ]; then printf '%s  /work/site.css\n' '{bad}'; fi"#,
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(port)),
            bad = "0".repeat(64),
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(&previews, &format!("http://127.0.0.1:{port}"), PROXY_URL);
        let live = previews
            .lock()
            .get(SESSION)
            .and_then(|entry| entry.live.clone())
            .expect("seeded fanout");
        let mut frames = live.subscribe();

        let err = refresh_with(
            &ai_root,
            &bin,
            SESSION,
            &["site.css".to_owned()],
            &previews,
            Duration::from_millis(1),
            Duration::from_millis(5),
        )
        .await
        .expect_err("an invisible stylesheet must not publish");
        assert!(
            err.contains("site.css"),
            "the message names the file: {err}"
        );
        assert!(err.contains("never became visible"), "{err}");

        let calls = invocations(&record);
        assert!(
            !calls.iter().any(|line| line.starts_with("restart")),
            "a timed-out gate forbids the restart entirely: {calls:?}"
        );
        assert!(
            matches!(
                frames.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ),
            "a refused refresh publishes no live frames"
        );
        let status = status(&previews, SESSION, 0).expect("entry exists");
        assert_eq!(status.state, PreviewState::Ready);
        assert_eq!(status.url.as_deref(), Some(PROXY_URL));
        let last = status.events_since.last().expect("the refusal is recorded");
        assert_eq!(last.state, PreviewState::Ready);
        assert_eq!(last.message.as_deref(), Some(err.as_str()));
    }

    /// A mixed change set (server code plus a stylesheet) keeps the full
    /// restart cycle, and `failed` is only claimed once the restart was
    /// actually attempted: here the restart command succeeds but the app
    /// never answers the health probe again, so the entry lands `failed`
    /// truthfully while the proxy URL survives with the last-good page.
    /// Slow by construction: it waits out the full bounded health window.
    #[tokio::test]
    async fn restart_cycle_marks_failed_only_after_the_restart_is_attempted() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        let ws = starter(&ai_root);
        std::fs::write(ws.root().join("server.js"), b"process.send?.(1)").expect("edit js");
        std::fs::write(ws.root().join("site.css"), b".app { color: green }").expect("edit css");
        let js_hash = sha256_hex(&ws.root().join("server.js")).expect("js hash");
        let css_hash = sha256_hex(&ws.root().join("site.css")).expect("css hash");
        let dead_port = {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a loopback port to hand back");
            let port = listener.local_addr().expect("local addr").port();
            drop(listener);
            port
        };

        // The gate passes on the first round for BOTH paths, so the only
        // possible failure left is the post-restart health probe against the
        // closed port the fake inspect reports.
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi
if [ "$1" = "exec" ]; then
  if [ "$4" = "/work/server.js" ]; then printf '%s  /work/server.js\n' '{js}'; else printf '%s  /work/site.css\n' '{css}'; fi
fi"#,
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(dead_port)),
            js = js_hash,
            css = css_hash,
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(
            &previews,
            &format!("http://127.0.0.1:{dead_port}"),
            PROXY_URL,
        );

        let err = refresh_with(
            &ai_root,
            &bin,
            SESSION,
            &["server.js".to_owned(), "site.css".to_owned()],
            &previews,
            Duration::from_millis(1),
            Duration::from_secs(2),
        )
        .await
        .expect_err("a restarted app that never answers the probe must fail");
        assert!(err.contains("did not become healthy"), "{err}");

        let calls = invocations(&record);
        assert!(
            calls.contains(&format!("exec {CID} sha256sum /work/server.js")),
            "the gate covered the server file too: {calls:?}"
        );
        assert!(
            calls.contains(&format!("restart --time 0 {CID}")),
            "a mixed set still restarts — the stylesheet does not downgrade the cycle: {calls:?}"
        );

        let status = status(&previews, SESSION, 0).expect("entry exists");
        assert_eq!(status.state, PreviewState::Failed);
        assert_eq!(status.error.as_deref(), Some(err.as_str()));
        assert_eq!(
            status.url.as_deref(),
            Some(PROXY_URL),
            "the proxy keeps its url and the last-good page after a failed restart"
        );
        let states: Vec<PreviewState> = status.events_since.iter().map(|e| e.state).collect();
        assert_eq!(
            states,
            vec![
                PreviewState::Preparing,
                PreviewState::Ready,
                PreviewState::Updating,
                PreviewState::Failed
            ],
            "`updating` only appears once the restart is actually next"
        );
    }

    #[tokio::test]
    async fn refresh_requires_an_existing_preview_and_touches_nothing() {
        let dir = tempfile::TempDir::new().expect("temp ai root");
        let ai_root = dir.path().join("ai");
        starter(&ai_root);
        let (bin, record) = fake_podman(
            dir.path(),
            "echo 'a refresh without a preview must not call the engine' >&2; exit 1",
        );
        let previews = Previews::default();

        let err = refresh(
            &ai_root,
            &bin,
            SESSION,
            &["server.js".to_owned()],
            &previews,
        )
        .await
        .expect_err("nothing is refreshing");
        assert!(err.contains("no preview"), "{err}");
        assert!(
            !record.exists(),
            "a preview-less refresh runs no podman command at all: {:?}",
            std::fs::read_to_string(&record).unwrap_or_default()
        );
        assert!(status(&previews, SESSION, 0).is_err(), "no phantom entry");
    }

    /// The [`refresh_if_running`] policy, driven through the pure gate so no
    /// test needs an AppHandle or Podman: a turn's writes restart ONLY a
    /// ready preview, and only when the turn actually wrote something.
    #[test]
    fn refresh_is_requested_only_for_a_ready_preview_with_changes() {
        let previews = Previews::default();
        let changed = vec!["server.js".to_owned()];

        assert!(
            !refresh_requested(&previews, SESSION, &changed),
            "a session that never started a preview is never refreshed"
        );

        // Every non-ready state of an existing entry stays untouched: the
        // gate must never restart a preparation, race an in-flight restart,
        // or resurrect a failed or stopped lifecycle.
        previews.begin(SESSION);
        assert!(
            !refresh_requested(&previews, SESSION, &changed),
            "preparing is not ready"
        );
        previews.transition(SESSION, PreviewState::Updating, None, None);
        assert!(
            !refresh_requested(&previews, SESSION, &changed),
            "a restart already in flight is not a second restart"
        );
        previews.transition(SESSION, PreviewState::Failed, None, Some("boom".to_owned()));
        assert!(
            !refresh_requested(&previews, SESSION, &changed),
            "failed waits for a user decision, not a background restart"
        );
        previews.transition(SESSION, PreviewState::Stopped, None, None);
        assert!(
            !refresh_requested(&previews, SESSION, &changed),
            "stopped stays stopped"
        );

        // Ready with changes is the one state that earns the restart...
        previews.transition(
            SESSION,
            PreviewState::Ready,
            Some(PROXY_URL.to_owned()),
            None,
        );
        assert!(
            refresh_requested(&previews, SESSION, &changed),
            "a ready preview with writes hot-reloads"
        );

        // ...and a turn that wrote nothing never restarts even a ready one.
        assert!(
            !refresh_requested(&previews, SESSION, &[]),
            "no changes, no restart"
        );
    }

    #[tokio::test]
    async fn stop_removes_container_then_volume_then_the_shared_network() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let (_server, port) = ready_app().await;
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi
if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '{vrow}'; fi
if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{vinspect}'; fi"#,
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(true, true, Some(port)),
            vrow = volume_ls_row(&volume_of()),
            vinspect = volume_inspect(true, SESSION),
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(&previews, &format!("http://127.0.0.1:{port}"), PROXY_URL);

        assert!(
            stop(&bin, SESSION, &previews).await.expect("stop succeeds"),
            "an existing preview was stopped"
        );

        assert_eq!(
            invocations(&record),
            vec![
                format!(
                    "ps --all --filter name=^{}$ --format json",
                    container_name_of()
                ),
                format!("inspect {CID}"),
                format!("stop --time 0 {CID}"),
                format!("rm {CID}"),
                format!("ps --all --filter name=^pockets-egress-{SESSION}$ --format {{{{.Names}}}}"),
                format!("network ls --filter name=^pockets-egress-net-{SESSION}$ --format {{{{.Name}}}}"),
                format!("secret ls --filter name=^pockets-runtime-preview-{SESSION}- --format {{{{.ID}}}} {{{{.Name}}}}"),
                format!("volume ls --filter name=^{}$ --format json", volume_of()),
                format!("volume inspect {}", volume_of()),
                format!("volume rm {}", volume_of()),
                format!("network rm {PREVIEW_NETWORK}"),
            ],
            "owned container by ID, then volume by proved name, then the \
             shared network once no active preview remains"
        );
        let status = status(&previews, SESSION, 0).expect("entry survives");
        assert_eq!(status.state, PreviewState::Stopped);
        assert_eq!(status.url, None, "a stopped preview advertises no URL");
    }

    #[tokio::test]
    async fn stop_refuses_a_foreign_container_under_the_owned_name() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '{prow}'; fi
if [ "$1" = "inspect" ]; then printf '{inspect}'; fi"#,
            prow = ps_row(&container_name_of(), CID),
            inspect = owned_inspect(false, true, Some(41111)),
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(&previews, "http://127.0.0.1:41111", PROXY_URL);

        let err = stop(&bin, SESSION, &previews)
            .await
            .expect_err("a foreign container claiming the owned name is refused");
        assert!(err.contains("refusing"), "expected a refusal, got {err}");

        let calls = invocations(&record);
        assert_eq!(calls.len(), 2, "lookup only: {calls:?}");
        assert!(
            !calls
                .iter()
                .any(|line| line.starts_with("stop") || line.starts_with("rm")),
            "a refused container is never touched: {calls:?}"
        );
    }

    #[tokio::test]
    async fn stop_keeps_the_shared_network_while_another_preview_is_active() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let other = "ab".repeat(16);
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '[]'; fi
if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '[]'; fi"#,
        );
        let (bin, record) = fake_podman(dir.path(), &script);
        let previews = Previews::default();
        seeded_ready(&previews, "http://127.0.0.1:41111", PROXY_URL);
        previews.begin(&other);
        previews.transition(&other, PreviewState::Ready, None, None);

        assert!(stop(&bin, SESSION, &previews).await.expect("stop succeeds"));

        let calls = invocations(&record);
        assert!(
            !calls.iter().any(|line| line.starts_with("network rm")),
            "the network survives while another preview still needs it: {calls:?}"
        );
    }

    #[tokio::test]
    async fn sweep_removes_owned_objects_and_leaves_everything_else_untouched() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        // A validly-spelled container carrying a foreign label: refused by
        // the inspect check even though its name completes the owned pattern.
        let foreign_labelled = format!(
            r#"[{{"Id":"deadbeef","Name":"{}","State":{{"Running":false}},"Config":{{"Labels":{{"{}":"someone-else"}}}},"NetworkSettings":{{"Ports":null}}}}]"#,
            container_name_of(),
            pocket::OWNERSHIP_LABEL,
        );
        let rows = format!(
            r#"[{{"Id":"{CID}","Names":["{}"]}},{{"Id":"ffffffff","Names":["/pockets-preview-nope"]}},{{"Id":"deadbeef","Names":["{}"]}}]"#,
            container_name_of(),
            container_name_of(),
        );
        let script = format!(
            r#"if [ "$1" = "ps" ]; then printf '%s' '{rows}'; fi
if [ "$1" = "inspect" ] && [ "$2" = "{CID}" ]; then printf '{inspect}'; fi
if [ "$1" = "inspect" ] && [ "$2" != "{CID}" ]; then printf '%s' '{foreign_labelled}'; fi
if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '[{{"Name":"{vol}"}},{{"Name":"pockets-preview-foreign-data"}}]'; fi
if [ "$1" = "volume" ] && [ "$2" = "inspect" ]; then printf '{vinspect}'; fi
if [ "$1" = "network" ]; then printf 'network is in use by container 99\n' >&2; exit 2; fi"#,
            rows = rows,
            inspect = owned_inspect(true, true, Some(41111)),
            foreign_labelled = foreign_labelled,
            vol = volume_of(),
            vinspect = volume_inspect(true, SESSION),
        );
        let (bin, record) = fake_podman(dir.path(), &script);

        let problems = sweep_orphans(&bin, &Default::default()).await;
        assert_eq!(
            problems.len(),
            1,
            "only the in-use network is reported: {problems:?}"
        );
        assert!(problems[0].contains("left in place"), "{problems:?}");

        let calls = invocations(&record);
        assert!(calls.contains(&format!("stop --time 0 {CID}")), "{calls:?}");
        assert!(calls.contains(&format!("rm {CID}")), "{calls:?}");
        assert!(
            calls.contains(&format!("volume rm {}", volume_of())),
            "{calls:?}"
        );
        assert!(
            calls.contains(&format!("network rm {PREVIEW_NETWORK}")),
            "the network is attempted once owned garbage was reclaimed: {calls:?}"
        );
        for forbidden in [
            "rm deadbeef",
            "rm ffffffff",
            "volume rm pockets-preview-foreign-data",
            "prune",
        ] {
            assert!(
                !calls.iter().any(|line| line.contains(forbidden)),
                "sweep must never touch {forbidden:?}: {calls:?}"
            );
        }
    }

    #[tokio::test]
    async fn sweep_leaves_a_clean_engine_entirely_alone() {
        let dir = tempfile::TempDir::new().expect("temp fixture dir");
        let script = r#"if [ "$1" = "ps" ]; then printf '[]'; fi
if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '[]'; fi"#;
        let (bin, record) = fake_podman(dir.path(), script);

        let problems = sweep_orphans(&bin, &Default::default()).await;
        assert!(problems.is_empty(), "nothing to reclaim: {problems:?}");
        assert_eq!(
            invocations(&record),
            vec![
                format!("ps --all --filter name=^{PREVIEW_NAME_PREFIX} --format json"),
                format!("volume ls --filter name=^{PREVIEW_NAME_PREFIX} --format json"),
            ],
            "listing is all a clean sweep may ever do; the shared network is \
             only removed alongside owned garbage"
        );
    }

    #[tokio::test]
    async fn startup_sweep_preserves_previews_already_registered_by_this_process() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!(
            r#"if [ "$1" = "ps" ]; then printf '%s' '{}'; fi
if [ "$1" = "volume" ] && [ "$2" = "ls" ]; then printf '%s' '{}'; fi"#,
            ps_row(&container_name_of(), CID),
            volume_ls_row(&volume_of())
        );
        let (bin, record) = fake_podman(dir.path(), &body);
        let protected = [SESSION.to_owned()].into_iter().collect();
        assert!(sweep_orphans(&bin, &protected).await.is_empty());
        let calls = invocations(&record);
        assert_eq!(
            calls.len(),
            2,
            "Only list; do not inspect or mutate an active preview: {calls:?}"
        );
    }

    #[test]
    fn status_streams_transitions_since_a_revision_cursor() {
        let previews = Previews::default();
        assert!(status(&previews, SESSION, 0)
            .expect_err("unknown sessions are refused")
            .contains("no preview"));
        assert!(status(&previews, "nope", 0)
            .expect_err("malformed ids are refused before any lookup")
            .contains("invalid session id"));

        previews.begin(SESSION);
        previews.transition(
            SESSION,
            PreviewState::Ready,
            Some("http://127.0.0.1:1".into()),
            None,
        );

        let all = status(&previews, SESSION, 0).expect("entry exists");
        assert_eq!(all.revision, 2);
        assert_eq!(all.next_cursor, 2);
        assert_eq!(all.events_since.len(), 2, "preparing then ready");
        assert_eq!(all.events_since[0].revision, 1);
        assert_eq!(all.events_since[1].state, PreviewState::Ready);

        // A poller holding cursor 2 sees nothing new; a mid-cursor poll sees
        // exactly the transitions after it.
        let fresh = status(&previews, SESSION, 2).expect("entry exists");
        assert!(fresh.events_since.is_empty(), "cursor at the head is empty");
        let since = status(&previews, SESSION, 1).expect("entry exists");
        assert_eq!(since.events_since.len(), 1);
        assert_eq!(since.events_since[0].state, PreviewState::Ready);

        // The ring stays bounded and the revision cursor survives eviction:
        // old events drop, but a poller's position never slips or repeats.
        for _ in 0..50 {
            previews.transition(SESSION, PreviewState::Updating, None, None);
        }
        let head = status(&previews, SESSION, 0).expect("entry exists");
        assert_eq!(head.revision, 52);
        assert_eq!(head.events_since.len(), MAX_PREVIEW_EVENTS);
        assert_eq!(
            head.events_since.first().expect("ring non-empty").revision,
            52 - MAX_PREVIEW_EVENTS as u64 + 1
        );
        assert_eq!(
            status(&previews, SESSION, 52)
                .expect("entry exists")
                .next_cursor,
            52
        );
    }

    #[test]
    fn status_json_matches_the_documented_surface() {
        let previews = Previews::default();
        previews.begin(SESSION);
        previews.transition(
            SESSION,
            PreviewState::Ready,
            Some("http://127.0.0.1:41111".into()),
            None,
        );
        let value =
            serde_json::to_value(status(&previews, SESSION, 0).expect("status")).expect("json");
        assert_eq!(value["state"], "ready", "states serialize lowercase");
        for (state, name) in [
            (PreviewState::Preparing, "preparing"),
            (PreviewState::Updating, "updating"),
            (PreviewState::Failed, "failed"),
            (PreviewState::Stopped, "stopped"),
        ] {
            assert_eq!(serde_json::to_value(state).expect("state json"), name);
        }
        assert_eq!(value["url"], "http://127.0.0.1:41111");
        assert!(value.get("error").is_none(), "absent error is skipped");
        assert!(value["eventsSince"].is_array(), "camelCase cursor field");
        assert_eq!(value["nextCursor"], 2);
        let event = &value["eventsSince"][0];
        assert!(event.get("atSeconds").is_some(), "events are timestamped");
        assert_eq!(event["state"], "preparing");
    }

    #[test]
    fn only_a_single_loopback_mapping_is_a_usable_url() {
        let parse = |json: &str| -> Option<u16> {
            let ports: HashMap<String, Option<Vec<PortBinding>>> =
                serde_json::from_str(json).expect("fixture ports");
            loopback_port(Some(&ports))
        };
        assert_eq!(
            parse(r#"{"8080/tcp":[{"HostIp":"127.0.0.1","HostPort":"44011"}]}"#),
            Some(44011)
        );
        assert_eq!(parse(r#"{"8080/tcp":null}"#), None, "unpublished");
        assert_eq!(
            parse(r#"{"8080/tcp":[{"HostIp":"0.0.0.0","HostPort":"44011"}]}"#),
            None,
            "wildcard host"
        );
        assert_eq!(
            parse(r#"{"8080/tcp":[{"HostIp":"","HostPort":"44011"}]}"#),
            None,
            "empty host"
        );
        assert_eq!(
            parse(r#"{"8080/tcp":[{"HostIp":"127.0.0.1","HostPort":"0"}]}"#),
            None,
            "port zero"
        );
        assert_eq!(
            parse(
                r#"{"8080/tcp":[{"HostIp":"127.0.0.1","HostPort":"44011"},{"HostIp":"::1","HostPort":"44012"}]}"#
            ),
            None,
            "two bindings are not exactly one"
        );
        assert_eq!(parse("{}"), None, "no keys at all");
    }

    /// Real-engine round trip: Start → Ready with a loopback proxy URL →
    /// Stop, against a live workspace, proving the whole `start` path outside
    /// the mocked unit seams. Opt-in because it drives the actual Podman
    /// engine and needs a completed session's workspace on disk:
    ///
    /// ```text
    /// POCKETS_REAL_AI_ROOT=~/Library/Application\ Support/dev.pockets.desktop/ai \
    /// POCKETS_REAL_SESSION=<32-hex session id> \
    /// cargo test --lib real_preview -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "drives the real podman engine against a real session workspace"]
    async fn real_preview_start_ready_stop_round_trip() {
        let ai_root = std::env::var("POCKETS_REAL_AI_ROOT").expect("required");
        let session = std::env::var("POCKETS_REAL_SESSION").expect("required");
        let bin = crate::runtime::find_podman().expect("podman on PATH");
        let previews = Previews::default();

        let status = start(std::path::Path::new(&ai_root), &bin, &session, &previews)
            .await
            .expect("start must reach and provision the engine");
        assert_eq!(status.state, PreviewState::Ready, "{status:?}");
        let url = status.url.expect("ready carries the proxy URL");
        assert!(is_loopback_proxy_url(&url), "{url}");

        // The proxy must actually serve the app through the loopback port.
        let probe = reqwest::get(format!("{url}/health"))
            .await
            .expect("proxy answers /health");
        assert!(probe.status().is_success(), "{probe:?}");

        // The browser's first request: an injectable HTML page, chunked and
        // keep-alive from the starter's server — the framing combination the
        // fake-upstream fixtures never produced. Must relay 200 with the
        // live-reload client injected, never a refusal.
        let page = reqwest::get(&url).await.expect("proxy answers /");
        assert_eq!(
            page.status(),
            reqwest::StatusCode::OK,
            "GET / through the proxy"
        );
        let body = page.text().await.expect("page body");
        assert!(
            body.contains(r#"<script src="/__pockets-live/client.js" defer></script>"#),
            "injected client tag missing; body starts: {}",
            &body[..body.len().min(200)]
        );

        let stopped = stop(&bin, &session, &previews)
            .await
            .expect("stop must reclaim the owned objects");
        assert!(stopped, "the registry entry was live");
    }

    #[tokio::test]
    async fn open_refuses_unknown_and_malformed_sessions() {
        let previews = Previews::default();
        let err = open(&previews, SESSION)
            .await
            .expect_err("a session that never started a preview must not open anything");
        assert!(err.contains("has no preview yet"), "{err}");

        let err = open(&previews, "not-a-session-id")
            .await
            .expect_err("a malformed session id must be refused at the boundary");
        assert!(err.contains("invalid session id"), "{err}");
    }

    #[tokio::test]
    async fn open_refuses_a_preview_that_is_not_ready() {
        let previews = Previews::default();
        let _sender = previews.begin(SESSION);
        let err = open(&previews, SESSION)
            .await
            .expect_err("preparing is not ready and must not open");
        assert!(err.contains("no ready preview"), "{err}");

        previews.transition(SESSION, PreviewState::Stopped, None, None);
        let err = open(&previews, SESSION)
            .await
            .expect_err("stopped is not ready and must not open");
        assert!(err.contains("no ready preview"), "{err}");
    }

    /// A `ready` entry whose recorded user url is corrupted must still never
    /// reach the browser: the shape gate refuses it, as it refuses a `ready`
    /// entry with no url at all. (Tests only drive refusal paths — the launch
    /// itself stays behind the validated helper and is never spawned here.)
    #[tokio::test]
    async fn open_refuses_a_corrupted_non_loopback_user_url() {
        let previews = Previews::default();
        seeded_ready(&previews, "http://127.0.0.1:40000", PROXY_URL);
        previews.lock().get_mut(SESSION).expect("seeded entry").url =
            Some("http://evil.example/".to_owned());

        let err = open(&previews, SESSION)
            .await
            .expect_err("a non-loopback url must never reach the browser");
        assert!(err.contains("was not opened"), "{err}");

        previews.lock().get_mut(SESSION).expect("seeded entry").url = None;
        let err = open(&previews, SESSION)
            .await
            .expect_err("a ready entry with no url must not open");
        assert!(err.contains("no url to open"), "{err}");
    }

    /// The escape hatch's defense-in-depth gate: only the exact proxy shape
    /// `http://127.0.0.1:<digits>` passes; every crafted variant fails.
    #[test]
    fn only_the_exact_loopback_proxy_shape_opens() {
        assert!(is_loopback_proxy_url("http://127.0.0.1:49999"));
        assert!(
            is_loopback_proxy_url("http://127.0.0.1:1"),
            "any digit port"
        );
        assert!(!is_loopback_proxy_url(""), "empty");
        assert!(!is_loopback_proxy_url("http://127.0.0.1:"), "no port");
        assert!(
            !is_loopback_proxy_url("http://127.0.0.1:49999/"),
            "trailing path"
        );
        assert!(
            !is_loopback_proxy_url("http://127.0.0.1:49999evil.example"),
            "crafted suffix"
        );
        assert!(
            !is_loopback_proxy_url("http://127.0.0.1:port"),
            "non-digit port"
        );
        assert!(
            !is_loopback_proxy_url("https://127.0.0.1:49999"),
            "any other scheme"
        );
        assert!(
            !is_loopback_proxy_url("http://localhost:49999"),
            "not the literal loopback address"
        );
        assert!(
            !is_loopback_proxy_url("http://evil.example/"),
            "foreign host"
        );
    }
}
