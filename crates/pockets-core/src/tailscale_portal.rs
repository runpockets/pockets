//! Lightweight loopback HTTP portal (`127.0.0.1:18787`) backing Tailscale
//! Serve on HTTPS port 443 (`https://<node>.<tailnet>.ts.net`).
//!
//! Why loopback HTTP instead of `tailscale serve <dir>`?
//! On macOS, the App Store and Standalone GUI variants of `Tailscale.app` run
//! inside Apple's Network Extension sandbox and cannot serve local directories.
//! Proxying a loopback port (`http://127.0.0.1:18787`) works on every Tailscale
//! variant without `sudo` and lets us serve real `302 Found` redirects for
//! `/<pocket-slug>` plus a live HTML launcher at `/`.
//!
//! Port `18787` sits just below the managed Pocket range (`18788..=32767` in
//! `local_sites.rs`) so it never collides with an installed Pocket.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex};

/// Fixed loopback port for the Tailscale launcher & named-slug redirector.
pub(crate) const PORTAL_LOCAL_PORT: u16 = 18787;

/// Hard cap on incoming HTTP request head size (4 KiB).
const MAX_REQUEST_BYTES: usize = 4096;

/// Hard read/write timeout per connection so a slow client cannot hold a task.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

const SECURITY_HEADERS: &str = "Cache-Control: no-store\r\n\
X-Content-Type-Options: nosniff\r\n\
X-Frame-Options: DENY\r\n\
Referrer-Policy: no-referrer\r\n\
Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'\r\n\
Connection: close\r\n";

/// One Pocket entry shown on the Tailscale launcher and reachable via `/<slug>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PortalEntry {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub description: String,
    /// Active Tailscale HTTPS port (`Some(18788)`) when running & served, or
    /// `None` while the Pocket is stopped/paused.
    pub port: Option<u16>,
}

/// Authoritative in-memory snapshot served by the portal listener.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PortalSnapshot {
    pub dns_name: String,
    pub entries: Vec<PortalEntry>,
}

struct RunningPortal {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

/// Managed Tauri state owning the optional `127.0.0.1:18787` portal server.
pub struct TailscalePortal {
    snapshot: Arc<RwLock<PortalSnapshot>>,
    running: Arc<AtomicBool>,
    handle: Mutex<Option<RunningPortal>>,
}

impl Default for TailscalePortal {
    fn default() -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(PortalSnapshot::default())),
            running: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
    }
}

impl TailscalePortal {
    pub(crate) fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub(crate) fn update_snapshot(&self, next: PortalSnapshot) {
        if let Ok(mut guard) = self.snapshot.write() {
            *guard = next;
        }
    }

    /// Updates the snapshot and starts the `127.0.0.1:18787` listener if not
    /// already running.
    pub(crate) async fn ensure_running(&self, next: PortalSnapshot) -> Result<(), String> {
        self.update_snapshot(next);
        let mut guard = self.handle.lock().await;
        if guard
            .as_ref()
            .is_some_and(|handle| !handle.task.is_finished())
        {
            return Ok(());
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, PORTAL_LOCAL_PORT))
            .await
            .map_err(|err| {
                format!("Could not bind Tailscale portal on 127.0.0.1:{PORTAL_LOCAL_PORT}: {err}")
            })?;
        let (tx, mut rx) = watch::channel(false);
        let snapshot = Arc::clone(&self.snapshot);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Release);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() || *rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _)) => {
                                let snapshot = Arc::clone(&snapshot);
                                tokio::spawn(async move {
                                    let _ = handle_connection(stream, snapshot).await;
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
            running.store(false, Ordering::Release);
        });
        *guard = Some(RunningPortal { shutdown: tx, task });
        Ok(())
    }

    /// Stops the portal listener and clears the snapshot.
    pub(crate) async fn stop(&self) {
        self.update_snapshot(PortalSnapshot::default());
        let mut guard = self.handle.lock().await;
        if let Some(handle) = guard.take() {
            let _ = handle.shutdown.send(true);
            handle.task.abort();
            let _ = handle.task.await;
        }
        self.running.store(false, Ordering::Release);
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    snapshot: Arc<RwLock<PortalSnapshot>>,
) -> Result<(), ()> {
    let mut buf = vec![0u8; MAX_REQUEST_BYTES];
    let mut filled = 0usize;
    let read_result = tokio::time::timeout(CONNECTION_TIMEOUT, async {
        while filled < buf.len() {
            let n = stream.read(&mut buf[filled..]).await.map_err(|_| ())?;
            if n == 0 {
                break;
            }
            filled += n;
            if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        Ok::<(), ()>(())
    })
    .await;
    if read_result.is_err() || filled == 0 {
        return Err(());
    }
    let head = String::from_utf8_lossy(&buf[..filled]);
    let request_line = head.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let snap = snapshot.read().map(|g| g.clone()).unwrap_or_default();
    let response = render_response(method, target, &snap);
    let bytes = response.to_http_bytes(method == "HEAD");
    let _ = tokio::time::timeout(CONNECTION_TIMEOUT, stream.write_all(&bytes)).await;
    let _ = stream.shutdown().await;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HttpResponse {
    pub status: &'static str,
    pub location: Option<String>,
    pub body: String,
}

impl HttpResponse {
    fn html(status: &'static str, body: String) -> Self {
        Self {
            status,
            location: None,
            body,
        }
    }

    fn redirect(location: String) -> Self {
        Self {
            status: "302 Found",
            location: Some(location),
            body: String::new(),
        }
    }

    fn to_http_bytes(&self, head_only: bool) -> Vec<u8> {
        let mut out = String::new();
        out.push_str("HTTP/1.1 ");
        out.push_str(self.status);
        out.push_str("\r\n");
        if let Some(loc) = &self.location {
            out.push_str("Location: ");
            out.push_str(loc);
            out.push_str("\r\n");
        }
        if !self.body.is_empty() {
            out.push_str("Content-Type: text/html; charset=utf-8\r\n");
        }
        out.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        out.push_str(SECURITY_HEADERS);
        out.push_str("\r\n");
        if !head_only {
            out.push_str(&self.body);
        }
        out.into_bytes()
    }
}

/// Pure request router for the portal, unit-tested without binding a socket.
pub(crate) fn render_response(
    method: &str,
    target: &str,
    snapshot: &PortalSnapshot,
) -> HttpResponse {
    if method != "GET" && method != "HEAD" {
        return HttpResponse::html(
            "405 Method Not Allowed",
            render_simple_page(
                "Method not allowed",
                "Only GET and HEAD requests are supported.",
                false,
            ),
        );
    }

    // Reject control characters before any URL/header handling.
    if target.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return HttpResponse::html(
            "400 Bad Request",
            render_simple_page("Bad request", "Invalid request path.", false),
        );
    }

    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };
    let trimmed = raw_path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed == "index.html" {
        return HttpResponse::html("200 OK", render_launcher_page(snapshot));
    }

    let (slug, rest) = match trimmed.split_once('/') {
        Some((s, r)) => (s, r),
        None => (trimmed, ""),
    };

    if let Some(entry) = snapshot
        .entries
        .iter()
        .find(|entry| entry.slug.eq_ignore_ascii_case(slug))
    {
        if let Some(port) = entry.port {
            let mut dest = format!("https://{}:{port}/", snapshot.dns_name);
            if !rest.is_empty() {
                dest.push_str(rest);
            }
            if let Some(q) = query {
                dest.push('?');
                dest.push_str(q);
            }
            return HttpResponse::redirect(dest);
        }
        return HttpResponse::html(
            "503 Service Unavailable",
            render_simple_page(
                &format!("{} is stopped", entry.name),
                "Start this Pocket in Pockets to open it from this device.",
                true,
            ),
        );
    }

    HttpResponse::html(
        "404 Not Found",
        render_simple_page(
            "Pocket not found",
            "No shared Pocket matches this address.",
            true,
        ),
    )
}

fn render_launcher_page(snapshot: &PortalSnapshot) -> String {
    let mut active_items = String::new();
    let mut paused_items = String::new();

    for entry in &snapshot.entries {
        let name = escape_html(&entry.name);
        let desc = escape_html(&entry.description);
        let slug = escape_html(&entry.slug);
        if let Some(port) = entry.port {
            let href = escape_html(&format!("https://{}:{port}/", snapshot.dns_name));
            active_items.push_str(&format!(
                r#"<li><a class="card" href="{href}"><div class="row"><span class="name">{name}</span><span class="status ok">Running</span></div><p class="desc">{desc}</p><div class="meta">/{slug} · :{port}</div></a></li>"#
            ));
        } else {
            paused_items.push_str(&format!(
                r#"<li><div class="card paused"><div class="row"><span class="name">{name}</span><span class="status">Paused</span></div><p class="desc">{desc}</p><div class="meta">/{slug} · Stopped</div></div></li>"#
            ));
        }
    }

    let body = if active_items.is_empty() && paused_items.is_empty() {
        r#"<p class="empty">No Pockets are currently shared on Tailscale. Turn on Tailscale sharing on any installed Pocket in Pockets.</p>"#.to_owned()
    } else {
        format!(r#"<ul class="grid">{active_items}{paused_items}</ul>"#)
    };
    let host = escape_html(&snapshot.dns_name);

    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>Pockets</title>
<style>{BASE_CSS}</style>
</head>
<body>
<main>
  <header>
    <h1>Pockets</h1>
    <p class="host">{host}</p>
  </header>
  {body}
</main>
</body>
</html>"#
    )
}

fn render_simple_page(title: &str, message: &str, show_home_link: bool) -> String {
    let title_esc = escape_html(title);
    let msg_esc = escape_html(message);
    let link = if show_home_link {
        r#"<p class="back"><a href="/">View all shared Pockets</a></p>"#
    } else {
        ""
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>{title_esc} — Pockets</title>
<style>{BASE_CSS}</style>
</head>
<body>
<main>
  <h1>{title_esc}</h1>
  <p class="empty">{msg_esc}</p>
  {link}
</main>
</body>
</html>"#
    )
}

fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const BASE_CSS: &str = r#"
:root {
  color-scheme: light dark;
  --bg: #f7f6f3;
  --surface: #ffffff;
  --border: #dedcd6;
  --text: #1a1a18;
  --text-2: #45433f;
  --dim: #64615b;
  --ok: #2a7d47;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #161616;
    --surface: #1e1e1e;
    --border: #2c2c2c;
    --text: #ececec;
    --text-2: #c4c4c4;
    --dim: #9b9b9b;
    --ok: #6bcb8b;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font: 14px/1.6 ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
}
main {
  max-width: 44rem;
  margin: 0 auto;
  padding: 2rem 1.25rem 3rem;
}
header {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  justify-content: space-between;
  gap: 0.5rem 1rem;
  padding-bottom: 1rem;
  border-bottom: 1px solid var(--border);
  margin-bottom: 1.25rem;
}
h1 { margin: 0; font-size: 1.25rem; font-weight: 700; }
.host { margin: 0; font-size: 0.85rem; color: var(--dim); }
.grid {
  list-style: none;
  margin: 0;
  padding: 0;
  display: grid;
  gap: 0.75rem;
}
.card {
  display: block;
  padding: 1rem 1.1rem;
  border: 1px solid var(--border);
  background: var(--surface);
  color: inherit;
  text-decoration: none;
}
a.card:hover, a.card:focus-visible { border-color: var(--text); outline: none; }
.card.paused { opacity: 0.72; }
.row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 0.75rem;
}
.name { font-size: 1rem; font-weight: 700; }
.status { font-size: 0.8rem; color: var(--dim); }
.status.ok { color: var(--ok); }
.desc { margin: 0.35rem 0 0.6rem; color: var(--text-2); }
.meta { font-size: 0.8rem; color: var(--dim); }
.empty { color: var(--dim); }
.back a { color: var(--text); }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_snapshot() -> PortalSnapshot {
        PortalSnapshot {
            dns_name: "macbook-pro.tail00000.ts.net".to_owned(),
            entries: vec![
                PortalEntry {
                    id: "a".repeat(32),
                    slug: "notes".to_owned(),
                    name: "Notes <script>alert(1)</script>".to_owned(),
                    description: "Quick & private notes".to_owned(),
                    port: Some(18788),
                },
                PortalEntry {
                    id: "b".repeat(32),
                    slug: "timer".to_owned(),
                    name: "Timer".to_owned(),
                    description: "Kitchen timer".to_owned(),
                    port: None,
                },
            ],
        }
    }

    #[test]
    fn launcher_lists_active_and_paused_pockets_with_escaped_html() {
        let res = render_response("GET", "/", &sample_snapshot());
        assert_eq!(res.status, "200 OK");
        assert!(res
            .body
            .contains("https://macbook-pro.tail00000.ts.net:18788/"));
        assert!(res
            .body
            .contains("Notes &lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!res.body.contains("<script>alert(1)</script>"));
        assert!(res.body.contains("Paused"));
        assert!(res.body.contains("/notes"));
        assert!(res.body.contains("/timer"));
    }

    #[test]
    fn named_slug_redirects_active_pockets_and_preserves_subpaths() {
        let snap = sample_snapshot();
        let root = render_response("GET", "/notes", &snap);
        assert_eq!(root.status, "302 Found");
        assert_eq!(
            root.location.as_deref(),
            Some("https://macbook-pro.tail00000.ts.net:18788/")
        );

        let deep = render_response("GET", "/NOTES/item/42?tab=edit", &snap);
        assert_eq!(deep.status, "302 Found");
        assert_eq!(
            deep.location.as_deref(),
            Some("https://macbook-pro.tail00000.ts.net:18788/item/42?tab=edit")
        );
    }

    #[test]
    fn paused_and_unknown_slugs_return_clear_status_pages() {
        let snap = sample_snapshot();
        let paused = render_response("GET", "/timer", &snap);
        assert_eq!(paused.status, "503 Service Unavailable");
        assert!(paused.body.contains("Timer is stopped"));

        let missing = render_response("GET", "/nope", &snap);
        assert_eq!(missing.status, "404 Not Found");
        assert!(missing.body.contains("Pocket not found"));
    }

    #[test]
    fn rejects_non_get_methods_and_header_injection() {
        let snap = sample_snapshot();
        assert_eq!(
            render_response("POST", "/notes", &snap).status,
            "405 Method Not Allowed"
        );
        assert_eq!(
            render_response("GET", "/notes\r\nX-Evil: 1", &snap).status,
            "400 Bad Request"
        );
    }
}
