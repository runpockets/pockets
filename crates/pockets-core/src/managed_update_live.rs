//! Opt-in live coverage of the explicit-update and source-history-restore
//! path, reusing this parent module's helpers, isolation, and scoped
//! [`cleanup_engine`]. One freshly imported Personal Notes fixture (own
//! UUID and a temporary store and development folder,
//! repository example strictly read-only) proves: `build_candidate_image`
//! never commits; `apply_candidate` installs without launching, then keeps
//! the UUID volume and host port across running updates and rolls back when
//! health fails; and source export, `development::sync` and
//! `restore_working` move only the working source. The same id, the same
//! inspected loopback URL, and the saved note survive every step; a final
//! unreachable health check is refused after one bounded 30-second health
//! wait with installed metadata untouched.

use super::*;

use crate::{development, project_updates, revisions};

const HEADING_ORIGINAL: &str = "<h1>Personal Notes</h1>";
const HEADING_UPDATED: &str = "<h1>Updated Notes</h1>";
const HEALTH_OLD: &str = "healthcheck: /health";
const HEALTH_BROKEN: &str = "healthcheck: /never-healthy";

/// GET the home page once and require every `wants` needle and the absence
/// of every `absent` needle.
async fn page(
    client: &reqwest::Client,
    url: &str,
    wants: &[&str],
    absent: &[&str],
) -> Result<(), String> {
    let home = get(client, &format!("{url}/")).await?;
    for needle in wants {
        if !home.contains(*needle) {
            return Err(format!("GET {url}/ is missing {needle:?}"));
        }
    }
    for needle in absent {
        if home.contains(*needle) {
            return Err(format!("GET {url}/ still carries {needle:?}"));
        }
    }
    Ok(())
}

/// Save `marker` exactly the way the Notes UI does: the page's own hidden
/// CSRF token, a 303-answering `POST /note`, then the marker back on GET /.
async fn save_note(client: &reqwest::Client, url: &str, marker: &str) -> Result<(), String> {
    let home = get(client, &format!("{url}/")).await?;
    if home.contains(marker) {
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
    let code = saved.status();
    if code.as_u16() != 303 {
        return Err(format!("POST /note must answer 303, got {code}"));
    }
    page(client, url, &[marker], &[]).await
}

/// The id of the archived revision carrying exactly `digest`.
fn revision_with_digest(store: &Store, id: &str, digest: &str) -> Result<String, String> {
    revisions::list(store, id)?
        .into_iter()
        .find(|revision| revision.source_digest == digest)
        .map(|revision| revision.id)
        .ok_or_else(|| format!("no archived revision carries digest {digest}"))
}

/// Install -> start -> save -> publish -> update -> restore -> update ->
/// refused unhealthy update, as one exclusive flow; the caller owns the
/// scoped engine cleanup on every path.
fn edit_file(path: &Path, from: &str, to: &str) -> Result<(), String> {
    let source = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    if !source.contains(from) {
        return Err(format!("Missing fixture text in {}.", path.display()));
    }
    std::fs::write(path, source.replacen(from, to, 1)).map_err(|error| error.to_string())
}

async fn check_installed_variables(
    bin: &Path,
    container: &str,
    expected: &str,
) -> Result<(), String> {
    let source = format!("process.exit(process.env.INSTALLED_FIXTURE_KEY === {} && !('PREVIEW_FIXTURE_KEY' in process.env) ? 0 : 1)", serde_json::to_string(expected).unwrap());
    if !runtime::run_private(bin, &["exec", container, "node", "-e", &source], None).await {
        return Err("Installed runtime variables did not match the synthetic fixture.".into());
    }
    if String::from_utf8_lossy(&runtime::run(bin, &["inspect", container]).await?.stdout)
        .contains(expected)
    {
        return Err("Podman inspect exposed an installed runtime variable value.".into());
    }
    Ok(())
}

async fn drive_updates(bin: &Path, store: &Store, parent: &Path, pid: &str) -> Result<(), String> {
    let client = live_client()?;

    // 1. A candidate build must not commit anything, and the first apply
    //    installs metadata without launching a container.
    let progress = BuildProgressHandle::new();
    let candidate = creation::build_candidate_image(store, bin, pid, &progress).await?;
    if progress.snapshot().state != BuildState::Succeeded {
        return Err("the candidate build must report Succeeded progress".to_owned());
    }
    if store.load(pid)?.installed.is_some() {
        return Err("building a candidate must not commit an installation".to_owned());
    }
    let first_image = candidate.image_id.clone();
    let installed = project_updates::apply_candidate(store, bin, candidate).await?;
    let pins_first = installed
        .installed
        .as_ref()
        .is_some_and(|pin| pin.image_id == first_image);
    if installed.id != pid || installed.phase != Phase::Installed || !pins_first {
        return Err("the first apply must install the same project on the new image".to_owned());
    }
    if installed.previewed {
        return Err("the first apply must leave the app unpreviewed".to_owned());
    }
    let name = format!("pockets-project-{pid}");
    let label = format!("project-{pid}");
    if owned_container(bin, &name, &label).await?.is_some() {
        return Err("the first installation must not launch a container".to_owned());
    }

    // Start for real and save a unique note, so later replacements prove
    // the data volume was carried through untouched.
    let initial_value = "pockets-installed-fixture-value";
    let variables = [("INSTALLED_FIXTURE_KEY".into(), initial_value.into())].into();
    let url = healthy_loopback(&project_lifecycle::start(store, bin, pid, &variables).await?)?;
    check_installed_variables(bin, &name, initial_value).await?;
    let marker = format!("pockets-live-{}", uuid::Uuid::new_v4().simple());
    save_note(&client, &url, &marker).await?;
    let updated_home = [HEADING_UPDATED, marker.as_str()];
    let original_home = [HEADING_ORIGINAL, marker.as_str()];

    // 2. The install archived the working source: that revision is the
    //    restore baseline. A workspace edit then publishes a new working
    //    revision while the installed app stays pinned to the old image.
    let baseline_digest = store.load(pid)?.source_digest;
    let baseline = revision_with_digest(store, pid, &baseline_digest)?;
    let history = revisions::list(store, pid)?;
    if development::export(store, pid, parent)? != installed
        || revisions::list(store, pid)? != history
    {
        return Err("export must preserve the saved project and its history".into());
    }
    let folder = development::folder(store, pid)?;
    edit_file(&folder.join("server.js"), HEADING_ORIGINAL, HEADING_UPDATED)?;
    let published = development::sync(store, pid)?;
    if published.id != pid || published.source_digest == baseline_digest {
        return Err("publishing must advance the working source of the same project".to_owned());
    }
    let Some(pinned) = published.installed.as_ref() else {
        return Err("the published source must stay pending behind the pinned install".to_owned());
    };
    if pinned.source_digest != baseline_digest {
        return Err("publishing must not advance the installed revision".to_owned());
    }
    revision_with_digest(store, pid, &published.source_digest)?;
    if store.list()?.len() != 1 {
        return Err("publishing must not create a second project".to_owned());
    }

    // 3. The explicit update replaces the running container in place: same
    //    id, same inspected loopback URL, same note, new installed image.
    let progress = BuildProgressHandle::new();
    let candidate = creation::build_candidate_image(store, bin, pid, &progress).await?;
    let updated_image = candidate.image_id.clone();
    let updated = project_updates::apply_candidate(store, bin, candidate).await?;
    let Some(pin) = updated.installed.as_ref() else {
        return Err("the update must install".to_owned());
    };
    if updated.id != pid || pin.image_id != updated_image || pin.revision_id.is_none() {
        return Err("the update must install the new image over a revision".to_owned());
    }
    if pin.source_digest != published.source_digest {
        return Err("the update must install over the published source".to_owned());
    }
    if healthy_loopback(&project_lifecycle::status(store, bin, pid).await?)? != url {
        return Err("a running update must keep the inspected loopback URL".to_owned());
    }
    page(&client, &url, &updated_home, &[HEADING_ORIGINAL]).await?;
    check_installed_variables(bin, &name, initial_value).await?;

    // 4. Restoring the baseline revision rewrites only the working source;
    //    the next explicit update reinstalls the original page while the
    //    note survives — restoring history never rolls back user data.
    development::unlink(store, pid)?;
    let restored = revisions::restore_working(store, pid, &baseline)?;
    if restored.id != pid || restored.source_digest != baseline_digest {
        return Err("the restore must return the working source to the baseline".to_owned());
    }
    let progress = BuildProgressHandle::new();
    let candidate = creation::build_candidate_image(store, bin, pid, &progress).await?;
    let reverted = project_updates::apply_candidate(store, bin, candidate).await?;
    let back = reverted
        .installed
        .as_ref()
        .map(|pin| pin.source_digest.as_str());
    if reverted.id != pid || back != Some(baseline_digest.as_str()) {
        return Err("the restored source must install over the same project".to_owned());
    }
    if healthy_loopback(&project_lifecycle::status(store, bin, pid).await?)? != url {
        return Err("the restored update must keep the inspected loopback URL".to_owned());
    }
    page(&client, &url, &original_home, &[HEADING_UPDATED]).await?;
    if store.list()?.len() != 1 {
        return Err("the update and restore cycle must not create a second project".to_owned());
    }

    // 5. A manifest whose health check can never answer 2xx must be refused
    //    after the bounded health wait, byte-identical installed metadata
    //    and the pinned container still ready on the same URL via its old
    //    health check. The image itself may legitimately be cached.
    edit_file(&folder.join("pocket.yaml"), HEALTH_OLD, HEALTH_BROKEN)?;
    development::link(store, Some(pid), &folder)?;
    development::sync(store, pid)?;
    let progress = BuildProgressHandle::new();
    let candidate = creation::build_candidate_image(store, bin, pid, &progress).await?;
    let before = store.load(pid)?;
    match project_updates::apply_candidate(store, bin, candidate).await {
        Ok(_) => return Err("an update that never becomes healthy must be refused".to_owned()),
        Err(err) if err.contains("did not become healthy") => {}
        Err(err) => return Err(format!("expected a bounded health failure, got: {err}")),
    }
    let after = store.load(pid)?;
    if after.installed != before.installed {
        return Err("a rolled-back update must keep the previous installed metadata".to_owned());
    }
    if healthy_loopback(&project_lifecycle::status(store, bin, pid).await?)? != url {
        return Err("the pinned container must still serve the original URL".to_owned());
    }
    page(&client, &url, &original_home, &[HEADING_UPDATED]).await?;
    check_installed_variables(bin, &name, initial_value).await?;
    project_lifecycle::stop(store, bin, pid).await?;
    let rotated = "pockets-installed-fixture-rotated";
    let variables = [("INSTALLED_FIXTURE_KEY".into(), rotated.into())].into();
    let restarted =
        healthy_loopback(&project_lifecycle::start(store, bin, pid, &variables).await?)?;
    check_installed_variables(bin, &name, rotated).await?;
    page(&client, &restarted, &original_home, &[HEADING_UPDATED]).await?;

    // The remote engine can lose a secret while the running app still has
    // its environment. Refuse to stop that app for an unrecoverable update.
    let output = runtime::run(bin, &["inspect", &name]).await?;
    let inspect: Vec<Value> = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let binding_label = inspect
        .first()
        .and_then(|entry| entry["Config"]["Labels"][crate::runtime_env::BINDINGS_LABEL].as_str())
        .ok_or("The fixture has no runtime bindings label.")?;
    let bindings: Value = serde_json::from_str(binding_label).map_err(|e| e.to_string())?;
    let secret = bindings["INSTALLED_FIXTURE_KEY"]
        .as_str()
        .filter(|name| name.starts_with(&format!("pockets-runtime-installed-{pid}-")))
        .ok_or("The fixture has no scoped secret.")?;
    runtime::run(bin, &["secret", "rm", secret]).await?;
    let installed = after
        .installed
        .as_ref()
        .ok_or("The fixture is not installed.")?;
    let target = crate::pocket::Target::managed(&installed.manifest, pid, &installed.image_id)?;
    let refused = crate::pocket::replace_target(bin, &target, &target).await;
    if !matches!(refused, Err(error) if error.contains("runtime variables are unavailable")) {
        return Err(
            "An update did not refuse missing runtime bindings before stopping the app.".into(),
        );
    }
    page(&client, &restarted, &original_home, &[HEADING_UPDATED]).await?;
    Ok(())
}

/// Everything the test does, as a `Result` so the parent's scoped
/// [`cleanup_engine`] runs on every path — a failed assertion is an `Err`
/// that still reaches cleanup, unlike a panic.
async fn managed_update_and_restore_cycle() -> Result<(), String> {
    let Some(bin) = runtime::find_podman() else {
        return Err("podman was not found on this computer".to_owned());
    };
    // An isolated store and external folder in a Podman-shared directory.
    let temp = tempfile::Builder::new()
        .prefix("pockets managed update live ")
        .tempdir_in("/private/tmp")
        .map_err(|err| format!("a temporary store root could not be created: {err}"))?;
    let store = Store::new(temp.path().join("projects"));

    // Import the repository's own example; the original folder stays a
    // read-only source and every later edit happens in a workspace copy.
    let project = source::import_project(&store, &example_dir())?;
    let pid = project.id.clone();

    let outcome = drive_updates(&bin, &store, temp.path(), &pid).await;
    let problems = cleanup_engine(&bin, &pid).await;
    if problems.is_empty() {
        return outcome;
    }
    let joined = problems.join("; ");
    Err(match outcome {
        Ok(()) => format!("the update cycle succeeded but scoped cleanup reported: {joined}"),
        Err(err) => format!("{err} (scoped cleanup also reported: {joined})"),
    })
}

#[tokio::test]
#[ignore = "requires Podman and pinned Node image; builds isolated fixture"]
async fn managed_update_and_restore_keep_identity_and_data() {
    if let Err(err) = managed_update_and_restore_cycle().await {
        panic!("live managed update-and-restore cycle failed: {err}");
    }
}
