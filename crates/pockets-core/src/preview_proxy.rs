//! The Pockets-owned host-side preview proxy (M4 live-preview slice).
//!
//! Follows `docs/live-preview.md` ("Mechanism" — Proxy and Morph client
//! bullets): a small reverse proxy on an OS-allocated loopback port sits
//! between the pane (or the escape-hatch browser) and the container's
//! loopback-only mapping. It injects one small client into `text/html`
//! responses and carries the single SSE that relays tiered updates and
//! `preparing | updating | ready | failed | stopped` status. Everything else
//! is forwarded untouched; the pane and the browser load this same proxy URL,
//! so one mechanism serves both, and the raw container URL is never the
//! user-facing address.
//!
//! Built on the tokio `net`/`io-util`/`sync` features already in the tree via
//! Tauri — no HTTP framework is added. The wire dialect is deliberately
//! narrow, sufficient for the starter's plain Node server:
//!
//! - **Keep-alive is closed by design.** One request per inbound connection:
//!   the proxy answers `Connection: close` and closes after the response
//!   body. Browsers, `curl`, and the pane tolerate this fully, and connection
//!   churn is irrelevant on loopback for a preview.
//! - **Request bodies** pass through when framed by `Content-Length` or
//!   `Transfer-Encoding: chunked`; anything else is treated as body-less.
//! - **Response bodies** are buffered only for injection (see
//!   [`should_inject`]); everything else streams through with its framing
//!   headers intact, byte-identical. Redirects and 304s pass through — never
//!   injected, never followed.
//! - **Encoded HTML is never injected into.** A compressed `text/html` body
//!   would be corrupted by byte insertion, so a `Content-Encoding` other than
//!   `identity` passes through untouched (the starter does not compress;
//!   plain bytes arrive uncompressed).
//! - **Upgrade requests** (`Upgrade:` header, e.g. WebSocket) are forwarded
//!   verbatim — the one case where `Connection:` survives instead of being
//!   forced to `close` — and a `101` response turns the connection into a
//!   raw two-way pipe for the life of the tunnel. There is no re-framing or
//!   injection inside a tunnel; a tunnel ends when either side closes.
//! - **Length adjustment on injection** recomputes `Content-Length` (and
//!   de-chunks) rather than re-encoding to chunked: the starter's plain Node
//!   server always answers rendered pages with `Content-Length`, so
//!   recomputing is the simple and exactly correct framing. Buffered responses
//!   are capped by [`MAX_INJECT_BYTES`]; an over-cap HTML page passes through
//!   un-injected rather than being truncated.
//!
//! The reserved path prefix [`RESERVED_PREFIX`] is never proxied upstream: it
//! serves the SSE stream, the injected client
//! (`src/assets/preview-client.js`, Pockets-owned), and the vendored morphdom
//! asset. The morphdom copy at `src/assets/morphdom-umd.js` is byte-identical
//! to `spikes/morph/morphdom-umd.js` — morphdom 2.7.8, MIT license, 31,952
//! bytes, sha256
//! `6b5bc205c08800dd59f1792032f723b7ee3aedae70ef64353c8233e0eed7c1cb`, from
//! unpkg (`spikes/morph/RESULTS.md`) — and because the asset file must stay
//! byte-identical, this provenance note lives here, not in the asset.
//!
//! The proxy is one instance per active preview, owned by the session's
//! `preview::PreviewEntry`: `preview::start` spawns it after the health probe
//! passes and records its URL as the user-facing url; `preview::stop` shuts
//! the listener down. Live updates arrive as [`LiveSignal`] values on the
//! session's broadcast fanout — published by the lifecycle on every state
//! transition and on a successful refresh — which each open SSE connection
//! renders into the client messages documented in `docs/live-preview.md`
//! ("Hot reload contract"): `{"type":"style","paths":[...]}`,
//! `{"type":"morph"}`, `{"type":"reload"}`, and `{"type":"state","state":...}`.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Notify};

use crate::preview::PreviewState;

/// Reserved, never-proxied path prefix. Everything under it is Pockets-owned,
/// exactly the way the docs make the proxy origin the only origin the pane
/// loads: the page cannot reach the container directly, and the lifecycle
/// control plane cannot be shadowed by an app route.
pub(crate) const RESERVED_PREFIX: &str = "/__pockets-live";

/// SSE fanout of one preview's live signals (docs: the proxy "carries the
/// single SSE for reload, tiered update, and status").
const STREAM_PATH: &str = "/__pockets-live/stream";

/// The injected client (Pockets-owned, dependency-free ES2020).
const CLIENT_PATH: &str = "/__pockets-live/client.js";

/// The vendored morphdom UMD asset (provenance in the module doc above).
const MORPHDOM_PATH: &str = "/__pockets-live/morphdom.js";

/// Pockets-owned preview client, embedded at compile time.
const CLIENT_JS: &str = include_str!("assets/preview-client.js");

/// Vendored morphdom 2.7.8 (MIT), byte-identical to
/// `spikes/morph/morphdom-umd.js`; sha256 asserted by
/// `morphdom_asset_bytes_match_the_recorded_sha256`.
const MORPHDOM_JS: &str = include_str!("assets/morphdom-umd.js");

/// Per-connection broadcast backlog before a stalled SSE consumer starts
/// dropping signals. Preview signals are sparse (one per transition, one per
/// refresh), and a pane that missed updates recovers through the polled
/// status surface, so lag is survivable rather than fatal.
pub(crate) const LIVE_CHANNEL_CAPACITY: usize = 64;

/// Hard cap on one request head line, so a garbage connection cannot make
/// the proxy buffer without bound.
const MAX_HEAD_LINE: usize = 16 * 1024;

/// Cap on a response body buffered for injection; over-cap HTML is forwarded
/// un-injected instead of truncated (a starter page is nowhere near this).
const MAX_INJECT_BYTES: usize = 8 * 1024 * 1024;

/// Idle pause between SSE heartbeat comments. The comment is also how a dead
/// consumer is noticed: the first heartbeat write to a closed socket fails
/// and the stream task exits.
const SSE_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

/// EventSource reconnect cadence announced at stream open (`retry:`), so a
/// reloaded pane resumes listening well inside a second chat turn.
const SSE_RETRY_MS: &str = "1000";

/// A live-update signal published by the preview lifecycle for one session
/// and consumed by that session's proxy — the interface `preview::Previews`
/// fans out and the SSE renders. This is the contract the harness edit-event
/// wiring and the frontend both consume:
///
/// - [`LiveSignal::State`] is published on every recorded transition
///   (`preparing`, `updating`, `ready`, `failed`, `stopped`) and relayed
///   verbatim as `{"type":"state","state":...}` for the frontend.
/// - [`LiveSignal::Changed`] carries the workspace-relative paths the harness
///   edit events reported, published on a successful `refresh` once the
///   restarted app is healthy. The proxy tiers them (see [`tier_of`] and
///   [`messages_for`]) into style/morph/reload client messages.
#[derive(Debug, Clone)]
pub(crate) enum LiveSignal {
    Reload,
    /// A lifecycle transition happened; relay it as a `state` message.
    State(PreviewState),
    /// These workspace-relative paths are live in the preview now; tier them.
    Changed(Vec<String>),
}

/// The live-update tier one changed path belongs to (`docs/live-preview.md`,
/// "Hot reload contract"). Classification is native and exact — the harness
/// edit events carry the paths — never guessed from content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Swap the affected stylesheet live; no reload, no restart.
    Style,
    /// Re-fetch the page and DOM-morph it (markup, server code, anything
    /// unknown: morphing a page whose content merely re-rendered is always
    /// correct, just less surgical than a style swap).
    Morph,
    /// Hard reload. The docs are explicit that "a changed static JS file
    /// hard-reloads" (client-script state is never preserved), and
    /// image/font assets have no live-swap mechanism — a morph would not
    /// bypass the browser cache — so reloading is the documented
    /// conservative choice for static binary assets.
    Reload,
}

/// Pure classification, the single decision point for tiering (table
/// asserted by `tier_of_follows_the_documented_extension_table`):
///
/// - `.css` → [`Tier::Style`].
/// - The root `server.js` → [`Tier::Morph`]: it is server code, and the
///   hot-reload contract restarts the process and **morphs** after it
///   (docs "Server-code changes"); reloading on every model edit would
///   throw away form state for no reason.
/// - Other `.js`/`.mjs`/`.cjs` and image/font extensions → [`Tier::Reload`]
///   (served static client scripts: their browser state is never preserved,
///   and binary assets have no live-swap mechanism).
/// - Everything else — markup, `pocket.yaml`, unknown extensions,
///   extension-less paths → [`Tier::Morph`].
pub(crate) fn tier_of(path: &str) -> Tier {
    // The starter's entry point is exactly root-level server.js; the same
    // name in a served directory would be a static script like any other.
    if path == "server.js" {
        return Tier::Morph;
    }
    // Only the final path segment's extension decides; a dot in a directory
    // name (`v1.2/app.js`) must not classify the file.
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let extension = match file.rfind('.') {
        // `idx > 0` keeps dotfiles (`.env`) extension-less.
        Some(idx) if idx > 0 => &file[idx + 1..],
        _ => "",
    };
    match extension.to_ascii_lowercase().as_str() {
        "css" => Tier::Style,
        "js" | "mjs" | "cjs" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "ico" | "bmp"
        | "woff" | "woff2" | "ttf" | "otf" | "eot" => Tier::Reload,
        _ => Tier::Morph,
    }
}

/// One client message exactly as the SSE carries it: `{"type":...}` plus
/// payload, matching the shapes `docs/live-preview.md` names and the
/// injected client acts on (`state` is the status relay for the frontend).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum ClientMessage {
    /// `{"type":"style","paths":[...]}` — bump these stylesheets, no reload.
    Style { paths: Vec<String> },
    /// `{"type":"morph"}` — re-fetch and DOM-morph with the form guard.
    Morph,
    /// `{"type":"reload"}` — hard reload (changed static JS/assets).
    Reload,
    /// `{"type":"state","state":"ready"}` — lifecycle status relay.
    State { state: PreviewState },
}

/// Maps one set of changed paths to the client frames for it. The whole
/// aggregation rule, asserted by `messages_for_aggregates_tiers_in_order`:
///
/// - **Reload preempts everything.** Any path classifies to
///   [`Tier::Reload`], which subsumes style and morph (a reload re-fetches
///   everything anyway); one `reload` frame is sent and nothing else.
/// - Otherwise **morph first, then style**: a morph re-parses server markup
///   carrying the *original* stylesheet hrefs, so the style cache-buster has
///   to run after the morph or morphdom would rewrite the busted href back.
/// - An empty path list means "something restarted but nothing specific was
///   reported": morphing is the safe re-sync, so it yields one `morph` frame.
pub(crate) fn messages_for(paths: &[String]) -> Vec<ClientMessage> {
    if paths.is_empty() {
        return vec![ClientMessage::Morph];
    }
    let mut tiers: Vec<Tier> = paths.iter().map(|path| tier_of(path)).collect();
    tiers.sort_by_key(|tier| match tier {
        Tier::Reload => 0,
        Tier::Morph => 1,
        Tier::Style => 2,
    });
    tiers.dedup();
    match tiers.first().copied() {
        Some(Tier::Reload) => vec![ClientMessage::Reload],
        _ => {
            let mut frames = Vec::new();
            if tiers.contains(&Tier::Morph) {
                frames.push(ClientMessage::Morph);
            }
            let css: Vec<String> = paths
                .iter()
                .filter(|path| tier_of(path) == Tier::Style)
                .cloned()
                .collect();
            if !css.is_empty() {
                frames.push(ClientMessage::Style { paths: css });
            }
            frames
        }
    }
}

/// Renders one lifecycle signal into the client frames it implies.
fn frames_for(signal: LiveSignal) -> Vec<ClientMessage> {
    match signal {
        LiveSignal::Reload => vec![ClientMessage::Reload],
        LiveSignal::State(state) => vec![ClientMessage::State { state }],
        LiveSignal::Changed(paths) => messages_for(&paths),
    }
}

/// A running proxy for one preview: the OS-allocated loopback URL that is the
/// user-facing address (docs: the pane and the escape hatch share it), plus
/// the handle that shuts the listener down. Cheap to clone; every clone stops
/// the same listener. The accept loop holds the session's [`LiveSignal`]
/// sender, so stopping the listener also drops that sender and every open SSE
/// stream sees `Closed` and ends.
#[derive(Clone, Debug)]
pub(crate) struct ProxyHandle {
    url: Arc<String>,
    local: SocketAddr,
    shutdown: Arc<Notify>,
    upstream: Arc<std::sync::RwLock<SocketAddr>>,
}

impl ProxyHandle {
    pub(crate) fn retarget(&self, container_url: &str) -> Result<(), String> {
        let target = parse_upstream(container_url)?;
        *self.upstream.write().unwrap_or_else(|e| e.into_inner()) = target;
        Ok(())
    }
    /// The user-facing proxy URL (`http://127.0.0.1:<port>`), the value
    /// `preview::Previews` records as the entry url on `ready`.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The bound loopback address, the assertion surface for "loopback only"
    /// (`proxy_binds_loopback_only`). Kept off the production call graph
    /// deliberately — nothing should route by it; routing is by the URL.
    #[allow(dead_code)]
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Ends the accept loop, so no new connection is accepted. In-flight
    /// requests finish their response and close; open SSE streams end when
    /// the accept task drops the fanout sender. Idempotent.
    pub(crate) fn stop(&self) {
        self.shutdown.notify_one();
    }
}

/// The validated upstream target: exactly the literal `127.0.0.1:<port>` the
/// engine publishes. Parsing is deliberately stricter than a URL parser —
/// `preview.rs` builds this shape natively and nothing (never the model,
/// never the frontend, never a file) chooses it, so any other spelling is a
/// bug or an attack and is refused before a socket is opened.
fn parse_upstream(container_url: &str) -> Result<SocketAddr, String> {
    let authority = container_url.strip_prefix("http://").ok_or_else(|| {
        format!("the preview proxy only speaks plain http upstream, found {container_url:?}")
    })?;
    // No path, userinfo, or query may hide inside the authority position.
    if authority.contains('/') || authority.contains('@') || authority.contains('?') {
        return Err(format!(
            "the preview upstream URL is not a bare loopback authority: {container_url:?}"
        ));
    }
    let (host, port) = authority.rsplit_once(':').ok_or_else(|| {
        format!("the preview upstream URL must carry an explicit port: {container_url:?}")
    })?;
    if host != Ipv4Addr::LOCALHOST.to_string() {
        return Err(format!(
            "the preview proxy only forwards to {}, found {container_url:?}",
            Ipv4Addr::LOCALHOST
        ));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| format!("the preview upstream port is not a number: {container_url:?}"))?;
    if port == 0 {
        return Err(format!(
            "the preview upstream port must be non-zero: {container_url:?}"
        ));
    }
    Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
}
/// Spawns the proxy for one preview on an OS-allocated ephemeral loopback
/// port. `container_url` must be the validated `http://127.0.0.1:<port>`
/// mapping `preview.rs` reconciled from Podman; `live` is the session's
/// fanout sender, which the accept loop owns for the listener's lifetime.
/// The bind is pinned to [`Ipv4Addr::LOCALHOST`] — never a wildcard — so a
/// preview is reachable only from this machine, mirroring the engine-side
/// loopback-only publish (docs: "host-published loopback ports").
pub(crate) async fn spawn(
    container_url: &str,
    live: broadcast::Sender<LiveSignal>,
) -> Result<ProxyHandle, String> {
    let upstream = Arc::new(std::sync::RwLock::new(parse_upstream(container_url)?));
    let loop_upstream = Arc::clone(&upstream);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|err| format!("the preview proxy could not bind a loopback port: {err}"))?;
    let local = listener
        .local_addr()
        .map_err(|err| format!("the preview proxy port could not be inspected: {err}"))?;
    debug_assert_eq!(local.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

    let shutdown = Arc::new(Notify::new());
    let loop_shutdown = Arc::clone(&shutdown);
    let url = Arc::new(format!("http://{local}"));
    tokio::spawn(async move {
        loop {
            // `select!` rather than a polling loop: `stop()` wakes
            // `notified()` immediately (and a `notify_one` recorded before
            // this poll is honored without waiting), and the accept future
            // is cancelled cleanly between connections.
            tokio::select! {
                _ = loop_shutdown.notified() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { break };
                    // Subscribe at accept time: connection tasks hold only a
                    // Receiver, so dropping the last Sender (listener stopped
                    // and the registry detached) ends every open stream.
                    let receiver = live.subscribe();
                    let upstream = *loop_upstream.read().unwrap_or_else(|e| e.into_inner());
                    tokio::spawn(async move {
                        serve_connection(stream, upstream, receiver).await;
                    });
                }
            }
        }
        // Refuse new connections before closing the stream fanout, so a
        // client that observes the SSE close cannot reconnect to this port.
        drop(listener);
        drop(live);
    });
    Ok(ProxyHandle {
        url,
        local,
        shutdown,
        upstream,
    })
}

/// Serves one inbound connection: exactly one request, then close (the
/// documented keep-alive choice). Inbound garbage, oversized heads, and
/// unsupported forms are refused with a small generated response, never a
/// panic and never a forward.
async fn serve_connection(
    stream: TcpStream,
    upstream: SocketAddr,
    receiver: broadcast::Receiver<LiveSignal>,
) {
    let _ = stream.set_nodelay(true);
    let (client_read, mut client_write) = stream.into_split();
    let mut client_read = BufReader::new(client_read);
    let request = match read_request_head(&mut client_read).await {
        Ok(Some(request)) => request,
        // EOF before a request line is a normal idle-close; anything else
        // was already answered with a refusal.
        Ok(None) => return,
        Err(response) => {
            let _ = response.write_to(&mut client_write).await;
            let _ = client_write.flush().await;
            return;
        }
    };

    // The reserved prefix is answered here and nowhere else: a single `?`-or
    // -fragment-stripped route match keeps an app route from ever shadowing
    // the control plane, and upstream never sees these paths at all.
    let route = request
        .path
        .split_once('?')
        .map_or(request.path.as_str(), |(path, _)| path);
    if route.starts_with(RESERVED_PREFIX) {
        match route {
            STREAM_PATH => serve_sse(&mut client_write, receiver).await,
            CLIENT_PATH => serve_asset(&mut client_write, CLIENT_JS.as_bytes()).await,
            MORPHDOM_PATH => serve_asset(&mut client_write, MORPHDOM_JS.as_bytes()).await,
            _ => {
                let _ = proxy_error(
                    &mut client_write,
                    404,
                    "not found under the reserved prefix",
                )
                .await;
            }
        }
        return;
    }

    if matches!(request.method.as_str(), "CONNECT" | "TRACE") {
        let _ = proxy_error(
            &mut client_write,
            405,
            "method not allowed on a preview proxy",
        )
        .await;
        return;
    }
    if !request.path.starts_with('/') {
        // Only origin-form requests are proxied: the browser targets this
        // proxy directly, so absolute-form or authority-form lines mean
        // someone is trying to steer the proxy elsewhere.
        let _ = proxy_error(
            &mut client_write,
            400,
            "expected an origin-form request target",
        )
        .await;
        return;
    }

    if let Err(err) = forward(&mut client_read, &mut client_write, upstream, &request).await {
        // The client is already gone in most cases; a mid-stream upstream
        // failure is reported honestly as a clean connection close, which
        // browsers surface as a failed load.
        if err.kind() != io::ErrorKind::UnexpectedEof && err.kind() != io::ErrorKind::BrokenPipe {
            eprintln!(
                "preview proxy: {upstream} {} {}: {err}",
                request.method, request.path
            );
        }
    }
}

/// One parsed inbound request head.
#[derive(Clone)]
struct Request {
    method: String,
    /// Origin-form target, including any query string.
    path: String,
    /// Raw `(name, value)` pairs in wire order, names still in the sender's
    /// casing; [`Request::header`] and [`Request::header_tokens`] do the
    /// case-insensitive lookups, while forwarding writes [`Request`]
    /// verbatim minus the headers the proxy owns.
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Whether `name` is present and its comma list contains `token`
    /// (case-insensitive), the HTTP token-list rule the `Connection`,
    /// `Upgrade`, and `Transfer-Encoding` checks need.
    fn header_tokens(&self, name: &str, token: &str) -> bool {
        self.header(name).is_some_and(|value| {
            value
                .split(',')
                .any(|candidate| candidate.trim().eq_ignore_ascii_case(token))
        })
    }

    fn wants_upgrade(&self) -> bool {
        self.headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("upgrade"))
    }
}

/// A generated refusal response for the pre-forward stage, before any
/// upstream exists.
struct Refusal {
    status: u16,
    message: &'static str,
}

impl Refusal {
    async fn write_to<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        let body = format!("{} {}\n", self.status, self.message);
        writer.write_all(b"HTTP/1.1 ").await?;
        writer.write_all(self.status.to_string().as_bytes()).await?;
        writer.write_all(b" ").await?;
        writer.write_all(self.message.as_bytes()).await?;
        writer
            .write_all(
                format!(
                    "\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await?;
        writer.write_all(body.as_bytes()).await?;
        writer.flush().await
    }
}

/// Reads the request line and headers, leaving the body (if any) buffered in
/// `reader` for [`forward`] to copy. `Ok(None)` is a clean pre-request EOF;
/// `Err` carries the refusal to send.
async fn read_request_head<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Request>, Refusal> {
    let request_line = read_head_line(reader).await.map_err(|_| Refusal {
        status: 431,
        message: "request line too large",
    })?;
    let Some(request_line) = request_line else {
        return Ok(None);
    };
    let mut parts = request_line.trim_end().splitn(3, ' ');
    let (Some(method), Some(path), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(Refusal {
            status: 400,
            message: "malformed request line",
        });
    };
    if !version.eq_ignore_ascii_case("HTTP/1.1") && !version.eq_ignore_ascii_case("HTTP/1.0") {
        return Err(Refusal {
            status: 505,
            message: "unsupported HTTP version",
        });
    }
    // Reject control characters before anything is echoed back upstream or
    // into a generated response (response-splitting hygiene).
    if path.chars().any(|c| matches!(c, '\r' | '\n' | '\0')) {
        return Err(Refusal {
            status: 400,
            message: "malformed request target",
        });
    }

    let mut headers = Vec::new();
    loop {
        let line = read_head_line(reader).await.map_err(|_| Refusal {
            status: 431,
            message: "header section too large",
        })?;
        let Some(line) = line else { break };
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        // Header folding (obs-fold) is not accepted: browsers do not send it
        // and the starter never produces it.
        let Some((name, value)) = line.split_once(':') else {
            return Err(Refusal {
                status: 400,
                message: "malformed header line",
            });
        };
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    Ok(Some(Request {
        method: method.to_ascii_uppercase(),
        path: path.to_owned(),
        headers,
    }))
}

/// One CRLF-terminated head line, or `None` at a clean EOF before the line
/// began, or `Err(())` past [`MAX_HEAD_LINE`] (the partial line is dropped;
/// the caller refuses the connection).
async fn read_head_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<String>, ()> {
    let mut line = String::new();
    loop {
        let read = reader.read_line(&mut line).await.map_err(|_| ())?;
        if read == 0 {
            return Ok(if line.is_empty() { None } else { Some(line) });
        }
        if line.ends_with('\n') {
            return Ok(Some(line));
        }
        if line.len() > MAX_HEAD_LINE {
            return Err(());
        }
    }
}

/// A small generated proxy error with a plain-text body.
async fn proxy_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    status: u16,
    message: &str,
) -> io::Result<()> {
    // The message is a fixed literal from this module, never attacker input.
    let body = format!("{status} {message}\n");
    write_all_parts(
        writer,
        &[
            b"HTTP/1.1 ",
            status.to_string().as_bytes(),
            b" ",
            message.as_bytes(),
            b"\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: ",
            body.len().to_string().as_bytes(),
            b"\r\nconnection: close\r\n\r\n",
            body.as_bytes(),
        ],
    )
    .await
}

/// Serves one Pockets-owned asset as `text/javascript`, uncached by design
/// (docs: "no caching layer"): a proxy restart reallocates the port, and a
/// stale client must never survive a reload.
async fn serve_asset<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) {
    let _ = write_all_parts(
        writer,
        &[
            b"HTTP/1.1 200 OK\r\ncontent-type: text/javascript; charset=utf-8\r\ncontent-length: ",
            bytes.len().to_string().as_bytes(),
            b"\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
            bytes,
        ],
    )
    .await;
}

/// Writes byte slices in order and flushes once.
async fn write_all_parts<W: AsyncWrite + Unpin>(writer: &mut W, parts: &[&[u8]]) -> io::Result<()> {
    for part in parts {
        writer.write_all(part).await?;
    }
    writer.flush().await
}
/// Requests a forwarding, with the framing the body must be read with.
enum RequestFraming {
    None,
    Length(u64),
    Chunked,
}

/// Forwards one request to the container and relays the response.
///
/// The forwarded request is untouched beyond what the proxy contract needs:
/// the request line is replayed verbatim, hop-by-hop headers
/// (`Connection`, `Keep-Alive`, `TE`, `Trailer`, `Proxy-Authorize*`,
/// `Proxy-Authenticate`) are dropped and a `Host` matching the upstream and
/// `Connection: close` are asserted — except for upgrade requests, where the
/// original head (including `Connection: Upgrade`) is replayed byte-for-byte
/// so the handshake stays valid. The response is then either streamed
/// through with its framing headers intact or, for injectable HTML, buffered
/// once and answered with an adjusted `Content-Length`.
async fn forward<R, W>(
    client_read: &mut BufReader<R>,
    client_write: &mut W,
    upstream: SocketAddr,
    request: &Request,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let container = TcpStream::connect(upstream).await.map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("the preview container at {upstream} is not reachable: {err}"),
        )
    })?;
    let _ = container.set_nodelay(true);
    let (upstream_read, mut upstream_write) = container.into_split();
    let mut upstream_read = BufReader::new(upstream_read);

    if request.wants_upgrade() {
        // Replay the client head verbatim (original Host included): the one
        // flow where `Connection:` survives, so the tunnel is negotiated
        // exactly as the client intended. No body is expected pre-handshake.
        write_head(
            &mut upstream_write,
            &request.method,
            &request.path,
            &request.headers,
            None,
        )
        .await?;
        let response = read_response_head(&mut upstream_read).await?;
        if response.status == 101 {
            relay_head(client_write, &response).await?;
            // Raw two-way pipe until either side closes; no framing, no
            // injection inside a tunnel.
            let client_to_upstream = tokio::io::copy(client_read, &mut upstream_write);
            let upstream_to_client = tokio::io::copy(&mut upstream_read, client_write);
            let _ = tokio::join!(client_to_upstream, upstream_to_client);
            return Ok(());
        }
        // Server refused the upgrade: fall through to ordinary relay.
        return relay_body(client_write, &mut upstream_read, response, &request.method).await;
    }

    let framing = match request.header_tokens("transfer-encoding", "chunked") {
        true => RequestFraming::Chunked,
        false => match request
            .header("content-length")
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            Some(length) => RequestFraming::Length(length),
            None => RequestFraming::None,
        },
    };
    // `write_head` terminates every asserted line itself: the asserted `Host`
    // is a bare line, never pre-terminated. (A trailing CRLF here emitted a
    // stray blank line, ending the forwarded head early — Node then read the
    // `Connection` line as pipelined garbage and answered a bare 400. Every
    // injectable HTML page through the proxy failed; JSON endpoints happened
    // to be probed before the parser reached the poisoned tail.)
    let host = format!("Host: {upstream}");
    let forwarded: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.to_ascii_lowercase().as_str(),
                "connection"
                    | "keep-alive"
                    | "te"
                    | "trailer"
                    | "proxy-authorization"
                    | "proxy-authenticate"
                    | "host"
            )
        })
        .cloned()
        .collect();
    write_head(
        &mut upstream_write,
        &request.method,
        &request.path,
        &forwarded,
        Some((host.as_str(), "Connection: close")),
    )
    .await?;
    copy_request_body(client_read, &mut upstream_write, framing).await?;
    upstream_write.flush().await?;

    let response = read_response_head(&mut upstream_read).await?;
    if should_inject(request, &response) {
        let original = read_framed(
            &mut upstream_read,
            response.framing_source(),
            MAX_INJECT_BYTES,
        )
        .await?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the HTML response exceeds the injection cap and was not forwarded \
                         (it would have been truncated)",
            )
        })?;
        let injected = inject_client(&original);
        let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, response.reason);
        for (name, value) in &response.headers {
            let lower = name.to_ascii_lowercase();
            // Length and chunk framing are replaced by the recomputed
            // Content-Length; every other header passes through untouched.
            if matches!(
                lower.as_str(),
                "content-length" | "transfer-encoding" | "connection"
            ) {
                continue;
            }
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("content-length: ");
        head.push_str(&injected.len().to_string());
        head.push_str("\r\nconnection: close\r\n\r\n");
        write_all_parts(client_write, &[head.as_bytes(), &injected]).await?;
        return Ok(());
    }
    relay_body(client_write, &mut upstream_read, response, &request.method).await
}

/// Writes a request head: request line, the given headers, a final CRLF.
/// `asserted` optionally appends `(Host line, Connection line)` the
/// non-upgrade flow uses; asserted lines must NOT carry their own line
/// terminator — this function owns all CRLFs (a pre-terminated line would
/// emit a stray blank line and end the head early).
async fn write_head<W: AsyncWrite + Unpin>(
    writer: &mut W,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    asserted: Option<(&str, &str)>,
) -> io::Result<()> {
    write_all_parts(
        writer,
        &[method.as_bytes(), b" ", path.as_bytes(), b" HTTP/1.1\r\n"],
    )
    .await?;
    let mut head = Vec::new();
    for (name, value) in headers {
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    if let Some((host, connection)) = asserted {
        head.extend_from_slice(host.as_bytes());
        head.extend_from_slice(b"\r\n");
        head.extend_from_slice(connection.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    writer.write_all(&head).await
}

/// Whether this response gets the injected script tag — the single place the
/// rule is decided (asserted by `injection_happens_only_for_2xx_text_html`):
/// a plain `GET` (HEAD has no body to inject into), a 2xx status that is
/// neither 204 nor 206 (no body; partial content must not be re-framed), an
/// uncompressed `text/html` content type, and an upgrade response is already
/// handled before this point. Non-HTML, redirects, 304s, and encoded bodies
/// pass through byte-identical.
fn should_inject(request: &Request, response: &ResponseHead) -> bool {
    if request.method != "GET" || response.upgraded {
        return false;
    }
    let injectable_status =
        (200..=299).contains(&response.status) && response.status != 204 && response.status != 206;
    if !injectable_status {
        return false;
    }
    let Some(content_type) = response.header("content-type") else {
        return false;
    };
    // `text/html` with an optional charset (or anything after a `;`) still
    // matches; `application/xhtml+xml` and friends deliberately do not.
    let is_html = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .eq_ignore_ascii_case("text/html");
    if !is_html {
        return false;
    }
    // Any real content encoding means compressed bytes that byte insertion
    // would corrupt; pass those through untouched.
    match response.header("content-encoding") {
        None => true,
        Some(encoding) => encoding.trim().eq_ignore_ascii_case("identity"),
    }
}

/// Inserts exactly one script tag before the last case-insensitive `</body`
/// (`docs/live-preview.md`: one small injected client), appending at the end
/// of the document when no closing tag exists. The tag is fixed, carries no
/// state, and points only at the reserved proxy path — same-origin by
/// construction.
pub(crate) fn inject_client(html: &[u8]) -> Vec<u8> {
    const TAG: &[u8] = b"<script src=\"/__pockets-live/client.js\" defer></script>";
    // Last `</body` whose following byte legally continues the tag (`>` or
    // whitespace before attributes); ASCII-case-insensitive, so `</BODY>`
    // and `</BoDy ` both match.
    let mut insert_at = None;
    let mut index = 0usize;
    while index + 6 <= html.len() {
        if html[index..index + 6].eq_ignore_ascii_case(b"</body") {
            let next = html.get(index + 6).copied();
            if next.is_none_or(|byte| byte == b'>' || byte.is_ascii_whitespace()) {
                insert_at = Some(index);
            }
        }
        index += 1;
    }
    let mut output = Vec::with_capacity(html.len() + TAG.len());
    match insert_at {
        Some(at) => {
            output.extend_from_slice(&html[..at]);
            output.extend_from_slice(TAG);
            output.extend_from_slice(&html[at..]);
        }
        None => {
            output.extend_from_slice(html);
            output.extend_from_slice(TAG);
        }
    }
    output
}

/// The parsed upstream response head plus the framing its body needs.
struct ResponseHead {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    /// Raw status line bytes minus the CRLF, replayed verbatim on relay so a
    /// non-200 reason phrase survives exactly.
    status_line: Vec<u8>,
    /// 101 responses complete the tunnel handshake instead of framing a body.
    upgraded: bool,
}

impl ResponseHead {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// How the body following this head is framed. `None` means "no body":
    /// 204/304 or a HEAD response — including when a `Content-Length` header
    /// claims one (HEAD to an HTML page carries the page's length but no
    /// body, and reading it would desynchronize the stream).
    fn framing(&self, method: &str) -> Option<BodyFraming> {
        if self.status == 101 || self.status == 204 || self.status == 304 || method == "HEAD" {
            return None;
        }
        // Token-list check, so `chunked, gzip` is still chunked-framed.
        let chunked = self.header("transfer-encoding").is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("chunked"))
        });
        if chunked {
            return Some(BodyFraming::Chunked);
        }
        if let Some(length) = self
            .header("content-length")
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            return Some(BodyFraming::Length(length));
        }
        Some(BodyFraming::UntilEof)
    }

    fn framing_source(&self) -> Option<BodyFraming> {
        self.framing("GET")
    }
}

/// Response body framing, in priority order: chunked wins over
/// `Content-Length` (a chunked reply that also echoes the decoded length is
/// a known quirk of small servers), and no framing at all means the body
/// runs to connection close — always terminal, because the proxy forwarded
/// `Connection: close`.
enum BodyFraming {
    Length(u64),
    Chunked,
    UntilEof,
}

/// Reads the upstream status line and headers.
async fn read_response_head<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> io::Result<ResponseHead> {
    let status_line = read_head_line(reader)
        .await
        .map_err(|_| io::Error::other("upstream request line too large"))?
        .ok_or_else(|| io::Error::other("the preview closed before its status line"))?;
    let status_line = status_line.trim_end_matches(['\r', '\n']);
    let mut parts = status_line.splitn(3, ' ');
    let (Some(version), Some(code), reason) = (parts.next(), parts.next(), parts.next()) else {
        return Err(io::Error::other(format!(
            "upstream status line is malformed: {status_line:?}"
        )));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(io::Error::other(format!(
            "upstream did not answer HTTP/1.x: {status_line:?}"
        )));
    }
    let status: u16 = code
        .parse()
        .map_err(|_| io::Error::other(format!("upstream status code is not numeric: {code:?}")))?;

    let mut headers = Vec::new();
    loop {
        let line = read_head_line(reader)
            .await
            .map_err(|_| io::Error::other("upstream header section too large"))?;
        let Some(line) = line else { break };
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::other("upstream header line is malformed"));
        };
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    Ok(ResponseHead {
        status,
        reason: reason.unwrap_or_default().to_owned(),
        upgraded: status == 101,
        status_line: status_line.as_bytes().to_vec(),
        headers,
    })
}

/// Relays a `101` handshake head verbatim — every header, including
/// `Upgrade` and `Connection`, must survive for the tunnel to hold.
async fn relay_head<W: AsyncWrite + Unpin>(
    client_write: &mut W,
    response: &ResponseHead,
) -> io::Result<()> {
    let mut head: Vec<u8> = response.status_line.clone();
    head.extend_from_slice(b"\r\n");
    for (name, value) in &response.headers {
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    client_write.write_all(&head).await?;
    client_write.flush().await
}

/// Relays a response head verbatim (minus hop-by-hop headers, plus the
/// proxy's own `Connection: close`) and copies the body with its framing.
async fn relay_body<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    client_write: &mut W,
    upstream_read: &mut BufReader<R>,
    response: ResponseHead,
    method: &str,
) -> io::Result<()> {
    let mut head: Vec<u8> = Vec::new();
    head.extend_from_slice(&response.status_line);
    head.extend_from_slice(b"\r\n");
    for (name, value) in &response.headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "proxy-authorization"
                | "proxy-authenticate"
                | "transfer-encoding"
        ) {
            continue;
        }
        // The chunk framing itself is re-encoded below, so a chunked body is
        // relayed with `Transfer-Encoding` stripped from the head — except
        // for tunnels, which never pass through here.
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    // The framing copy re-emits chunked bodies chunk-framed; the head line
    // above dropped the original `Transfer-Encoding`, so assert it again for
    // a chunked reply.
    let framing = response.framing(method);
    let chunked = framing
        .as_ref()
        .is_some_and(|framing| matches!(framing, BodyFraming::Chunked));
    if chunked {
        head.extend_from_slice(b"transfer-encoding: chunked\r\n");
    }
    head.extend_from_slice(b"connection: close\r\n\r\n");
    client_write.write_all(&head).await?;

    match framing {
        None => client_write.flush().await,
        Some(BodyFraming::Length(length)) => {
            let copied = tokio::io::copy(&mut upstream_read.take(length), client_write).await?;
            if copied != length {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("the preview closed after {copied} of {length} body bytes"),
                ));
            }
            client_write.flush().await
        }
        Some(BodyFraming::Chunked) => copy_chunked(upstream_read, client_write).await,
        Some(BodyFraming::UntilEof) => {
            // Terminal framing: upstream honored `Connection: close`, so the
            // body runs to EOF and the close is itself the terminator.
            tokio::io::copy(upstream_read, client_write).await?;
            client_write.flush().await
        }
    }
}
/// Copies the inbound request body to the container with the framing the
/// client announced. `Content-Length` is copied exactly; chunked bodies are
/// re-emitted chunk-framed (chunk sizes recomputed, trailers tolerated by
/// consuming the trailer section); anything else has no body to copy.
async fn copy_request_body<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    client_read: &mut BufReader<R>,
    upstream_write: &mut W,
    framing: RequestFraming,
) -> io::Result<()> {
    match framing {
        RequestFraming::None => Ok(()),
        RequestFraming::Length(length) => {
            let copied = tokio::io::copy(&mut client_read.take(length), upstream_write).await?;
            if copied != length {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("the client sent {copied} of {length} declared body bytes"),
                ));
            }
            Ok(())
        }
        RequestFraming::Chunked => copy_chunked(client_read, upstream_write).await,
    }
}

/// Copies a chunked body frame-by-frame, re-emitting canonical chunk framing
/// (hex size, payload, CRLF, zero terminator). Chunk extensions
/// (`size;ext`) are preserved in the re-emitted size line only in their
/// bare-size part — the starter produces none, and re-serializing the size
/// keeps the output valid regardless.
async fn copy_chunked<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut BufReader<R>,
    writer: &mut W,
) -> io::Result<()> {
    loop {
        let size = read_chunk_size(reader).await?;
        if size == 0 {
            // Trailer section (headers or the lone CRLF): consume and end.
            loop {
                let line = read_head_line(reader)
                    .await
                    .map_err(|_| io::Error::other("chunk trailer line too large"))?;
                match line {
                    None => break,
                    Some(line) if line.trim_end().is_empty() => break,
                    Some(_) => continue,
                }
            }
            writer.write_all(b"0\r\n\r\n").await?;
            return writer.flush().await;
        }
        write_all_parts(writer, &[format!("{size:x}\r\n").as_bytes()]).await?;
        let copied = tokio::io::copy(&mut reader.take(size), writer).await?;
        if copied != size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the chunked body ended inside a chunk",
            ));
        }
        // Consume the CRLF terminating this chunk's payload — re-emitting it
        // below is worthless if the reader leaves it in the stream, because
        // it would then be parsed as the next chunk's size line.
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).await?;
        if &crlf != b"\r\n" {
            return Err(io::Error::other("chunk payload is not CRLF-terminated"));
        }
        writer.write_all(b"\r\n").await?;
    }
}

/// Reads one chunk size line (`<hex>[;ext]\r\n`); rejects oversized chunks
/// the way [`MAX_INJECT_BYTES`] bounds buffered reads.
async fn read_chunk_size<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> io::Result<u64> {
    let line = read_head_line(reader)
        .await
        .map_err(|_| io::Error::other("chunk size line too large"))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "chunked body without a terminator",
            )
        })?;
    let token = line.trim_end().split(';').next().unwrap_or_default().trim();
    let size = u64::from_str_radix(token, 16)
        .map_err(|_| io::Error::other(format!("chunk size line is malformed: {line:?}")))?;
    if size > MAX_INJECT_BYTES as u64 {
        return Err(io::Error::other("chunk larger than the proxy bound"));
    }
    Ok(size)
}

/// Reads a framed body completely for buffering (injection path). `Ok(None)`
/// means the body exceeded `budget` and must not be forwarded; chunked
/// bodies are decoded to their concatenation (the de-chunking the injection
/// response's `Content-Length` then describes).
async fn read_framed<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    framing: Option<BodyFraming>,
    budget: usize,
) -> io::Result<Option<Vec<u8>>> {
    let Some(framing) = framing else {
        return Ok(Some(Vec::new()));
    };
    match framing {
        BodyFraming::Length(length) => {
            if length > budget as u64 {
                return Ok(None);
            }
            let mut body = vec![0u8; length as usize];
            reader.read_exact(&mut body).await?;
            Ok(Some(body))
        }
        BodyFraming::Chunked => {
            let mut body = Vec::new();
            loop {
                let size = read_chunk_size(reader).await?;
                if size == 0 {
                    // Consume the trailer section, exactly as `copy_chunked`
                    // does, leaving the stream at the next-message boundary.
                    loop {
                        let line = read_head_line(reader)
                            .await
                            .map_err(|_| io::Error::other("chunk trailer line too large"))?;
                        match line {
                            None => break,
                            Some(line) if line.trim_end().is_empty() => break,
                            Some(_) => continue,
                        }
                    }
                    return Ok((body.len() <= budget).then_some(body));
                }
                if body.len() as u64 + size > budget as u64 {
                    return Ok(None);
                }
                let before = body.len();
                reader
                    .take(size)
                    .read_to_end(&mut body)
                    .await
                    .map_err(|_| io::Error::other("chunked body ended inside a chunk"))?;
                if body.len() - before != size as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the chunked body ended inside a chunk",
                    ));
                }
                // Consume the CRLF terminating this chunk's payload.
                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf).await?;
            }
        }
        BodyFraming::UntilEof => {
            // A preview upstream closes after a close-framed body, so EOF is
            // the terminator; the budget still bounds the buffer.
            let mut body = Vec::new();
            loop {
                let chunk = reader.fill_buf().await?;
                if chunk.is_empty() {
                    return Ok((body.len() <= budget).then_some(body));
                }
                if body.len() + chunk.len() > budget {
                    return Ok(None);
                }
                body.extend_from_slice(chunk);
                let taken = chunk.len();
                reader.consume(taken);
            }
        }
    }
}

/// Serves the SSE stream: `text/event-stream`, uncached, one `data:` frame
/// per JSON client message. Frames are `data: {...}\n\n`; keep-alive comments
/// double as dead-consumer detection. The stream ends when the lifecycle or
/// the listener drops every fanout sender (`Closed`) or a write fails.
async fn serve_sse<W: AsyncWrite + Unpin>(
    writer: &mut W,
    mut receiver: broadcast::Receiver<LiveSignal>,
) {
    if write_all_parts(
        writer,
        &[
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
            format!("retry: {SSE_RETRY_MS}\n\n").as_bytes(),
        ],
    )
    .await
    .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            signal = receiver.recv() => match signal {
                Ok(signal) => {
                    for message in frames_for(signal) {
                        if write_frame(writer, &message).await.is_err() {
                            return;
                        }
                    }
                }
                // A stalled consumer that fell behind the backlog misses
                // intermediate updates; the polled status surface and the
                // next change re-sync it, so skip and keep serving.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                // Every proxy sender dropped: stop closed the preview.
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = tokio::time::sleep(SSE_HEARTBEAT) => {
                // Comment frame: ignored by EventSource, fatal to a socket
                // the browser already closed.
                if write_all_parts(writer, &[b": ping\n\n"]).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Writes one SSE `data:` frame with a JSON body (compact serde output;
/// message JSON is single-line by construction).
async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &ClientMessage,
) -> io::Result<()> {
    let json = serde_json::to_vec(message)
        .map_err(|err| io::Error::other(format!("client message could not be encoded: {err}")))?;
    let mut frame = Vec::with_capacity(json.len() + 8);
    frame.extend_from_slice(b"data: ");
    frame.extend_from_slice(&json);
    frame.extend_from_slice(b"\n\n");
    writer.write_all(&frame).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests;
