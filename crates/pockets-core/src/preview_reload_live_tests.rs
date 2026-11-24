//! Ignored real-Podman regression test for the controller-driven hot-reload
//! contract (`docs/live-preview.md`, `spikes/runtime/RESULTS.md`), kept in its
//! own file so the mocked unit seams in `preview.rs` stay untouched.
//!
//! Attached at the `preview.rs` root by the lead as
//! `#[cfg(test)] #[path = "preview_reload_live_tests.rs"] mod reload_live_tests;`
//! once the no-restart style path lands: the module body reaches the private
//! preview surface (`start`, `refresh`, `stop`, `Previews`, `lookup_owned`,
//! `is_loopback_proxy_url`) through `use super::*` and drives the *real*
//! engine — no fake-binary substitution anywhere.
//!
//! What it pins, against a disposable session this test fully owns:
//!
//! * `start` on a fresh UUID session reaches `ready` with a loopback proxy
//!   URL, the proxy answers the manifest healthcheck, and the trusted Node
//!   fixture serves the *red* stylesheet through the read-only bind mount.
//! * A style-only `refresh` (atomic `Workspace::write` of `style.css`, exactly
//!   the harness's rename-based edit shape) returns `ready` on the *same*
//!   proxy URL, serves the *green* bytes, and leaves `.State.StartedAt`
//!   **unchanged** — the hot path must not restart a running container.
//! * A server-code `refresh` (the same write shape against `server.js`)
//!   returns `ready` on the same proxy URL, serves the *latest* HTML, and
//!   `.State.StartedAt` **changed** — the automatic restart path, with no
//!   manual `start` between.
//!
//! Isolation rules the test obeys: its own `find_podman` + freshly generated
//! UUID session + private temp AI root (never the existing `real_preview_*`
//! env-var test's variables, which would stop a user's live session); a small
//! trusted stdlib-only Node fixture replaces the seeded `server.js` (no app
//! user files, no provider, no model requests, no app-data records); cleanup
//! runs the production `stop` unconditionally — even when a check failed —
//! before the final assertion, reclaiming this session's container, volume,
//! and proxy, while the shared network teardown inside `stop` stays
//! best-effort and engine-refused while other previews are attached. Every
//! wait inside the flow is an existing bounded helper (health probe window,
//! refresh's visibility gate); this file adds no retry loops of its own.
//!
//! ```text
//! cargo test --lib real_preview_hot_reload_keeps_running -- --ignored --nocapture
//! ```

use super::*;
use uuid::Uuid;

/// Workspace-relative paths the two refresh cycles report as changed.
const STYLE_REL: &str = "style.css";
const SERVER_REL: &str = "server.js";

/// The stylesheet in each revision. The words `red` and `green` are the
/// served-bytes markers; neither contains the other as a substring.
const CSS_RED: &str = "button {\n  color: red;\n}\n";
const CSS_GREEN: &str = "button {\n  color: green;\n}\n";

/// The HTML revision markers: v1 ships `data-rev="one"`, the server-code edit
/// flips it to `data-rev="two"` and nothing else.
const MARKER_ONE: &str = "data-rev=\"one\"";
const MARKER_TWO: &str = "data-rev=\"two\"";

/// The live client tag [`preview_proxy`] injects into served HTML; asserting
/// it after the server restart proves the proxy still fronts the new upstream.
const LIVE_CLIENT_TAG: &str = r#"<script src="/__pockets-live/client.js" defer></script>"#;

/// The small TRUSTED fixture that replaces the seeded `server.js` for this
/// test's session only: Node stdlib HTTP, nothing else. `GET /health` answers
/// 200 for the manifest healthcheck; `GET /style.css` reads `style.css`
/// afresh on every request, so the running server always serves the current
/// bind-mounted bytes; `GET /` serves the HTML linking the stylesheet and
/// carrying the revision-marked button. No user files, no provider, no
/// network beyond the loopback listener on the injected `HOST`/`PORT`.
const SERVER_JS: &str = r#"import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const host = process.env.HOST || '0.0.0.0';
const port = Number(process.env.PORT || 8080);
const appRoot = path.dirname(fileURLToPath(import.meta.url));

const server = http.createServer((req, res) => {
  const route = (req.url || '/').split('?')[0];
  if (route === '/health') {
    res.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' });
    res.end('ok');
    return;
  }
  if (route === '/style.css') {
    // Read fresh per request: host edits land through the read-only bind
    // mount, and a hot reload must serve them with no restart.
    let css;
    try {
      css = fs.readFileSync(path.join(appRoot, 'style.css'), 'utf8');
    } catch (err) {
      res.writeHead(500, { 'content-type': 'text/plain; charset=utf-8' });
      res.end('style.css is unreadable');
      return;
    }
    res.writeHead(200, { 'content-type': 'text/css; charset=utf-8' });
    res.end(css);
    return;
  }
  if (route === '/' && req.method === 'GET') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    res.end('<!doctype html><html><head><title>Pockets hot-reload fixture</title>'
      + '<link rel="stylesheet" href="/style.css"></head>'
      + '<body><button id="action" data-rev="one">Press me</button></body></html>\n');
    return;
  }
  res.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' });
  res.end('not found');
});

server.listen(port, host);
"#;

/// Fetch one path through the preview's user-facing proxy URL and return the
/// status code with the body text. Plain reqwest — same as the existing
/// round-trip test; a transport failure is an actionable `Err`, never a retry.
async fn proxy_get(user_url: &str, path: &str) -> Result<(u16, String), String> {
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|err| format!("could not build the fixture HTTP client: {err}"))?
        .get(format!("{}{path}", user_url.trim_end_matches('/')))
        .send()
        .await
        .map_err(|err| format!("GET {path} through the proxy at {user_url} failed: {err}"))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|err| format!("GET {path} returned an unreadable body: {err}"))?;
    Ok((status, body))
}

/// The owned container's `.State.StartedAt`, read through the existing
/// `lookup_owned` proof-of-ownership and `runtime::run`, straight off raw
/// `podman inspect` JSON. Podman reports nanosecond precision, so the value
/// is a faithful restart witness: `podman restart` always moves it.
async fn started_at(bin: &Path, session_id: &str) -> Result<String, String> {
    let owned = lookup_owned(bin, session_id, false)
        .await?
        .ok_or_else(|| "the owned preview container disappeared mid-scenario".to_owned())?;
    let output = runtime::run(bin, &["inspect", &owned.id]).await?;
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("could not parse `podman inspect` output: {err}"))?;
    entries
        .first()
        .and_then(|entry| entry.get("State"))
        .and_then(|state| state.get("StartedAt"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "`podman inspect` reported no .State.StartedAt".to_owned())
}

/// The whole hot-reload scenario against the real engine. Every check returns
/// its failure as an `Err` string instead of panicking, so the caller always
/// reaches the `stop` cleanup; the final assertion happens after teardown.
async fn hot_reload_round_trip(
    ai_root: &Path,
    bin: &Path,
    session: &str,
    previews: &Previews,
) -> Result<(), String> {
    // A fresh real workspace seeded by the production path, then the trusted
    // fixture replaces the generated server and the stylesheet starts red.
    let workspace = Workspace::create(ai_root, session)?;
    workspace.write(SERVER_REL, SERVER_JS)?;
    workspace.write(STYLE_REL, CSS_RED)?;

    // -- Start on the real engine: ready, loopback proxy, healthy, red CSS --
    let status = start(ai_root, bin, session, previews).await?;
    if status.state != PreviewState::Ready {
        return Err(format!("start must reach ready, got {status:?}"));
    }
    let user_url = status
        .url
        .clone()
        .ok_or_else(|| format!("ready must carry the proxy url, got {status:?}"))?;
    if !is_loopback_proxy_url(&user_url) {
        return Err(format!(
            "the user-facing url must be the loopback proxy, got {user_url}"
        ));
    }
    let (health_status, _) = proxy_get(&user_url, "/health").await?;
    if health_status != 200 {
        return Err(format!(
            "the proxy must relay the manifest healthcheck with 200, got {health_status}"
        ));
    }
    let (css_status, css) = proxy_get(&user_url, "/style.css").await?;
    if css_status != 200 || !css.contains("red") || css.contains("green") {
        return Err(format!(
            "the stylesheet must serve the red revision (200, got {css_status}): {css:?}"
        ));
    }
    let started_before = started_at(bin, session).await?;

    // -- Style-only edit: hot reload through the no-restart path ------------
    // `Workspace::write` is the production atomic temp-plus-rename edit, so
    // the refresh's bounded visibility gate is what waits for propagation.
    workspace.write(STYLE_REL, CSS_GREEN)?;
    let refreshed = refresh(ai_root, bin, session, &[STYLE_REL.to_owned()], previews).await?;
    if refreshed.state != PreviewState::Ready {
        return Err(format!(
            "the style refresh must land ready, got {refreshed:?}"
        ));
    }
    if refreshed.url.as_deref() != Some(user_url.as_str()) {
        return Err(format!(
            "the style refresh must keep the user-facing proxy url {user_url}, got {:?}",
            refreshed.url
        ));
    }
    let (css_status, css) = proxy_get(&user_url, "/style.css").await?;
    if css_status != 200 || !css.contains("green") || css.contains("red") {
        return Err(format!(
            "the hot-reloaded stylesheet must serve the green revision with no restart \
             (200, got {css_status}): {css:?}"
        ));
    }
    let started_after_style = started_at(bin, session).await?;
    if started_after_style != started_before {
        return Err(format!(
            "the style-only refresh restarted the container: StartedAt moved from \
             {started_before:?} to {started_after_style:?}; the CSS path must hot-reload \
             without a restart"
        ));
    }

    // -- Server-code edit: automatic restart, same proxy URL, latest HTML ---
    // Derived from the same fixture so exactly the HTML marker changed — the
    // harmless server-code edit a morph-class refresh must pick up.
    let updated_server = SERVER_JS.replace(MARKER_ONE, MARKER_TWO);
    if updated_server == SERVER_JS {
        return Err("the server fixture does not carry the revision marker".to_owned());
    }
    workspace.write(SERVER_REL, &updated_server)?;
    let refreshed = refresh(ai_root, bin, session, &[SERVER_REL.to_owned()], previews).await?;
    if refreshed.state != PreviewState::Ready {
        return Err(format!(
            "the server refresh must land ready, got {refreshed:?}"
        ));
    }
    if refreshed.url.as_deref() != Some(user_url.as_str()) {
        return Err(format!(
            "the server refresh must keep the user-facing proxy url {user_url}, got {:?}",
            refreshed.url
        ));
    }
    let (html_status, html) = proxy_get(&user_url, "/").await?;
    if html_status != 200 || !html.contains(MARKER_TWO) || html.contains(MARKER_ONE) {
        return Err(format!(
            "the restarted server must serve the latest HTML on the same proxy url \
             (200, got {html_status}): {html:?}"
        ));
    }
    if !html.contains(LIVE_CLIENT_TAG) {
        return Err(format!(
            "the proxy must still inject the live client after the restart: {html:?}"
        ));
    }
    let started_after_server = started_at(bin, session).await?;
    if started_after_server == started_before {
        return Err(format!(
            "the server-code refresh did not restart the container: StartedAt is still \
             {started_after_server:?}; the morph path must pick up server changes through \
             an automatic restart"
        ));
    }
    Ok(())
}

/// The ignored real-engine test. Creates its own UUID session and private
/// temp AI root (never the existing `real_preview_*` env-driven test's
/// variables), runs every check inside the `Result` flow above, then ALWAYS
/// awaits the production `stop` and removes only its own temp root after
/// successful cleanup. A cleanup failure keeps that root for recovery rather
/// than deleting files that might still be mounted. No app-data records,
/// provider calls, or unrelated engine objects are touched.
#[tokio::test]
#[ignore = "requires real Podman and pinned preview image"]
async fn real_preview_hot_reload_keeps_running() {
    let bin = runtime::find_podman().expect("podman at a known location or on PATH");
    let session = Uuid::new_v4().simple().to_string();
    let parent = std::env::temp_dir().join("opencode");
    std::fs::create_dir_all(&parent).expect("create the test scratch parent");
    let ai_root = parent.join(format!("pockets-preview-hot-reload-{session}"));
    let mut directory = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory
        .create(&ai_root)
        .expect("create the private temp ai root");
    let previews = Previews::default();

    let checks = hot_reload_round_trip(&ai_root, &bin, &session, &previews).await;
    if checks.is_err() {
        if let Ok(Some(owned)) = lookup_owned(&bin, &session, false).await {
            if let Ok(logs) = runtime::run(&bin, &["logs", "--tail", "30", &owned.id]).await {
                eprintln!(
                    "Disposable fixture logs:\n{}\n{}",
                    String::from_utf8_lossy(&logs.stdout),
                    String::from_utf8_lossy(&logs.stderr),
                );
            }
        }
    }

    // Cleanup first, assertion last: the owned container, data volume, and
    // proxy listener for this session alone are reclaimed through `stop`
    // even when a check failed. The shared network is only removed when this
    // registry has no active preview and the engine allows it — an "in use"
    // refusal is expected and tolerated inside `stop`, never fatal here.
    let cleanup = stop(&bin, &session, &previews).await;
    // This directory was created above under a fresh UUID name and holds only
    // this test's workspace, so removing it removes only owned bytes.
    if cleanup.is_ok() {
        let _ = std::fs::remove_dir_all(&ai_root);
    }

    match (checks, cleanup) {
        (Ok(()), Ok(true)) => {}
        (Ok(()), Ok(false)) => panic!(
            "the hot-reload scenario passed but `stop` reported no registry entry to reclaim"
        ),
        (Ok(()), Err(err)) => panic!("the hot-reload scenario passed but cleanup failed: {err}"),
        (Err(err), Ok(_)) => panic!("the hot-reload scenario failed: {err}"),
        (Err(err), Err(cleanup_err)) => {
            panic!("the hot-reload scenario failed: {err} (cleanup also failed: {cleanup_err})")
        }
    }
}
