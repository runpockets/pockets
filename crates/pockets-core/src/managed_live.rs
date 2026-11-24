//! Opt-in live integration test for the full managed-project pipeline.
//!
//! The single ignored test here drives one isolated fixture through the
//! production native helpers end to end: [`source::import_project`] of the
//! repository's own `examples/personal-notes` folder into a temporary
//! store, [`creation::build_approved_project`] (which installs the built
//! record as `Installed` while leaving it unpreviewed),
//! [`project_lifecycle::start`] to a healthy inspected loopback view, a
//! real form save over HTTP, a service
//! stop, a fresh `Store` over the same temporary root, and a second start
//! that recreates the container and must still serve the saved note from
//! the named volume.
//!
//! Isolation: everything this test creates is derived from its own freshly
//! generated project UUID — container `pockets-project-<id>`, volume
//! `pockets-project-<id>-data`, and image tag
//! `localhost/pockets-managed-<id>:build`. The bundled Hello Pocket, any
//! existing Personal Notes installation, the user's own processes, and the
//! Podman machine are never touched. The original example folder is only
//! ever read through the import capture rules, so its excluded `data/`
//! directory is never stat'ed, opened, or copied.
//!
//! Scoped cleanup runs on every path (the flow is a `Result`, never a
//! panic, so cleanup is always reached): it locates the container by exact
//! name through `podman ps` — whose zero exit on no match keeps real engine
//! failures distinguishable from absence — stops and removes it by the
//! inspected ID after verifying the exact name (with optional leading
//! slash) and the `dev.pockets.managed=project-<id>` label, removes the
//! UUID-derived volume only after the same label check, and removes exactly
//! the one unique `localhost/pockets-managed-<id>:build` tag — never an
//! image by ID, never a prune, never any other object. Temporary store
//! files are cleaned up by `TempDir` itself.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

#[path = "managed_update_live.rs"]
mod managed_update_live;

use crate::builder::{BuildProgressHandle, BuildState};
use crate::pocket::PocketState;
use crate::projects::{Phase, Store};
use crate::{creation, local_sites, project_lifecycle, runtime, source};

/// Ownership label Pockets writes on its containers and volumes. Duplicated
/// as a literal here because the production constant in `pocket` is
/// module-private on purpose; the live test only ever verifies it.
const OWNERSHIP_LABEL: &str = "dev.pockets.managed";

/// The repository's own Personal Notes example, resolved from the crate
/// manifest directory (no dependence on the test working directory).
fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../examples/personal-notes")
}

/// Bounded loopback-only HTTP client: no proxy, no redirects, 5-second
/// timeout, mirroring the production probe restrictions so the test can
/// neither hang nor leave loopback.
fn live_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|err| format!("could not build the live HTTP client: {err}"))
}

/// GET `url` and return the response body text.
async fn get(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("GET {url} failed: {err}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|err| format!("GET {url} response body could not be read: {err}"))?;
    if !status.is_success() {
        return Err(format!("GET {url} returned {status}"));
    }
    Ok(body)
}

/// The value of the Notes home page's hidden CSRF input
/// (`<input type="hidden" name="token" value="…">`, see the example's
/// `server.js`; the token is hex, so no HTML escaping can distort it).
fn hidden_token(html: &str) -> Result<String, String> {
    html.split("name=\"token\" value=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "the Notes home page carries no hidden token input".to_owned())
}

/// The inspected loopback URL of a view that must be running and healthy.
fn healthy_loopback(view: &crate::PocketView) -> Result<String, String> {
    match (&view.status.url, view.status.state, view.ready) {
        (Some(url), PocketState::Running, true) if url.starts_with("http://127.0.0.1:") => {
            Ok(url.clone())
        }
        _ => Err(format!(
            "start did not report a healthy inspected loopback Pocket: {:?}",
            view.status.message
        )),
    }
}

/// Whether one `podman ps` JSON row names the container exactly `name`.
/// Podman marshals `Names` as an array and some engines prefix a slash;
/// both are handled exactly as the production lookup does.
fn entry_holds_name(entry: &Value, name: &str) -> bool {
    let matches = |candidate: &str| candidate == name || candidate.strip_prefix('/') == Some(name);
    match entry.get("Names") {
        Some(Value::String(one)) => matches(one),
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).any(matches),
        _ => false,
    }
}

/// Looks the container up by exact name through `podman ps --all`, which
/// exits zero even when nothing matches: an `Err` here is therefore a real
/// engine failure, never a swallowed absence. Ownership is then verified
/// against that inspected ID. `Ok(None)` means the container is confirmed
/// absent; anything under the name that is not this test's owned container
/// is an `Err` and never touched. Later commands act only on the inspected
/// `id`, never the name.
async fn owned_container(
    bin: &Path,
    name: &str,
    label: &str,
) -> Result<Option<(String, bool)>, String> {
    let filter = format!("name=^{name}$");
    let output = runtime::run(
        bin,
        &["ps", "--all", "--filter", &filter, "--format", "json"],
    )
    .await?;
    let entries: Vec<Value> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse `podman ps` output: {err}"))?;
    let Some(found) = entries.iter().find(|entry| entry_holds_name(entry, name)) else {
        return Ok(None); // confirmed absent: nothing to clean up
    };
    let id = found.get("Id").and_then(Value::as_str).unwrap_or_default();
    let id_safe = !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric());
    if !id_safe {
        return Err(format!(
            "`podman ps` reported an unusable container id for {name}"
        ));
    }

    // Verify identity against the inspected id — the exact identity later
    // commands act on. Podman normally reports the bare name; a leading
    // slash is stripped when present, and a bare name is kept as-is.
    let output = runtime::run(bin, &["inspect", id]).await?;
    let entries: Vec<Value> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse `podman inspect` output: {err}"))?;
    let entry = entries
        .first()
        .ok_or_else(|| format!("`podman inspect` returned no result for {name}"))?;
    let reported_raw = entry
        .get("Name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let reported = reported_raw.strip_prefix('/').unwrap_or(reported_raw);
    let owned = entry
        .get("Config")
        .and_then(|config| config.get("Labels"))
        .and_then(|labels| labels.get(OWNERSHIP_LABEL))
        .and_then(Value::as_str)
        == Some(label);
    if reported != name || !owned {
        return Err(format!(
            "container {id} claims the name {name} but is not this test's \
             owned container; leaving it untouched"
        ));
    }
    let running = entry
        .get("State")
        .and_then(|state| state.get("Running"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Some((id.to_owned(), running)))
}

/// Removes exactly the three engine objects this test's project UUID could
/// have produced, and reports every problem instead of panicking. Container
/// first (stopped by inspected ID if still running, then `rm`), then the
/// labelled volume, then the unique build tag. Foreign objects under the
/// same names are reported and left untouched.
async fn cleanup_engine(bin: &Path, project_id: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let name = format!("pockets-project-{project_id}");
    let label = format!("project-{project_id}");

    match owned_container(bin, &name, &label).await {
        Err(err) => problems.push(format!("container cleanup for {name}: {err}")),
        Ok(None) => {}
        Ok(Some((id, running))) => {
            if running {
                if let Err(err) = runtime::run(bin, &["stop", "--time", "5", &id]).await {
                    problems.push(format!("stop container {id}: {err}"));
                }
            }
            if let Err(err) = runtime::run(bin, &["rm", &id]).await {
                problems.push(format!("remove container {id}: {err}"));
            }
        }
    }

    if problems.is_empty() {
        let scope = crate::runtime_env::Scope::new(
            project_id,
            crate::runtime_settings::Environment::Installed,
        )
        .expect("fixture project ID");
        if let Err(error) = crate::runtime_env::clear(bin, &scope).await {
            problems.push(error);
        }
    }

    let volume = format!("pockets-project-{project_id}-data");
    match runtime::run(bin, &["volume", "inspect", &volume]).await {
        // Absent volumes exit non-zero: nothing was ever created for them.
        Err(_) => {}
        Ok(output) => {
            let owned = serde_json::from_slice::<Vec<Value>>(&output.stdout)
                .ok()
                .and_then(|entries| entries.into_iter().next())
                .is_some_and(|entry| {
                    entry
                        .get("Labels")
                        .and_then(|labels| labels.get(OWNERSHIP_LABEL))
                        .and_then(Value::as_str)
                        == Some(label.as_str())
                });
            if !owned {
                problems.push(format!(
                    "volume {volume} is not labelled {OWNERSHIP_LABEL}={label}; \
                     leaving it untouched"
                ));
            } else if let Err(err) = runtime::run(bin, &["volume", "rm", &volume]).await {
                problems.push(format!("remove volume {volume}: {err}"));
            }
        }
    }

    // Only this test's unique tag, by reference — never an image ID,
    // never a prune, never anything else.
    let tag = format!("localhost/pockets-managed-{project_id}:build");
    match runtime::run(bin, &["image", "inspect", &tag]).await {
        Err(_) => {} // never built: nothing to remove
        Ok(_) => {
            if let Err(err) = runtime::run(bin, &["rmi", &tag]).await {
                problems.push(format!("remove image tag {tag}: {err}"));
            }
        }
    }

    problems
}

/// Build -> start -> save -> stop -> reopen store -> start again, asserting
/// the invariants at each step. Everything is keyed by `project_id` in a
/// fresh `Store` over `root`; no engine object is removed here — the caller
/// owns scoped cleanup on every path.
async fn drive_lifecycle(bin: &Path, root: &Path, project_id: &str) -> Result<(), String> {
    // The build runs without dialogs or locks and must commit the
    // verified image id, installing the record as `Installed` while
    // leaving it unpreviewed.
    let store = Store::new(root.to_path_buf());
    let progress = BuildProgressHandle::new();
    let built = creation::build_approved_project(&store, bin, project_id, &progress).await?;
    if progress.snapshot().state != BuildState::Succeeded {
        return Err("a successful build must report a Succeeded progress job".to_owned());
    }
    if built.id != project_id || built.phase != Phase::Installed || built.previewed {
        return Err(format!(
            "a successful build must install the record and stay unpreviewed, got phase {:?} \
             previewed {}",
            built.phase, built.previewed
        ));
    }
    if built.built_image_id.is_none() {
        return Err("the built record carries no image id".to_owned());
    }

    // Start through the production lifecycle service: healthy, inspected,
    // loopback-only.
    let view = project_lifecycle::start(&store, bin, project_id, &Default::default()).await?;
    healthy_loopback(&view)?;

    let address =
        local_sites::Site::Managed(&built).running_address(&store, view.status.url.as_deref())?;
    let url = address.url();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .resolve(
            &address.host,
            (std::net::Ipv4Addr::LOCALHOST, address.port).into(),
        )
        .build()
        .map_err(|err| err.to_string())?;
    let marker = format!("pockets-live-{}", uuid::Uuid::new_v4().simple());

    // A fresh volume starts blank; the save needs the page's own CSRF token
    // and answers 303, which the no-redirect client observes directly. The
    // body is the form urlencoding of `note` and `token`; both values are
    // strictly `[a-z0-9-]` and hex, so no escaping is needed (and the
    // reqwest `form` helper is not compiled into this dependency set).
    let home = get(&client, &format!("{url}/")).await?;
    if home.contains(&marker) {
        return Err("a fresh data volume must start with an empty note".to_owned());
    }
    let token = hidden_token(&home)?;
    let saved = client
        .post(format!("{url}/note"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("note={marker}&token={token}"))
        .send()
        .await
        .map_err(|err| format!("POST {url}/note failed: {err}"))?;
    if saved.status().as_u16() != 303 {
        return Err(format!(
            "POST /note must answer 303, got {}",
            saved.status()
        ));
    }
    let home = get(&client, &format!("{url}/")).await?;
    if !home.contains(&marker) {
        return Err("the saved note is missing from GET / right after the save".to_owned());
    }

    // Restart with Podman alone and visit the original named URL.
    runtime::run(bin, &["restart", &format!("pockets-project-{project_id}")]).await?;
    crate::health::wait_ready(
        &healthy_loopback(&view)?,
        &built.installed_manifest().web.healthcheck,
    )
    .await?;
    if !get(&client, &url).await?.contains(&marker) {
        return Err("Podman restart lost the named address or the saved note.".into());
    }

    // Stop through the service: reconciled stopped, never ready.
    let stopped = project_lifecycle::stop(&store, bin, project_id).await?;
    if stopped.ready || stopped.status.state != PocketState::Stopped {
        return Err(format!(
            "stop must report a stopped un-ready Pocket, got {:?} ready {}",
            stopped.status.state, stopped.ready
        ));
    }
    if client.get(format!("{url}/")).send().await.is_ok() {
        return Err("The stopped Pocket still answers its published port.".to_owned());
    }

    // Another Mac process can take a stopped Pocket's port. Refuse the
    // collision, then recover the same origin after the listener goes away.
    let occupant = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, address.port))
        .map_err(|err| err.to_string())?;
    let refused = project_lifecycle::start(&store, bin, project_id, &Default::default()).await;
    if !matches!(refused, Err(err) if err.contains("is in use")) {
        return Err("Start should report the occupied port.".into());
    }
    drop(occupant);

    // A brand-new Store over the same root — the container definition is
    // recreated from the fixed target arguments, and the note must survive
    // on the named data volume.
    let reopened = Store::new(root.to_path_buf());
    let view = project_lifecycle::start(&reopened, bin, project_id, &Default::default()).await?;
    healthy_loopback(&view)?;
    let home = get(&client, &format!("{url}/")).await?;
    if !home.contains(&marker) {
        return Err("the saved note did not survive the container recreation".to_owned());
    }
    Ok(())
}

/// Everything the test does, as a `Result` so scoped engine cleanup is
/// attempted on every path — a failed assertion is an `Err` that still
/// reaches cleanup, unlike a panic.
async fn managed_live_round_trip() -> Result<(), String> {
    let Some(bin) = runtime::find_podman() else {
        return Err("podman was not found on this computer".to_owned());
    };
    // A prefix containing a space places the fixture under an app-data-like
    // path with spaces, so the real `podman build` runs against a spaced
    // context path; TempDir owns these files' cleanup as usual.
    let workspace = tempfile::Builder::new()
        .prefix("pockets managed live ")
        .tempdir()
        .map_err(|err| format!("a temporary store root could not be created: {err}"))?;
    let root = workspace.path().join("projects");
    let store = Store::new(root.clone());

    // Import the repository's own Personal Notes example. Nothing under the
    // example folder is read beyond the capture rules, and from here on
    // every step works against the managed copy only.
    let project = source::import_project(&store, &example_dir())?;
    if let Some(leaked) = project
        .files
        .iter()
        .find(|file| *file == ".env" || file.starts_with("data/"))
    {
        return Err(format!(
            "the imported snapshot leaked {leaked:?} from the example folder"
        ));
    }
    for required in ["Containerfile", "pocket.yaml", "package.json", "server.js"] {
        if !project.files.iter().any(|file| file == required) {
            return Err(format!("the imported snapshot is missing {required}"));
        }
    }
    let managed = store.source_path(&project.id)?;
    if managed.join("data").exists() || managed.join(".env").exists() {
        return Err("the managed source carries data/ or .env".to_owned());
    }

    let outcome = drive_lifecycle(&bin, &root, &project.id).await;
    let problems = cleanup_engine(&bin, &project.id).await;
    if problems.is_empty() {
        return outcome;
    }
    let joined = problems.join("; ");
    Err(match outcome {
        Ok(()) => format!("the round trip succeeded but scoped cleanup reported: {joined}"),
        Err(err) => format!("{err} (scoped cleanup also reported: {joined})"),
    })
}

#[tokio::test]
#[ignore = "requires operational Podman; builds and runs isolated managed Notes fixture"]
async fn managed_live_import_build_start_save_restart_preserves_note() {
    if let Err(err) = managed_live_round_trip().await {
        panic!("live managed-Pocket round trip failed: {err}");
    }
}
