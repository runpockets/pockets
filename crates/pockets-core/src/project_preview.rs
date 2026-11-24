//! Project-owned preview coordination. Host polling observes external edits;
//! only filtered, validated bytes enter the private development mirror.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::manifest::{Manifest, PreviewApi};
use crate::AppContext;
use crate::preview::{self, LaunchOptions, PreviewState, PreviewStatus, Previews};
use crate::preview_source::MirroredSource;
use crate::projects::{Project, SourceSummary, Store};

const POLL: Duration = Duration::from_millis(400);
const RECONCILE: Duration = Duration::from_secs(15);

#[derive(Clone, Default)]
pub struct ProjectPreviews {
    sessions: Arc<Mutex<HashMap<String, Arc<PreviewSession>>>>,
}

struct PreviewSession {
    active: AtomicBool,
    access: Mutex<PreviewAccess>,
}

#[derive(Clone, Serialize)]
struct ApiReview {
    token: String,
    services: Vec<PreviewApi>,
}

#[derive(Deserialize)]
pub struct ApiDecision {
    pub token: String,
    pub allow: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopPreviewStatus {
    #[serde(flatten)]
    status: PreviewStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_access: Option<ApiReview>,
}

struct PreviewAccess {
    link_scope: String,
    session_scope: String,
    allowed: Vec<String>,
    dismissed: BTreeSet<String>,
    pending: Option<ApiReview>,
}

impl PreviewAccess {
    fn new(link_scope: String, allowed: Vec<String>, apis: &[PreviewApi]) -> Self {
        let mut access = Self {
            session_scope: format!("{link_scope}/{}", uuid::Uuid::new_v4()),
            link_scope,
            allowed,
            dismissed: BTreeSet::new(),
            pending: None,
        };
        access.pending = access.review(apis);
        access
    }

    fn review(&self, apis: &[PreviewApi]) -> Option<ApiReview> {
        api_review(&self.session_scope, apis, &self.allowed, &self.dismissed)
    }
}

fn api_review(
    scope: &str,
    apis: &[PreviewApi],
    allowed: &[String],
    dismissed: &BTreeSet<String>,
) -> Option<ApiReview> {
    let mut services: Vec<_> = apis
        .iter()
        .filter(|api| !allowed.contains(&api.host) && !dismissed.contains(&api.host))
        .cloned()
        .collect();
    if services.is_empty() {
        return None;
    }
    services.sort_by(|a, b| a.host.cmp(&b.host));
    let intent =
        serde_json::to_vec(&(scope, &services)).expect("API requests contain only strings");
    Some(ApiReview {
        token: format!("{:x}", Sha256::digest(intent)),
        services,
    })
}

impl PreviewSession {
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn access(&self) -> std::sync::MutexGuard<'_, PreviewAccess> {
        self.access.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ProjectPreviews {
    pub(crate) fn active(&self, id: &str) -> bool {
        self.session(id).is_some_and(|session| session.is_active())
    }

    fn session(&self, id: &str) -> Option<Arc<PreviewSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }
    fn begin(&self, id: &str, access: PreviewAccess) -> Arc<PreviewSession> {
        let session = Arc::new(PreviewSession {
            active: AtomicBool::new(true),
            access: Mutex::new(access),
        });
        if let Some(previous) = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.into(), session.clone())
        {
            previous.active.store(false, Ordering::Release);
        }
        session
    }
    pub(crate) fn cancel(&self, id: &str) {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
        {
            session.active.store(false, Ordering::Release);
        }
    }
}

/// Two equal consecutive host signatures form a quiet edit batch. A change
/// during a restart is seen on the following tick; no event queue is lost.
#[derive(Default)]
struct ChangeGate {
    observed: Option<String>,
    attempted: Option<String>,
}
impl ChangeGate {
    fn observe(&mut self, signature: String) -> bool {
        if self.observed.as_ref() != Some(&signature) {
            self.observed = Some(signature);
            return false;
        }
        if self.attempted.as_ref() == Some(&signature) {
            return false;
        }
        self.attempted = Some(signature);
        true
    }
    fn reset(&mut self) {
        self.observed = None;
        self.attempted = None;
    }
}

#[derive(Default)]
struct Progress {
    attempted: Option<String>,
    successful: Option<String>,
    manifest: Option<Manifest>,
    dirty: BTreeSet<String>,
    allowed_hosts: Vec<String>,
    /// A failed provision may already have changed the container's port or
    /// mount. Recreate on the next edit even when it restores the old manifest.
    reconfigure: bool,
}

fn validate_profile(files: &[(String, Vec<u8>)]) -> Result<(), String> {
    if !files.iter().any(|(path, _)| path == "server.js") {
        return Err("This preview currently needs a root server.js. Development profiles for other entry points are a later milestone; Build remains available.".into());
    }
    if let Some((_, bytes)) = files.iter().find(|(path, _)| path == "package.json") {
        let package: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| format!("package.json is invalid: {e}"))?;
        if !package.is_object() {
            return Err("package.json must be an object.".into());
        }
        for field in ["dependencies", "optionalDependencies"] {
            if let Some(dependencies) = package.get(field) {
                if !dependencies.is_object()
                    || dependencies.as_object().is_some_and(|map| !map.is_empty())
                {
                    return Err("This preview currently supports dependency-free Node apps. Dependency-aware development profiles are a later milestone; Build remains available.".into());
                }
            }
        }
    }
    Ok(())
}

struct CapturedSource {
    files: Vec<(String, Vec<u8>)>,
    manifest: Manifest,
    summary: SourceSummary,
}

fn capture_linked(store: &Store, project: &Project) -> Result<CapturedSource, String> {
    let folder = crate::development::folder(store, &project.id)?;
    let captured = crate::source::capture_approved(&folder, "the linked source");
    let current_folder = crate::development::folder(store, &project.id)?;
    if current_folder != folder {
        return Err("The linked folder changed during capture.".into());
    }
    let (files, manifest, summary) = captured?;
    if manifest.metadata.id != project.manifest.metadata.id {
        return Err(
            "The linked manifest's metadata.id changed. Restore it to preview this Pocket.".into(),
        );
    }
    validate_profile(&files)?;
    Ok(CapturedSource {
        files,
        manifest,
        summary,
    })
}

/// Refreshes the preview without changing the project's working snapshot,
/// installed image, revisions, external files, or application data volume.
/// Callers hold Controller through capture, mirror publication and refresh.
async fn update(
    store: &Store,
    bin: &Path,
    id: &str,
    progress: &mut Progress,
    previews: &Previews,
    session: &PreviewSession,
    options: &LaunchOptions,
) -> Result<bool, String> {
    let root = store.root().to_owned();
    let project_id = id.to_owned();
    let captured = tokio::task::spawn_blocking(move || {
        let store = Store::new(root);
        let project = store.load(&project_id)?;
        let CapturedSource {
            files,
            manifest,
            summary,
        } = capture_linked(&store, &project)?;
        Ok::<_, String>((files, manifest, summary.source_digest))
    })
    .await
    .map_err(|_| "Capturing development source was interrupted.".to_owned())??;
    if !session.is_active() {
        return Ok(false);
    }
    let (files, manifest, digest) = captured;
    {
        let mut access = session.access();
        access.pending = access.review(&manifest.preview_apis);
    }
    let network_changed = progress.allowed_hosts != options.allowed_hosts;
    let ready = previews
        .status(id, 0)
        .is_some_and(|status| status.state == PreviewState::Ready);
    if !network_changed && progress.successful.as_ref() == Some(&digest) && ready {
        return Ok(false);
    }
    if !network_changed
        && progress.attempted.as_ref() == Some(&digest)
        && progress.successful.as_ref() != Some(&digest)
    {
        return Ok(false);
    }
    let workspace_path = store.source_path(id)?.with_file_name("development-source");
    let (source, changed) =
        tokio::task::spawn_blocking(move || MirroredSource::sync(&workspace_path, &files))
            .await
            .map_err(|_| "Preparing development source was interrupted.".to_owned())??;
    progress.dirty.extend(changed);
    progress.attempted = Some(digest.clone());
    if !session.is_active() {
        return Ok(false);
    }
    let reconfigure = progress.manifest.as_ref().is_some_and(|previous| {
        previous.web.port != manifest.web.port || previous.storage.path != manifest.storage.path
    });
    let configuring =
        progress.reconfigure || reconfigure || network_changed || progress.manifest.is_none();
    progress.allowed_hosts.clone_from(&options.allowed_hosts);
    let outcome = if configuring || previews.status(id, 0).is_none() {
        // A freshly started watcher must not adopt a container with settings
        // from another session, especially a preview launched by Desktop.
        preview::reconfigure_source(bin, id, &source, previews, options).await
    } else {
        preview::refresh_source(
            bin,
            id,
            &source,
            &progress.dirty.iter().cloned().collect::<Vec<_>>(),
            previews,
            options,
        )
        .await
    };
    if let Err(error) = outcome {
        progress.reconfigure |= configuring;
        return Err(error);
    }
    progress.reconfigure = false;
    progress.successful = Some(digest);
    progress.manifest = Some(manifest);
    progress.dirty.clear();
    Ok(true)
}

fn report(previews: &Previews, id: &str, error: &str) {
    if previews
        .status(id, 0)
        .is_none_or(|status| status.error.as_deref() != Some(error))
    {
        previews.fail(id, error);
    }
}

async fn watch_project(
    store: Store,
    bin: PathBuf,
    controller: crate::Controller,
    previews: Previews,
    id: String,
    session: Arc<PreviewSession>,
    mut options: LaunchOptions,
    on_ready: impl Fn() + Send + Sync + 'static,
) {
    let mut gate = ChangeGate::default();
    let mut progress = Progress::default();
    let mut reconciled = Instant::now();
    while session.is_active() {
        let root = store.root().to_owned();
        let project_id = id.clone();
        let signature = tokio::task::spawn_blocking(move || {
            let store = Store::new(root);
            crate::source::source_signature(&crate::development::folder(&store, &project_id)?)
        })
        .await
        .map_err(|_| "Inspecting development source was interrupted.".to_owned())
        .and_then(|result| result);
        match signature {
            Ok(signature) => {
                let changed = gate.observe(signature);
                let network_changed = session.access().allowed != options.allowed_hosts;
                if changed || network_changed || reconciled.elapsed() >= RECONCILE {
                    reconciled = Instant::now();
                    let _lifecycle = controller.lifecycle_guard().await;
                    if !session.is_active() {
                        break;
                    }
                    options.allowed_hosts.clone_from(&session.access().allowed);
                    match update(
                        &store,
                        &bin,
                        &id,
                        &mut progress,
                        &previews,
                        &session,
                        &options,
                    )
                    .await
                    {
                        Err(error) => report(&previews, &id, &error),
                        Ok(true) => on_ready(),
                        Ok(false) => {}
                    }
                }
            }
            Err(error) => {
                let _lifecycle = controller.lifecycle_guard().await;
                if !session.is_active() {
                    break;
                }
                gate.reset();
                report(&previews, &id, &error);
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_review_only_covers_new_hosts_and_is_scoped_to_the_shown_details() {
        let approved = [String::from("approved.example.com")];
        let dismissed = BTreeSet::from([String::from("dismissed.example.com")]);
        let make = |host: &str| PreviewApi {
            host: host.into(),
            name: "Pricing".into(),
            purpose: "Read prices".into(),
        };
        let mut requests = vec![
            make("approved.example.com"),
            make("new.example.com"),
            make("dismissed.example.com"),
        ];
        let review = api_review("linked-folder-one", &requests, &approved, &dismissed).unwrap();
        assert_eq!(review.services, [make("new.example.com")]);
        assert_eq!(
            with_approved_hosts(&approved, &review),
            ["approved.example.com", "new.example.com"]
        );
        let relinked = api_review("linked-folder-two", &requests, &approved, &dismissed).unwrap();
        assert_ne!(review.token, relinked.token);
        requests[1].purpose = "Read different prices".into();
        let edited = api_review("linked-folder-one", &requests, &approved, &dismissed).unwrap();
        assert_ne!(review.token, edited.token);
    }

    #[test]
    fn edit_batches_settle_and_superseded_watchers_cannot_resume_after_stop() {
        let mut gate = ChangeGate::default();
        assert!(!gate.observe("first edit".into()));
        assert!(!gate.observe("second edit".into()));
        assert!(gate.observe("second edit".into()));
        assert!(!gate.observe("second edit".into()));
        gate.reset();
        assert!(!gate.observe("second edit".into()));
        assert!(gate.observe("second edit".into()));
        let watchers = ProjectPreviews::default();
        let access = || PreviewAccess::new("test folder".into(), Vec::new(), &[]);
        let old = watchers.begin("project", access());
        let next = watchers.begin("project", access());
        assert!(!old.is_active());
        assert!(next.is_active());
        watchers.cancel("project");
        assert!(!next.is_active());
        assert!(!watchers.active("project"));
    }

    #[test]
    fn profile_errors_are_explicit_and_metadata_watch_ignores_secrets_and_caches() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("server.js"), "one").unwrap();
        let original = crate::source::source_signature(temp.path()).unwrap();
        std::fs::write(temp.path().join(".env"), "SECRET=private").unwrap();
        std::fs::create_dir(temp.path().join("node_modules")).unwrap();
        std::fs::write(temp.path().join("node_modules/cache"), "unrelated").unwrap();
        assert_eq!(
            crate::source::source_signature(temp.path()).unwrap(),
            original
        );
        std::fs::write(temp.path().join("atomic.js"), "two").unwrap();
        std::fs::rename(temp.path().join("atomic.js"), temp.path().join("server.js")).unwrap();
        assert_ne!(
            crate::source::source_signature(temp.path()).unwrap(),
            original
        );
        let files = vec![
            ("server.js".into(), b"one".to_vec()),
            (
                "package.json".into(),
                br#"{"dependencies":{"a":"1"}}"#.to_vec(),
            ),
        ];
        assert!(validate_profile(&files)
            .unwrap_err()
            .contains("dependency-free"));
        assert!(validate_profile(&[
            ("server.js".into(), b"one".to_vec()),
            (
                "package.json".into(),
                br#"{"devDependencies":{"formatter":"1"}}"#.to_vec()
            )
        ])
        .is_ok());
        assert!(validate_profile(&[]).unwrap_err().contains("server.js"));
    }

    #[test]
    fn read_preview_network_prefers_live_linked_manifest_and_falls_back_to_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("linked-pocket");
        std::fs::create_dir(&root).unwrap();
        let base_manifest = "\
apiVersion: pockets.dev/v1alpha1
kind: Pocket
metadata:
  id: pricing-pocket
  name: Pricing Pocket
  version: 0.1.0
  description: Shows live pricing.
runtime:
  image: pockets-local/pricing-pocket:dev
  architectures:
    - arm64
    - amd64
web:
  port: 8080
  healthcheck: /health
storage:
  name: data
  path: /data
";
        let initial_manifest = format!(
            "{base_manifest}previewApis:\n  - host: api.initial.example\n    name: Initial Feed\n    purpose: Seed price data.\n"
        );
        std::fs::write(root.join("pocket.yaml"), &initial_manifest).unwrap();
        std::fs::write(root.join("Containerfile"), "FROM node:22-alpine\n").unwrap();
        std::fs::write(root.join("server.js"), "console.log('ok');\n").unwrap();

        let store = Store::new(temp.path().join("app-data/projects"));
        let project = crate::development::link(&store, None, &root).unwrap();
        crate::development::set_preview_hosts(
            &store,
            &project.id,
            &["api.livecoinwatch.com".into(), "manual.example.com".into()],
        )
        .unwrap();

        // Live edit in the linked folder before syncing source.
        let updated_manifest = format!(
            "{base_manifest}previewApis:\n  - host: api.livecoinwatch.com\n    name: Live Coin Watch\n    purpose: Fetch live prices.\n"
        );
        std::fs::write(root.join("pocket.yaml"), &updated_manifest).unwrap();

        let network = read_preview_network(&store, &project.id).unwrap();
        assert_eq!(
            network.hosts,
            vec!["api.livecoinwatch.com", "manual.example.com"]
        );
        assert_eq!(
            network.preview_apis,
            vec![PreviewApi {
                host: "api.livecoinwatch.com".into(),
                name: "Live Coin Watch".into(),
                purpose: "Fetch live prices.".into(),
            }]
        );

        // A mid-edit syntax error in the linked pocket.yaml falls back to the
        // stored snapshot manifest instead of failing Allowed hostnames.
        std::fs::write(root.join("pocket.yaml"), "previewApis: [broken").unwrap();
        let fallback = read_preview_network(&store, &project.id).unwrap();
        assert_eq!(fallback.hosts, network.hosts);
        assert_eq!(
            fallback.preview_apis,
            vec![PreviewApi {
                host: "api.initial.example".into(),
                name: "Initial Feed".into(),
                purpose: "Seed price data.".into(),
            }]
        );
    }
}

#[cfg(test)]
#[path = "project_preview_live.rs"]
mod live_tests;

#[derive(Clone)]
struct PreviewService {
    store: Store,
    controller: crate::Controller,
    previews: Previews,
    sessions: ProjectPreviews,
    on_ready: Arc<dyn Fn(String) + Send + Sync>,
}

struct PreviewIntent {
    scope: String,
    approved: Vec<String>,
    apis: Vec<PreviewApi>,
}

async fn preview_intent(store: &Store, id: &str) -> Result<PreviewIntent, String> {
    let store = store.clone();
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || {
        let project = store.load(&id)?;
        let scope = crate::development::preview_approval_scope(&store, &id)?;
        let capture = capture_linked(&store, &project)?;
        let approved = crate::development::preview_hosts(&store, &id)?;
        if crate::development::preview_approval_scope(&store, &id)? != scope {
            return Err("The linked folder changed. Review API access again.".into());
        }
        Ok(PreviewIntent {
            scope,
            approved,
            apis: capture.manifest.preview_apis,
        })
    })
    .await
    .map_err(|_| "Reviewing the preview's API access was interrupted.".to_owned())?
}

fn with_approved_hosts(existing: &[String], review: &ApiReview) -> Vec<String> {
    existing
        .iter()
        .cloned()
        .chain(review.services.iter().map(|api| api.host.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

impl PreviewService {
    fn from_ctx(ctx: &AppContext) -> Self {
        Self {
            store: ctx.store(),
            controller: ctx.controller.clone(),
            previews: ctx.previews.clone(),
            sessions: ctx.project_previews.clone(),
            on_ready: Arc::new(|_| {}),
        }
    }

    fn already_starting_or_live(&self, id: &str) -> bool {
        self.sessions.active(id)
            && self.previews.status(id, 0).is_some_and(|status| {
                matches!(
                    status.state,
                    PreviewState::Ready | PreviewState::Preparing | PreviewState::Updating
                )
            })
    }

    /// Caller holds the Controller lifecycle guard.
    async fn start_locked(&self, id: String) -> Result<DesktopPreviewStatus, String> {
        if self.already_starting_or_live(&id) {
            return Ok(self.desktop_status(&id, 0));
        }
        let intent = preview_intent(&self.store, &id).await?;
        let bin = crate::require_podman()?;
        let allowed_hosts = intent.approved.clone();
        let access = PreviewAccess::new(intent.scope, intent.approved, &intent.apis);
        let session = self.sessions.begin(&id, access);
        self.previews.prepare(&id);
        let status = self.desktop_status(&id, 0);
        let service = self.clone();
        tokio::spawn(async move {
            let values = match crate::runtime_settings::launch_values(
                &service.store,
                &id,
                crate::runtime_settings::Environment::Preview,
            )
            .await
            {
                Ok(values) => values,
                Err(error) => {
                    let _guard = service.controller.lifecycle_guard().await;
                    if session.is_active() {
                        report(&service.previews, &id, &error);
                    }
                    return;
                }
            };
            let capture = service.on_ready;
            let capture_id = id.clone();
            watch_project(
                service.store,
                bin,
                service.controller,
                service.previews,
                id,
                session,
                LaunchOptions {
                    variables: values,
                    allowed_hosts,
                },
                move || capture(capture_id.clone()),
            )
            .await;
        });
        Ok(status)
    }

    fn status(&self, id: &str, cursor: u64) -> PreviewStatus {
        self.previews.status(id, cursor).unwrap_or(PreviewStatus {
            state: PreviewState::Stopped,
            revision: 0,
            url: None,
            events_since: Vec::new(),
            next_cursor: 0,
            error: None,
        })
    }

    fn desktop_status(&self, id: &str, cursor: u64) -> DesktopPreviewStatus {
        DesktopPreviewStatus {
            status: self.status(id, cursor),
            api_access: self
                .sessions
                .session(id)
                .and_then(|session| session.access().pending.clone()),
        }
    }

    async fn review_api_access_locked(
        &self,
        project_id: &str,
        decision: ApiDecision,
    ) -> Result<DesktopPreviewStatus, String> {
        let session = self
            .sessions
            .session(project_id)
            .filter(|session| session.is_active())
            .ok_or("The preview was stopped. Start it to review API access again.")?;
        let intent = preview_intent(&self.store, project_id).await?;
        {
            let mut access = session.access();
            if access.link_scope != intent.scope {
                return Err(
                    "The linked folder changed. Start the preview again to review API access."
                        .into(),
                );
            }
            let pending = access.review(&intent.apis);
            if let Some(review) = pending
                .as_ref()
                .filter(|review| review.token == decision.token)
            {
                if decision.allow {
                    crate::development::set_preview_hosts(
                        &self.store,
                        project_id,
                        &with_approved_hosts(&intent.approved, review),
                    )?;
                    access.allowed = with_approved_hosts(&access.allowed, review);
                } else {
                    access
                        .dismissed
                        .extend(review.services.iter().map(|api| api.host.clone()));
                }
                access.pending = access.review(&intent.apis);
            } else {
                access.pending = pending;
            }
        }
        Ok(self.desktop_status(project_id, 0))
    }

    /// Caller holds Controller. Cancel queued watcher work before cleanup.
    async fn stop_locked(&self, id: &str) -> Result<bool, String> {
        self.sessions.cancel(id);
        if self.status(id, 0).state == PreviewState::Stopped {
            return Ok(false);
        }
        preview::stop(&crate::require_podman()?, id, &self.previews).await
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewNetwork {
    pub hosts: Vec<String>,
    pub preview_apis: Vec<PreviewApi>,
}

const MAX_MANIFEST_BYTES: u64 = 128 * 1024;

/// Reads `previewApis` from the verified linked folder's live `pocket.yaml`
/// when it is a safe, bounded regular file belonging to this project, falling
/// back to the stored snapshot manifest if the file is mid-edit or unreadable.
fn read_linked_manifest_apis(folder: &Path, expected_id: &str) -> Option<Vec<PreviewApi>> {
    use std::io::Read;

    let path = folder.join("pocket.yaml");
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    let file = std::fs::File::open(&path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return None;
    }
    let yaml = String::from_utf8(bytes).ok()?;
    let manifest = crate::manifest::parse_manifest(&yaml).ok()?;
    if manifest.metadata.id != expected_id {
        return None;
    }
    Some(manifest.preview_apis)
}

fn read_preview_network(store: &Store, project_id: &str) -> Result<PreviewNetwork, String> {
    let hosts = crate::development::preview_hosts(store, project_id)?;
    let preview_apis = match store.load(project_id) {
        Ok(project) => crate::development::folder(store, project_id)
            .ok()
            .and_then(|folder| read_linked_manifest_apis(&folder, &project.manifest.metadata.id))
            .unwrap_or(project.manifest.preview_apis),
        Err(_) => Vec::new(),
    };
    Ok(PreviewNetwork {
        hosts,
        preview_apis,
    })
}

pub async fn project_preview_network(
    ctx: &AppContext,
    project_id: String,
) -> Result<PreviewNetwork, String> {
    let store = ctx.store();
    tokio::task::spawn_blocking(move || read_preview_network(&store, &project_id))
        .await
        .map_err(|_| "Reading preview API access was interrupted.".to_owned())?
}

pub async fn set_project_preview_network(
    ctx: &AppContext,
    project_id: String,
    hosts: Vec<String>,
) -> Result<Vec<String>, String> {
    let service = PreviewService::from_ctx(ctx);
    let _lifecycle = service.controller.lifecycle_guard().await;
    if service.sessions.active(&project_id)
        || service.status(&project_id, 0).state != PreviewState::Stopped
    {
        return Err("Stop the preview before changing its API access.".into());
    }
    crate::development::set_preview_hosts(&service.store, &project_id, &hosts)
}

pub async fn project_preview_start(
    ctx: &AppContext,
    project_id: String,
) -> Result<DesktopPreviewStatus, String> {
    let service = PreviewService::from_ctx(ctx);
    let _lifecycle = service.controller.lifecycle_guard().await;
    service.start_locked(project_id).await
}

pub async fn project_preview_review_api_access(
    ctx: &AppContext,
    project_id: String,
    decision: ApiDecision,
) -> Result<DesktopPreviewStatus, String> {
    let service = PreviewService::from_ctx(ctx);
    let _lifecycle = service.controller.lifecycle_guard().await;
    service
        .review_api_access_locked(&project_id, decision)
        .await
}

pub async fn project_preview_status(
    ctx: &AppContext,
    project_id: String,
    cursor: u64,
) -> Result<DesktopPreviewStatus, String> {
    let service = PreviewService::from_ctx(ctx);
    service.store.load(&project_id)?;
    Ok(service.desktop_status(&project_id, cursor))
}

pub(crate) async fn stop_owned(ctx: &AppContext, id: &str) -> Result<bool, String> {
    PreviewService::from_ctx(ctx).stop_locked(id).await
}

pub async fn project_preview_stop(ctx: &AppContext, project_id: String) -> Result<bool, String> {
    let service = PreviewService::from_ctx(ctx);
    service.store.load(&project_id)?;
    service.sessions.cancel(&project_id);
    let _lifecycle = service.controller.lifecycle_guard().await;
    service.stop_locked(&project_id).await
}

pub async fn project_preview_open(ctx: &AppContext, project_id: String) -> Result<(), String> {
    ctx.store().load(&project_id)?;
    preview::open(&ctx.previews, &project_id).await
}

pub async fn project_preview_logs(ctx: &AppContext, project_id: String) -> Result<String, String> {
    let store = ctx.store();
    store.load(&project_id)?;
    let _lifecycle = ctx.controller.lifecycle_guard().await;
    if ctx
        .previews
        .status(&project_id, 0)
        .is_none_or(|status| status.state == PreviewState::Stopped)
    {
        return Ok(String::new());
    }
    preview::logs(&crate::require_podman()?, &project_id).await
}
