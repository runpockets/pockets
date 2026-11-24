//! Opt-in production watcher acceptance against an existing Podman engine.
//! Owns a fresh temp source/store, project UUID, preview container and volume.
//! No model, user application, installed volume, image build, pull, or VM
//! configuration is used. Cleanup runs even when an assertion fails.

use super::*;

const SERVER: &str = r#"const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
fs.mkdirSync(process.env.DATA_DIR, {recursive:true});
const marker = path.join(process.env.DATA_DIR, 'marker');
if (!fs.existsSync(marker)) fs.writeFileSync(marker, 'preview-data-kept');
http.createServer((req,res) => {
  if (req.url === '/health') { res.end('ok'); return; }
  if (req.url === '/marker') { res.end(fs.readFileSync(marker)); return; }
  if (req.url === '/style.css') { res.setHeader('Content-Type','text/css'); res.end(fs.readFileSync('style.css')); return; }
  if (req.url === '/secrets') { res.end(JSON.stringify(['.env','.git','data/local.json'].map(p=>fs.existsSync(p)))); return; }
  if (req.url === '/environment') { res.end(String(process.env.PREVIEW_FIXTURE_KEY === 'pockets-preview-fixture-value' && process.env.EMPTY_FIXTURE === '' && !('INSTALLED_FIXTURE_KEY' in process.env))); return; }
  if (req.url === '/api-access') {
    const base = process.env.POCKETS_HTTPS_GATEWAY;
    const denied = new URL('/fetch', base);
    denied.searchParams.set('url', 'https://unapproved.example.net/prices');
    Promise.all([fetch(new URL('/health', base)).then(r=>r.text()), fetch(denied).then(r=>r.status)])
      .then(result=>res.end(result.join('|')), ()=>res.end('gateway unavailable'));
    return;
  }
  res.setHeader('Content-Type','text/html');
  res.end('<html><head><link rel="stylesheet" href="/style.css"></head><body>__MARKER__</body></html>');
}).listen(Number(process.env.PORT), process.env.HOST);
"#;

fn atomic_write(root: &Path, file: &str, text: &str) -> Result<(), String> {
    let temporary = root.join(format!(".external-save-{}", uuid::Uuid::new_v4().simple()));
    std::fs::write(&temporary, text)
        .and_then(|_| std::fs::rename(&temporary, root.join(file)))
        .map_err(|e| e.to_string())
}

async fn wait_for(
    previews: &Previews,
    id: &str,
    wanted: PreviewState,
    ready_count: &std::sync::atomic::AtomicUsize,
    count: usize,
) -> Result<PreviewStatus, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = previews.status(id, 0) {
            if status.state == wanted && ready_count.load(Ordering::Acquire) >= count {
                return Ok(status);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Timed out waiting for {wanted:?}: {:?}",
                previews.status(id, 0)
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn body(client: &reqwest::Client, url: &str, route: &str) -> Result<String, String> {
    client
        .get(format!("{url}{route}"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())
}

#[tokio::test]
#[ignore = "requires operational Podman and cached node:24-alpine; owns only a disposable fixture"]
async fn external_edits_refresh_recover_and_stop_without_ai_or_reimporting() {
    use std::sync::atomic::AtomicUsize;
    let bin = crate::require_podman().expect("existing Podman");
    crate::runtime::run(
        &bin,
        &["image", "exists", "docker.io/library/node:24-alpine"],
    )
    .await
    .expect("cached Node image; test never pulls");
    let temp = tempfile::Builder::new()
        .prefix("pockets-project-preview-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let source = temp.path().join("external project");
    std::fs::create_dir(&source).unwrap();
    let manifest = include_str!("../../../examples/hello-pocket/pocket.yaml");
    std::fs::write(source.join("pocket.yaml"), manifest).unwrap();
    std::fs::write(source.join("Containerfile"), "FROM scratch\n").unwrap();
    std::fs::write(
        source.join("server.js"),
        SERVER.replace("__MARKER__", "first-version"),
    )
    .unwrap();
    std::fs::write(source.join("style.css"), "body { color: red; }").unwrap();
    std::fs::write(source.join(".env"), "EXCLUDED_SECRET=never-mounted").unwrap();
    std::fs::create_dir(source.join(".git")).unwrap();
    std::fs::write(
        source.join(".git/config"),
        "private repository configuration",
    )
    .unwrap();
    std::fs::create_dir(source.join("data")).unwrap();
    std::fs::write(
        source.join("data/local.json"),
        "private local application data",
    )
    .unwrap();
    let store = Store::new(temp.path().join("app-data/projects"));
    let project = crate::development::link(&store, None, &source).unwrap();
    let id = project.id.clone();
    let previews = Previews::default();
    let controller = crate::Controller::default();
    let sessions = ProjectPreviews::default();
    let allowed =
        crate::development::set_preview_hosts(&store, &id, &["example.com".into()]).unwrap();
    let access = PreviewAccess::new(
        crate::development::preview_approval_scope(&store, &id).unwrap(),
        allowed.clone(),
        &[],
    );
    let active = sessions.begin(&id, access);
    let service = PreviewService {
        store: store.clone(),
        controller: controller.clone(),
        previews: previews.clone(),
        sessions: sessions.clone(),
        on_ready: Arc::new(|_| {}),
    };
    let ready_count = Arc::new(AtomicUsize::new(0));
    let notify = ready_count.clone();
    previews.prepare(&id);
    let watcher = tokio::spawn(watch_project(
        Store::new(store.root().to_owned()),
        bin.clone(),
        controller.clone(),
        previews.clone(),
        id.clone(),
        active.clone(),
        LaunchOptions {
            variables: [
                (
                    "PREVIEW_FIXTURE_KEY".into(),
                    "pockets-preview-fixture-value".into(),
                ),
                ("EMPTY_FIXTURE".into(), String::new()),
            ]
            .into(),
            allowed_hosts: allowed,
        },
        move || {
            notify.fetch_add(1, Ordering::AcqRel);
        },
    ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let name = format!("pockets-preview-{id}");
    let result: Result<(), String> = async {
        let initial = wait_for(&previews, &id, PreviewState::Ready, &ready_count, 1).await?;
        let url = initial.url.ok_or("Ready preview has no URL.")?;
        if !body(&client, &url, "/").await?.contains("first-version") {
            return Err("Initial source not served.".into());
        }
        if body(&client, &url, "/secrets").await? != "[false,false,false]" {
            return Err("Excluded external files entered the preview mount.".into());
        }
        if body(&client, &url, "/environment").await? != "true" {
            return Err("The preview did not receive its scoped variables.".into());
        }
        if body(&client, &url, "/api-access").await? != "ready|403" {
            return Err(
                "The preview gateway was unavailable or let an unapproved hostname through.".into(),
            );
        }
        let inspect = crate::runtime::run(&bin, &["inspect", &name]).await?;
        let metadata: serde_json::Value =
            serde_json::from_slice(&inspect.stdout).map_err(|error| error.to_string())?;
        let attachments = metadata[0]["NetworkSettings"]["Networks"]
            .as_object()
            .ok_or("No network attachments were reported.")?;
        if attachments.len() != 1 || !attachments.contains_key(&format!("pockets-egress-net-{id}"))
        {
            return Err("The preview was not exclusively attached to its isolated bridge.".into());
        }
        if String::from_utf8_lossy(&inspect.stdout).contains("pockets-preview-fixture-value") {
            return Err("Podman inspect exposed a runtime variable value.".into());
        }
        let started = crate::runtime::run(
            &bin,
            &["inspect", "--format", "{{.State.StartedAt}}", &name],
        )
        .await?
        .stdout;
        atomic_write(&source, "style.css", "body { color: green; }")?;
        wait_for(&previews, &id, PreviewState::Ready, &ready_count, 2).await?;
        if !body(&client, &url, "/style.css").await?.contains("green") {
            return Err("External stylesheet was not refreshed.".into());
        }
        if crate::runtime::run(
            &bin,
            &["inspect", "--format", "{{.State.StartedAt}}", &name],
        )
        .await?
        .stdout
            != started
        {
            return Err("A stylesheet restarted the server.".into());
        }
        atomic_write(&source, "server.js", "this is invalid JavaScript !!!")?;
        wait_for(&previews, &id, PreviewState::Failed, &ready_count, 2).await?;
        atomic_write(
            &source,
            "server.js",
            &SERVER.replace("__MARKER__", "second-version"),
        )?;
        let repaired = wait_for(&previews, &id, PreviewState::Ready, &ready_count, 3).await?;
        if repaired.url.as_deref() != Some(&url)
            || !body(&client, &url, "/").await?.contains("second-version")
        {
            return Err("Recovery lost the proxy origin or new source.".into());
        }
        if body(&client, &url, "/marker").await? != "preview-data-kept" {
            return Err("Restart lost preview data.".into());
        }
        if body(&client, &url, "/environment").await? != "true" {
            return Err("Live reload lost the preview variables.".into());
        }
        atomic_write(
            &source,
            "pocket.yaml",
            &manifest
                .replace("port: 8080", "port: 8081")
                .replace("path: /data", "path: /alternate-data"),
        )?;
        let moved = wait_for(&previews, &id, PreviewState::Ready, &ready_count, 4).await?;
        if moved.url.as_deref() != Some(&url)
            || body(&client, &url, "/marker").await? != "preview-data-kept"
        {
            return Err("Runtime reconfiguration changed origin or preview data.".into());
        }
        // A failed reconfiguration may already have replaced the container.
        // Reverting its manifest must recreate the previous mapping and mount,
        // rather than restarting the failed container with its newer settings.
        atomic_write(
            &source,
            "server.js",
            &SERVER
                .replace("__MARKER__", "fixed-port")
                .replace("Number(process.env.PORT)", "8081"),
        )?;
        wait_for(&previews, &id, PreviewState::Ready, &ready_count, 5).await?;
        let moved_manifest = manifest
            .replace("port: 8080", "port: 8081")
            .replace("path: /data", "path: /alternate-data");
        atomic_write(
            &source,
            "pocket.yaml",
            &moved_manifest.replace("port: 8081", "port: 8082"),
        )?;
        wait_for(&previews, &id, PreviewState::Failed, &ready_count, 5).await?;
        atomic_write(&source, "pocket.yaml", &moved_manifest)?;
        let restored = wait_for(&previews, &id, PreviewState::Ready, &ready_count, 6).await?;
        if restored.url.as_deref() != Some(&url)
            || body(&client, &url, "/marker").await? != "preview-data-kept"
            || body(&client, &url, "/environment").await? != "true"
            || body(&client, &url, "/api-access").await? != "ready|403"
        {
            return Err("Reverting a failed configuration lost origin or preview data.".into());
        }
        let declared = format!("{moved_manifest}\npreviewApis:\n  - host: api.example.com\n    name: Pricing fixture\n    purpose: Read public prices.\n");
        atomic_write(&source, "pocket.yaml", &declared)?;
        wait_for(&previews, &id, PreviewState::Ready, &ready_count, 7).await?;
        let old = service.desktop_status(&id, 0).api_access.ok_or("The running preview did not request access.")?;
        let new_request = declared.replace("api.example.com", "prices.example.com");
        {
            let _lock = controller.lifecycle_guard().await;
            atomic_write(&source, "pocket.yaml", &new_request)?;
            let refreshed = service.review_api_access_locked(&id, ApiDecision { token: old.token, allow: true }).await?;
            let current = refreshed.api_access.ok_or("The edited API request was not returned for review.")?;
            if current.services[0].host != "prices.example.com" || crate::development::preview_hosts(&store, &id)? != ["example.com"] {
                return Err("A stale approval changed live API access.".into());
            }
            service.review_api_access_locked(&id, ApiDecision { token: current.token, allow: true }).await?;
        }
        let after_approval = wait_for(&previews, &id, PreviewState::Ready, &ready_count, 8).await?;
        if after_approval.url.as_deref() != Some(&url)
            || body(&client, &url, "/marker").await? != "preview-data-kept"
            || body(&client, &url, "/environment").await? != "true"
            || service.desktop_status(&id, 0).api_access.is_some()
            || crate::development::preview_hosts(&store, &id)? != ["example.com", "prices.example.com"] {
            return Err("Applying API access changed preview data, variables, URL, or approval state.".into());
        }
        let gateway = format!("pockets-egress-{id}");
        let command = crate::runtime::run(&bin, &["container", "inspect", "--format", "{{json .Config.Cmd}}", &gateway]).await?;
        let arguments: Vec<String> = serde_json::from_slice(&command.stdout).map_err(|error| error.to_string())?;
        if arguments.last().map(String::as_str) != Some(r#"["example.com","prices.example.com"]"#) {
            return Err("The gateway did not pick up the approved hostname.".into());
        }
        if store.load(&id)? != project {
            return Err("Preview edits changed the authoritative installation snapshot.".into());
        }
        if temp.path().join("app-data/ai").exists() {
            return Err("Preview unexpectedly allocated AI storage.".into());
        }
        sessions.cancel(&id);
        {
            let _lock = controller.lifecycle_guard().await;
            preview::stop(&bin, &id, &previews).await?;
        }
        let filter = format!("name=^pockets-runtime-preview-{id}-");
        let remaining =
            crate::runtime::run(&bin, &["secret", "ls", "--filter", &filter, "--quiet"]).await?;
        if !remaining
            .stdout
            .iter()
            .all(|byte| byte.is_ascii_whitespace())
        {
            return Err("Stop retained preview credentials in Podman.".into());
        }
        let container = format!("name=^pockets-egress-{id}$");
        let result = crate::runtime::run(
            &bin,
            &[
                "ps",
                "--all",
                "--filter",
                &container,
                "--format",
                "{{.Names}}",
            ],
        )
        .await?;
        let network = format!("name=^pockets-egress-net-{id}$");
        let network = crate::runtime::run(
            &bin,
            &[
                "network",
                "ls",
                "--filter",
                &network,
                "--format",
                "{{.Name}}",
            ],
        )
        .await?;
        if [&result.stdout, &network.stdout]
            .iter()
            .any(|output| output.iter().any(|byte| !byte.is_ascii_whitespace()))
        {
            return Err("Stop retained HTTPS gateway resources in Podman.".into());
        }
        atomic_write(
            &source,
            "server.js",
            &SERVER.replace("__MARKER__", "must-stay-stopped"),
        )?;
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        if previews.status(&id, 0).map(|s| s.state) != Some(PreviewState::Stopped) {
            return Err("An external edit resurrected the stopped preview.".into());
        }
        Ok(())
    }
    .await;
    sessions.cancel(&id);
    let joined = tokio::time::timeout(Duration::from_secs(10), watcher).await;
    let cleanup = {
        let _lock = controller.lifecycle_guard().await;
        preview::stop(&bin, &id, &previews).await
    };
    assert!(joined.is_ok(), "Watcher did not stop after cancellation.");
    assert!(cleanup.is_ok(), "Owned preview cleanup failed: {cleanup:?}");
    assert!(
        result.is_ok(),
        "External preview acceptance failed: {result:?}"
    );
}

#[tokio::test]
#[ignore = "requires operational Podman and cached node:24-alpine; owns only a disposable fixture"]
async fn desktop_created_starter_runs_and_refreshes_without_waiting_for_api_approval() {
    use std::sync::atomic::AtomicUsize;
    let bin = crate::require_podman().expect("existing Podman");
    let temp = tempfile::Builder::new()
        .prefix("pockets-created-preview-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let store = Store::new(temp.path().join("app-data/projects"));
    let project = crate::scaffold::create(
        &store,
        temp.path(),
        "New Pocket",
        "new-pocket",
        "Build a small app",
    )
    .unwrap();
    let source = crate::development::folder(&store, &project.id).unwrap();
    let manifest = std::fs::read_to_string(source.join("pocket.yaml")).unwrap();
    atomic_write(
        &source,
        "pocket.yaml",
        &format!("{manifest}\npreviewApis:\n  - host: prices.example.com\n    name: Pricing fixture\n    purpose: Read public prices.\n"),
    )
    .unwrap();
    let id = project.id.clone();
    let previews = Previews::default();
    let controller = crate::Controller::default();
    let count = Arc::new(AtomicUsize::new(0));
    let notify = count.clone();
    let service = PreviewService {
        store: store.clone(),
        controller: controller.clone(),
        previews: previews.clone(),
        sessions: ProjectPreviews::default(),
        on_ready: Arc::new(move |_| {
            notify.fetch_add(1, Ordering::AcqRel);
        }),
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let result: Result<(), String> = async {
        let start = {
            let _lock = controller.lifecycle_guard().await;
            service.start_locked(id.clone()).await?
        };
        let review = start
            .api_access
            .ok_or("Start did not include the API request.")?;
        if start.status.state != PreviewState::Preparing
            || review.services[0].host != "prices.example.com"
        {
            return Err("Start waited for approval or returned the wrong request.".into());
        }
        let initial = wait_for(&previews, &id, PreviewState::Ready, &count, 1).await?;
        let url = initial.url.ok_or("Ready starter has no URL.")?;
        if !body(&client, &url, "/")
            .await?
            .contains("Hello from your Pocket")
        {
            return Err("The bundled starter did not serve its page.".into());
        }
        if service
            .desktop_status(&id, 0)
            .api_access
            .as_ref()
            .map(|pending| &pending.token)
            != Some(&review.token)
            || !crate::development::preview_hosts(&store, &id)?.is_empty()
        {
            return Err(
                "The preview lost the pending request or approved it without a decision.".into(),
            );
        }
        {
            let _lock = controller.lifecycle_guard().await;
            let ignored = service
                .review_api_access_locked(
                    &id,
                    ApiDecision {
                        token: review.token,
                        allow: false,
                    },
                )
                .await?;
            if ignored.api_access.is_some() || ignored.status.url.as_deref() != Some(&url) {
                return Err("Ignoring the request changed the running preview.".into());
            }
        }
        if !preview::logs(&bin, &id).await?.contains("pocket listening") {
            return Err(
                "The project-owned preview logs did not include the starter output.".into(),
            );
        }
        let code = std::fs::read_to_string(source.join("server.js")).map_err(|e| e.to_string())?;
        atomic_write(
            &source,
            "server.js",
            &code.replace("Hello from your Pocket", "Changed in my coding tool"),
        )?;
        let changed = wait_for(&previews, &id, PreviewState::Ready, &count, 2).await?;
        if changed.url.as_deref() != Some(&url)
            || !body(&client, &url, "/")
                .await?
                .contains("Changed in my coding tool")
            || service.desktop_status(&id, 0).api_access.is_some()
        {
            return Err(
                "Saved starter edits changed the preview URL or reopened the ignored request."
                    .into(),
            );
        }
        if store.load(&id)? != project || temp.path().join("app-data/ai").exists() {
            return Err(
                "Starter preview changed the install snapshot or allocated AI storage.".into(),
            );
        }
        Ok(())
    }
    .await;
    let cleanup = {
        let _lock = controller.lifecycle_guard().await;
        service.stop_locked(&id).await
    };
    assert!(cleanup.is_ok(), "Starter cleanup failed: {cleanup:?}");
    assert!(
        result.is_ok(),
        "Created starter acceptance failed: {result:?}"
    );
}
