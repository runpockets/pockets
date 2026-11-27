//! Authenticated loopback HTTP daemon serving the Pockets Web UI and RPC API.
//!
//! Security boundary:
//! - Binds strictly to `127.0.0.1:<port>` (never `0.0.0.0`).
//! - Validates `Host` against `pockets.localhost:<port>`, `localhost:<port>`,
//!   or `127.0.0.1:<port>` on every request to block DNS rebinding.
//! - Validates `Origin` on every state-changing (`POST`) request so neither
//!   arbitrary websites nor running Pockets on other `*.localhost` ports can
//!   invoke Pockets control endpoints.
//! - Protects `/api/*` (except `/api/health`) with a per-daemon 256-bit random
//!   session token verified via `HttpOnly; SameSite=Strict` cookie or
//!   `Authorization: Bearer <token>`.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{header, HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::{Frame, Incoming, SizeHint};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncRead;
use tokio::net::TcpListener;
use uuid::Uuid;

use pockets_core::{pocket::MAX_BACKUP_ARCHIVE_BYTES, project_lifecycle, tailscale, AppContext};

use crate::rpc;

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

pub const DEFAULT_PORT: u16 = 18786;
const COOKIE_NAME: &str = "pockets_session";
const MAX_RPC_BODY_BYTES: usize = 1024 * 1024;
const BACKUP_STREAM_CHUNK_BYTES: usize = 64 * 1024;
const BOOTSTRAP_CODE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_PENDING_BOOTSTRAP_CODES: usize = 64;

/// Streaming body backed by a staged temporary file on disk. Deletes the temp
/// file automatically when EOF is reached or if the client disconnects early.
struct TempFileStreamBody {
    file: tokio::fs::File,
    temp_path: Option<PathBuf>,
    remaining: u64,
    buf: Vec<u8>,
}

impl TempFileStreamBody {
    fn new(file: tokio::fs::File, temp_path: PathBuf, len: u64) -> Self {
        Self {
            file,
            temp_path: Some(temp_path),
            remaining: len,
            buf: vec![0u8; BACKUP_STREAM_CHUNK_BYTES],
        }
    }
}

impl Drop for TempFileStreamBody {
    fn drop(&mut self) {
        if let Some(path) = self.temp_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl hyper::body::Body for TempFileStreamBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let mut read_buf = tokio::io::ReadBuf::new(&mut this.buf);
        match Pin::new(&mut this.file).poll_read(cx, &mut read_buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled();
                if filled.is_empty() {
                    if let Some(path) = this.temp_path.take() {
                        let _ = std::fs::remove_file(path);
                    }
                    Poll::Ready(None)
                } else {
                    let chunk = Bytes::copy_from_slice(filled);
                    this.remaining = this.remaining.saturating_sub(chunk.len() as u64);
                    Poll::Ready(Some(Ok(Frame::data(chunk))))
                }
            }
            Poll::Ready(Err(err)) => {
                if let Some(path) = this.temp_path.take() {
                    let _ = std::fs::remove_file(path);
                }
                Poll::Ready(Some(Err(err)))
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

enum ResponseBody {
    Full(Full<Bytes>),
    File(TempFileStreamBody),
}

impl ResponseBody {
    fn full(bytes: Bytes) -> Self {
        Self::Full(Full::new(bytes))
    }

    fn empty() -> Self {
        Self::Full(Full::new(Bytes::new()))
    }

    fn temp_file(file: tokio::fs::File, path: PathBuf, len: u64) -> Self {
        Self::File(TempFileStreamBody::new(file, path, len))
    }
}

impl hyper::body::Body for ResponseBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            Self::Full(inner) => Pin::new(inner)
                .poll_frame(cx)
                .map(|opt| opt.map(|res| res.map_err(|never| match never {}))),
            Self::File(inner) => Pin::new(inner).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Full(inner) => inner.is_end_stream(),
            Self::File(_) => false,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Full(inner) => inner.size_hint(),
            Self::File(inner) => inner.size_hint(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedOrigin {
    pub origin: String,
    pub hosts: Vec<String>,
    pub is_https: bool,
}

/// Validates and normalizes an explicit remote dashboard origin (such as
/// `https://my-vps.tail1234.ts.net:18786` or `https://pockets.example.com`).
pub fn parse_trusted_origin(raw: &str) -> Result<TrustedOrigin, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let lower_full = trimmed.to_ascii_lowercase();
    let (scheme, rest) = if let Some(rest) = lower_full.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = lower_full.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(format!(
            "Remote URL '{raw}' must start with https:// or http://"
        ));
    };
    if rest.is_empty()
        || rest.contains('/')
        || rest.contains('?')
        || rest.contains('#')
        || rest.contains('@')
        || rest.bytes().any(|b| b <= 0x20 || b == 0x7f)
    {
        return Err(format!(
            "Remote URL '{raw}' must be a bare origin (scheme://host[:port]) without paths or credentials."
        ));
    }
    let lower = rest.to_ascii_lowercase();
    let (host, port) = if let Some((h, p)) = lower.rsplit_once(':') {
        if h.is_empty() || h.contains(':') {
            return Err(format!("Invalid host in remote URL '{raw}'."));
        }
        let port = p
            .parse::<u16>()
            .map_err(|_| format!("Invalid port in remote URL '{raw}'."))?;
        if port == 0 {
            return Err(format!("Port cannot be 0 in remote URL '{raw}'."));
        }
        (h.to_owned(), Some(port))
    } else {
        (lower, None)
    };
    if host.is_empty()
        || host.starts_with('.')
        || host.ends_with('.')
        || host.contains("..")
        || !host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'))
    {
        return Err(format!("Invalid hostname in remote URL '{raw}'."));
    }
    let is_https = scheme == "https";
    let default_port = if is_https { 443 } else { 80 };
    let effective_port = port.unwrap_or(default_port);
    let origin = if effective_port == default_port {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{effective_port}")
    };
    let mut hosts = vec![format!("{host}:{effective_port}")];
    if effective_port == default_port {
        hosts.push(host);
    }
    Ok(TrustedOrigin {
        origin,
        hosts,
        is_https,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonRecord {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
}

/// In-memory store of short-lived, single-use `/auth/bootstrap?code=...` codes
/// so sign-in URLs printed by `pockets open` / `pockets url` never expose the
/// long-lived `session.key` in browser history, Referer headers, or process args.
#[derive(Debug, Default)]
pub struct BootstrapCodes {
    pending: Mutex<Vec<(String, Instant)>>,
}

impl BootstrapCodes {
    pub fn issue(&self) -> String {
        self.issue_at(Instant::now())
    }

    pub(crate) fn issue_at(&self, now: Instant) -> String {
        let code = generate_session_token();
        let mut guard = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.retain(|(_, created)| now.saturating_duration_since(*created) < BOOTSTRAP_CODE_TTL);
        while guard.len() >= MAX_PENDING_BOOTSTRAP_CODES {
            guard.remove(0);
        }
        guard.push((code.clone(), now));
        code
    }

    pub fn consume(&self, candidate: &str) -> bool {
        self.consume_at(candidate, Instant::now())
    }

    pub(crate) fn consume_at(&self, candidate: &str, now: Instant) -> bool {
        if candidate.is_empty() {
            return false;
        }
        let mut guard = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.retain(|(_, created)| now.saturating_duration_since(*created) < BOOTSTRAP_CODE_TTL);
        let mut matched_idx = None;
        for (idx, (code, _)) in guard.iter().enumerate() {
            if constant_time_eq(code.as_bytes(), candidate.as_bytes()) {
                matched_idx = Some(idx);
            }
        }
        if let Some(idx) = matched_idx {
            guard.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn clear(&self) {
        let mut guard = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clear();
    }
}

#[derive(Debug, Clone)]
pub struct DaemonReady {
    pub port: u16,
    pub remote_url: Option<String>,
    bootstrap_codes: Arc<BootstrapCodes>,
}

impl DaemonReady {
    pub fn issue_bootstrap_code(&self) -> String {
        self.bootstrap_codes.issue()
    }
}

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub port: u16,
    pub dev_origin: Option<String>,
    pub public_url: Option<String>,
    pub enable_tailscale: bool,
    pub token: String,
    pub token_from_env: bool,
}

#[derive(Clone)]
pub struct ServerState {
    pub ctx: AppContext,
    pub port: u16,
    pub token: Arc<RwLock<String>>,
    pub token_from_env: bool,
    pub dev_origin: Option<String>,
    pub extra_origins: Arc<RwLock<Vec<TrustedOrigin>>>,
    pub bootstrap_codes: Arc<BootstrapCodes>,
}

impl ServerState {
    /// Returns the active session token, transparently picking up rotations
    /// written to `session.key` on disk and revoking any pending one-time
    /// bootstrap codes whenever the key changes.
    pub fn current_token(&self) -> String {
        if !self.token_from_env {
            if let Some(disk_token) = read_session_token_from_disk(&self.ctx) {
                let needs_update = self
                    .token
                    .read()
                    .map(|cur| !constant_time_eq(cur.as_bytes(), disk_token.as_bytes()))
                    .unwrap_or(true);
                if needs_update {
                    if let Ok(mut guard) = self.token.write() {
                        *guard = disk_token;
                    }
                    self.bootstrap_codes.clear();
                }
            }
        }
        self.token
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

pub fn generate_session_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub fn session_key_path(ctx: &AppContext) -> PathBuf {
    ctx.data_dir().join("session.key")
}

fn read_session_token_from_disk(ctx: &AppContext) -> Option<String> {
    let existing = std::fs::read_to_string(session_key_path(ctx)).ok()?;
    let trimmed = existing.trim();
    if trimmed.len() >= 32 {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

fn write_session_token(ctx: &AppContext, token: &str) -> Result<(), String> {
    std::fs::create_dir_all(ctx.data_dir())
        .map_err(|err| format!("Could not create Pockets data directory: {err}"))?;
    let path = session_key_path(ctx);
    let temp = ctx
        .data_dir()
        .join(format!(".session-{}.tmp", Uuid::new_v4().simple()));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|err| format!("Could not write Pockets session key: {err}"))?;
        file.write_all(token.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|err| {
                let _ = std::fs::remove_file(&temp);
                format!("Could not flush Pockets session key: {err}")
            })?;
    }
    std::fs::rename(&temp, &path).map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        format!("Could not save Pockets session key: {err}")
    })?;
    Ok(())
}

/// Loads a persistent per-machine 256-bit session token from `session.key`
/// (`0600` perms), creating one on first run so browsers and Dock PWAs stay
/// signed in across daemon restarts. Returns `(token, from_env)`.
pub fn load_or_create_session_token_with_source(ctx: &AppContext) -> Result<(String, bool), String> {
    if let Ok(env_token) = std::env::var("POCKETS_AUTH_TOKEN") {
        let trimmed = env_token.trim();
        if !trimmed.is_empty() {
            return Ok((trimmed.to_owned(), true));
        }
    }
    if let Some(existing) = read_session_token_from_disk(ctx) {
        return Ok((existing, false));
    }
    let token = generate_session_token();
    write_session_token(ctx, &token)?;
    Ok((token, false))
}

/// Loads a persistent per-machine 256-bit session token from `session.key`
/// (`0600` perms), creating one on first run so browsers and Dock PWAs stay
/// signed in across daemon restarts.
pub fn load_or_create_session_token(ctx: &AppContext) -> Result<String, String> {
    load_or_create_session_token_with_source(ctx).map(|(token, _)| token)
}

/// Generates and persists a fresh 256-bit session token in `session.key`
/// (`0600` perms), updating `daemon.json` if present. A running daemon detects
/// the rotated key on disk immediately and revokes all prior cookies and
/// pending bootstrap codes.
pub fn rotate_session_token(ctx: &AppContext) -> Result<String, String> {
    if let Ok(env_token) = std::env::var("POCKETS_AUTH_TOKEN") {
        if !env_token.trim().is_empty() {
            return Err(
                "Cannot rotate session token while POCKETS_AUTH_TOKEN is set in the environment. \
                 Unset POCKETS_AUTH_TOKEN and retry."
                    .to_owned(),
            );
        }
    }
    let token = generate_session_token();
    write_session_token(ctx, &token)?;
    if let Some(mut existing) = read_daemon_record(ctx) {
        existing.token = token.clone();
        let _ = write_daemon_record(ctx, &existing);
    }
    Ok(token)
}

pub fn daemon_record_path(ctx: &AppContext) -> PathBuf {
    ctx.data_dir().join("daemon.json")
}

pub fn read_daemon_record(ctx: &AppContext) -> Option<DaemonRecord> {
    let bytes = std::fs::read(daemon_record_path(ctx)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn write_daemon_record(ctx: &AppContext, record: &DaemonRecord) -> Result<(), String> {
    std::fs::create_dir_all(ctx.data_dir())
        .map_err(|err| format!("Could not create Pockets data directory: {err}"))?;
    let path = daemon_record_path(ctx);
    let temp = ctx
        .data_dir()
        .join(format!(".daemon-{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|err| format!("Could not serialize daemon metadata: {err}"))?;
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|err| format!("Could not write daemon metadata: {err}"))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|err| {
                let _ = std::fs::remove_file(&temp);
                format!("Could not flush daemon metadata: {err}")
            })?;
    }
    std::fs::rename(&temp, &path).map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        format!("Could not publish daemon metadata: {err}")
    })
}

pub fn remove_daemon_record(ctx: &AppContext, pid: u32) {
    if let Some(existing) = read_daemon_record(ctx) {
        if existing.pid == pid {
            let _ = std::fs::remove_file(daemon_record_path(ctx));
        }
    }
}

/// Checks whether a previously recorded daemon is still answering `/api/health`.
pub async fn probe_running_daemon(record: &DaemonRecord) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), record.port);
    let Ok(Ok(mut stream)) =
        tokio::time::timeout(std::time::Duration::from_millis(500), tokio::net::TcpStream::connect(addr)).await
    else {
        return false;
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let req = format!(
        "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        record.port
    );
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 256];
    match tokio::time::timeout(std::time::Duration::from_millis(500), stream.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            let text = String::from_utf8_lossy(&buf[..n]);
            text.starts_with("HTTP/1.1 200")
        }
        _ => false,
    }
}

/// Requests a short-lived, single-use `/auth/bootstrap?code=...` code from the
/// running local daemon over loopback using the session key from `session.key`.
pub async fn request_bootstrap_code(port: u16, session_token: &str) -> Result<String, String> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let mut stream = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| format!("Timed out connecting to Pockets daemon on 127.0.0.1:{port}."))?
    .map_err(|err| format!("Could not connect to Pockets daemon on 127.0.0.1:{port}: {err}"))?;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let req = format!(
        "POST /auth/bootstrap-code HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {session_token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|err| format!("Could not request sign-in code from Pockets daemon: {err}"))?;

    let mut response_bytes = Vec::new();
    let mut buf = [0u8; 1024];
    let read_result = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            response_bytes.extend_from_slice(&buf[..n]);
            if response_bytes.len() > 8192 {
                break;
            }
        }
        Ok::<(), std::io::Error>(())
    })
    .await
    .map_err(|_| "Timed out waiting for sign-in code from Pockets daemon.".to_owned())?;
    read_result.map_err(|err| format!("Could not read sign-in code response: {err}"))?;

    let text = String::from_utf8_lossy(&response_bytes);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return Err("Unexpected response from Pockets daemon while minting sign-in code.".to_owned());
    };
    let status_line = head.lines().next().unwrap_or("");
    if !status_line.starts_with("HTTP/1.1 200") {
        let detail = serde_json::from_str::<Value>(body.trim())
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| status_line.to_owned());
        return Err(format!("Could not mint sign-in link: {detail}"));
    }

    #[derive(Deserialize)]
    struct CodeResponse {
        code: String,
    }
    let parsed: CodeResponse = serde_json::from_str(body.trim())
        .map_err(|err| format!("Could not parse sign-in code from Pockets daemon: {err}"))?;
    if parsed.code.is_empty() {
        return Err("Pockets daemon returned an empty sign-in code.".to_owned());
    }
    Ok(parsed.code)
}

pub fn bootstrap_code_url(port: u16, code: &str) -> String {
    format!("http://pockets.localhost:{port}/auth/bootstrap?code={code}")
}

pub fn bootstrap_code_url_for(base_url: &str, code: &str) -> String {
    format!("{}/auth/bootstrap?code={code}", base_url.trim_end_matches('/'))
}

pub fn dashboard_url(port: u16) -> String {
    format!("http://pockets.localhost:{port}/")
}

pub fn dashboard_url_for(base_url: &str) -> String {
    format!("{}/", base_url.trim_end_matches('/'))
}

pub async fn run_daemon(
    ctx: AppContext,
    options: DaemonOptions,
    on_ready: Option<tokio::sync::oneshot::Sender<DaemonReady>>,
) -> Result<(), String> {
    let mut initial_origins = Vec::new();
    let mut primary_remote_url: Option<String> = None;

    if let Some(raw_public) = options.public_url.as_deref() {
        let parsed = parse_trusted_origin(raw_public)?;
        primary_remote_url = Some(parsed.origin.clone());
        initial_origins.push(parsed);
    }

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), options.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|err| format!("Could not bind Pockets daemon on {addr}: {err}"))?;
    let bound_port = listener
        .local_addr()
        .map_err(|err| format!("Could not read bound address: {err}"))?
        .port();

    let mut needs_tailscale_retry = false;
    if options.enable_tailscale {
        match tailscale::enable_port_serve(bound_port).await {
            Ok(ts_url) => {
                if let Ok(parsed) = parse_trusted_origin(&ts_url) {
                    if primary_remote_url.is_none() {
                        primary_remote_url = Some(parsed.origin.clone());
                    }
                    if !initial_origins.contains(&parsed) {
                        initial_origins.push(parsed);
                    }
                }
            }
            Err(err) => {
                eprintln!("warning: could not publish Pockets dashboard on Tailscale yet ({err})");
                needs_tailscale_retry = true;
            }
        }
    }

    let extra_origins = Arc::new(RwLock::new(initial_origins));
    let shared_token = Arc::new(RwLock::new(options.token.clone()));
    let bootstrap_codes = Arc::new(BootstrapCodes::default());
    let pid = std::process::id();
    write_daemon_record(
        &ctx,
        &DaemonRecord {
            pid,
            port: bound_port,
            token: options.token.clone(),
            remote_url: primary_remote_url.clone(),
        },
    )?;

    let housekeeping_ctx = ctx.clone();
    tokio::spawn(async move {
        housekeeping_ctx.startup_housekeeping().await;
    });

    // On VM boot, `tailscaled` can take a few seconds to come online after
    // `systemd --user` starts Pockets. Retry a few times cleanly in the background.
    if needs_tailscale_retry {
        let retry_ctx = ctx.clone();
        let retry_origins = Arc::clone(&extra_origins);
        let retry_token = Arc::clone(&shared_token);
        let retry_token_from_env = options.token_from_env;
        let existing_public = primary_remote_url.clone();
        tokio::spawn(async move {
            for delay_secs in [2u64, 4, 8, 15, 30] {
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                if let Ok(ts_url) = tailscale::enable_port_serve(bound_port).await {
                    if let Ok(parsed) = parse_trusted_origin(&ts_url) {
                        if let Ok(mut guard) = retry_origins.write() {
                            if !guard.contains(&parsed) {
                                guard.push(parsed.clone());
                            }
                        }
                        let current_token = if !retry_token_from_env {
                            read_session_token_from_disk(&retry_ctx).unwrap_or_else(|| {
                                retry_token
                                    .read()
                                    .map(|g| g.clone())
                                    .unwrap_or_default()
                            })
                        } else {
                            retry_token
                                .read()
                                .map(|g| g.clone())
                                .unwrap_or_default()
                        };
                        let remote_url = existing_public.clone().or(Some(parsed.origin));
                        let _ = write_daemon_record(
                            &retry_ctx,
                            &DaemonRecord {
                                pid,
                                port: bound_port,
                                token: current_token,
                                remote_url,
                            },
                        );
                        eprintln!("Pockets dashboard is now reachable on your tailnet at {ts_url}");
                    }
                    break;
                }
            }
        });
    }

    if let Some(tx) = on_ready {
        let _ = tx.send(DaemonReady {
            port: bound_port,
            remote_url: primary_remote_url,
            bootstrap_codes: Arc::clone(&bootstrap_codes),
        });
    }

    let reexec_args = build_reexec_args(
        ctx.data_dir(),
        bound_port,
        options.enable_tailscale,
        options.public_url.as_deref(),
        options.dev_origin.as_deref(),
    );

    let state = Arc::new(ServerState {
        ctx: ctx.clone(),
        port: bound_port,
        token: shared_token,
        token_from_env: options.token_from_env,
        dev_origin: options.dev_origin,
        extra_origins,
        bootstrap_codes,
    });

    let mut restart_rx = pockets_core::app_update::subscribe_restart();
    let mut restart_exe: Option<PathBuf> = None;

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, _) = match accept {
                    Ok(pair) => pair,
                    Err(err) => {
                        eprintln!("pockets daemon accept error: {err}");
                        continue;
                    }
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(handle_request(state, req).await) }
                    });
                    if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                        let msg = err.to_string();
                        if !msg.contains("connection closed before message completed") {
                            eprintln!("pockets daemon connection error: {msg}");
                        }
                    }
                });
            }
            changed = restart_rx.changed() => {
                if changed.is_ok() {
                    if let Some(exe) = restart_rx.borrow().clone() {
                        restart_exe = Some(exe);
                        break;
                    }
                }
            }
            _ = shutdown_signal() => {
                break;
            }
        }
    }

    drop(listener);
    ctx.tailscale_portal.stop().await;

    if let Some(exe) = restart_exe {
        reexec_updated_daemon(&exe, &reexec_args);
    }

    remove_daemon_record(&ctx, pid);
    Ok(())
}

fn build_reexec_args(
    data_dir: &std::path::Path,
    port: u16,
    enable_tailscale: bool,
    public_url: Option<&str>,
    dev_origin: Option<&str>,
) -> Vec<std::ffi::OsString> {
    let mut args = vec![
        std::ffi::OsString::from("--data-dir"),
        data_dir.as_os_str().to_os_string(),
        std::ffi::OsString::from("serve"),
        std::ffi::OsString::from("--port"),
        std::ffi::OsString::from(port.to_string()),
        std::ffi::OsString::from("--no-bootstrap"),
    ];
    if enable_tailscale {
        args.push(std::ffi::OsString::from("--tailscale"));
    }
    if let Some(url) = public_url {
        args.push(std::ffi::OsString::from("--public-url"));
        args.push(std::ffi::OsString::from(url));
    }
    if let Some(origin) = dev_origin {
        args.push(std::ffi::OsString::from("--dev-origin"));
        args.push(std::ffi::OsString::from(origin));
    }
    args
}

/// Replaces the current daemon process in place with the newly installed
/// `pockets` binary so `launchd`, `systemd`, and foreground terminals keep
/// running the updated version under the same PID without dropping the site.
fn reexec_updated_daemon(exe: &std::path::Path, args: &[std::ffi::OsString]) {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(exe).args(args).exec();
    eprintln!(
        "pockets self-update: could not exec {}: {err}",
        exe.display()
    );
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn is_loopback_host(host: &str, port: u16) -> bool {
    let lower = host.trim().to_ascii_lowercase();
    let expected = [
        format!("pockets.localhost:{port}"),
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
    ];
    expected.iter().any(|candidate| candidate == &lower)
}

fn is_dev_host(host: &str, dev_origin: Option<&str>) -> bool {
    let Some(dev) = dev_origin else {
        return false;
    };
    let Ok(trusted) = parse_trusted_origin(dev) else {
        return false;
    };
    let lower = host.trim().to_ascii_lowercase();
    trusted.hosts.iter().any(|candidate| candidate == &lower)
}

fn allowed_host(host: &str, port: u16, dev_origin: Option<&str>, extra: &[TrustedOrigin]) -> bool {
    if is_loopback_host(host, port) || is_dev_host(host, dev_origin) {
        return true;
    }
    let lower = host.trim().to_ascii_lowercase();
    extra
        .iter()
        .any(|origin| origin.hosts.iter().any(|candidate| candidate == &lower))
}

fn allowed_origin(
    origin: &str,
    port: u16,
    dev_origin: Option<&str>,
    extra: &[TrustedOrigin],
) -> bool {
    let lower = origin.trim().to_ascii_lowercase();
    let expected = [
        format!("http://pockets.localhost:{port}"),
        format!("http://localhost:{port}"),
        format!("http://127.0.0.1:{port}"),
    ];
    if expected.iter().any(|candidate| candidate == &lower) {
        return true;
    }
    if extra.iter().any(|trusted| trusted.origin == lower) {
        return true;
    }
    if let Some(dev) = dev_origin {
        if lower == dev.trim().trim_end_matches('/').to_ascii_lowercase() {
            return true;
        }
    }
    false
}

fn is_https_request(headers: &HeaderMap, host: &str, extra: &[TrustedOrigin]) -> bool {
    if let Some(proto) = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
    {
        if proto
            .split(',')
            .next()
            .is_some_and(|first| first.trim().eq_ignore_ascii_case("https"))
        {
            return true;
        }
    }
    let lower = host.trim().to_ascii_lowercase();
    extra
        .iter()
        .any(|trusted| trusted.is_https && trusted.hosts.iter().any(|h| h == &lower))
}

/// Builds a host-only `Set-Cookie` header (no `Domain` attribute). Because
/// browsers still share host-only cookies across ports on the same hostname,
/// `pockets_core::local_sites` reserves `pockets.localhost` exclusively for
/// the dashboard so managed Pockets can never share this host.
fn session_cookie_header(token: &str, secure: bool) -> String {
    if secure {
        format!(
            "{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Secure; Path=/; Max-Age=31536000"
        )
    } else {
        format!("{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000")
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn is_authenticated(headers: &HeaderMap, expected_token: &str) -> bool {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|val| val.to_str().ok())
    {
        if let Some(bearer) = auth.strip_prefix("Bearer ") {
            if constant_time_eq(bearer.trim().as_bytes(), expected_token.as_bytes()) {
                return true;
            }
        }
    }
    if let Some(cookie_header) = headers.get(header::COOKIE).and_then(|val| val.to_str().ok()) {
        for part in cookie_header.split(';') {
            if let Some((name, val)) = part.trim().split_once('=') {
                if name.trim() == COOKIE_NAME
                    && constant_time_eq(val.trim().as_bytes(), expected_token.as_bytes())
                {
                    return true;
                }
            }
        }
    }
    false
}

fn percent_decode_query_value(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(val) = u8::from_str_radix(hex, 16) {
                    out.push(val);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn favicon_response(asset: Option<pockets_core::favicon::FaviconAsset>) -> Response<ResponseBody> {
    match asset {
        Some(asset) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, asset.content_type)
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .header(
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'",
            )
            .header(header::CACHE_CONTROL, "private, max-age=300")
            .body(ResponseBody::full(Bytes::from(asset.bytes)))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CACHE_CONTROL, "no-store")
            .body(ResponseBody::empty())
            .unwrap(),
    }
}

fn extract_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_owned());
            }
        }
    }
    None
}

async fn handle_request(
    state: Arc<ServerState>,
    req: Request<Incoming>,
) -> Response<ResponseBody> {
    let extra_origins = state
        .extra_origins
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    let request_host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let Some(host) = request_host.as_deref() else {
        return json_response(
            StatusCode::FORBIDDEN,
            json!({ "error": "Refused request with disallowed Host header." }),
        );
    };
    if !allowed_host(host, state.port, state.dev_origin.as_deref(), &extra_origins) {
        return json_response(
            StatusCode::FORBIDDEN,
            json!({ "error": "Refused request with disallowed Host header." }),
        );
    }
    let remote_client =
        !is_loopback_host(host, state.port) && !is_dev_host(host, state.dev_origin.as_deref());
    let secure_cookie = is_https_request(req.headers(), host, &extra_origins);

    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let query = req.uri().query().unwrap_or("").to_owned();

    // CORS preflight only for explicit --dev-origin during local Vite dev.
    if method == Method::OPTIONS {
        if let (Some(dev), Some(origin)) = (
            state.dev_origin.as_deref(),
            req.headers()
                .get(header::ORIGIN)
                .and_then(|v| v.to_str().ok()),
        ) {
            if allowed_origin(origin, state.port, Some(dev), &extra_origins) {
                return Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
                    .header(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true")
                    .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
                    .header(
                        header::ACCESS_CONTROL_ALLOW_HEADERS,
                        "Content-Type, Authorization, X-Backup-Filename",
                    )
                    .body(ResponseBody::empty())
                    .unwrap();
            }
        }
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(ResponseBody::empty())
            .unwrap();
    }

    let request_origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    if method == Method::POST {
        if let Some(origin) = request_origin.as_deref() {
            if !allowed_origin(
                origin,
                state.port,
                state.dev_origin.as_deref(),
                &extra_origins,
            ) {
                return json_response(
                    StatusCode::FORBIDDEN,
                    json!({ "error": "Refused cross-origin state change." }),
                );
            }
        }
    }

    let mut response = match (&method, path.as_str()) {
        (&Method::GET, "/api/health") => json_response(
            StatusCode::OK,
            json!({ "ok": true, "version": pockets_core::APP_VERSION }),
        ),

        (&Method::POST, "/auth/bootstrap-code") => {
            if remote_client {
                return json_response(
                    StatusCode::FORBIDDEN,
                    json!({ "error": "Bootstrap codes can only be minted over loopback." }),
                );
            }
            let current_token = state.current_token();
            if !is_authenticated(req.headers(), &current_token) {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({ "error": "Authentication required." }),
                );
            }
            let code = state.bootstrap_codes.issue();
            json_response(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "code": code,
                    "expiresInSeconds": BOOTSTRAP_CODE_TTL.as_secs(),
                }),
            )
        }

        (&Method::GET, "/auth/bootstrap") => {
            let current_token = state.current_token();
            let provided = extract_query_param(&query, "code").unwrap_or_default();
            if !state.bootstrap_codes.consume(&provided) {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({
                        "error": "This Pockets sign-in link is invalid, expired, or was already used. Run `pockets open` or `pockets url` for a fresh link."
                    }),
                );
            }
            let cookie = session_cookie_header(&current_token, secure_cookie);
            Response::builder()
                .status(StatusCode::FOUND)
                .header(header::SET_COOKIE, cookie)
                .header(header::LOCATION, "/")
                .header(header::CACHE_CONTROL, "no-store")
                .body(ResponseBody::empty())
                .unwrap()
        }

        (&Method::POST, "/auth/session") => {
            let body_bytes = match read_bounded_body(req.into_body(), 4096).await {
                Ok(bytes) => bytes,
                Err(msg) => {
                    return json_response(StatusCode::PAYLOAD_TOO_LARGE, json!({ "error": msg }));
                }
            };
            #[derive(Deserialize)]
            struct TokenBody {
                token: String,
            }
            let parsed: TokenBody = match serde_json::from_slice(&body_bytes) {
                Ok(v) => v,
                Err(_) => {
                    return json_response(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": "Enter your Pockets sign-in URL or session token." }),
                    );
                }
            };
            let current_token = state.current_token();
            // Accept a pasted one-time bootstrap URL (`?code=...`), a raw one-time
            // code, or the session token itself.
            let raw = parsed.token.trim();
            let valid = if let Some((_, rest)) = raw.split_once("code=") {
                let code = rest.split(&['&', '#'][..]).next().unwrap_or(rest).trim();
                state.bootstrap_codes.consume(code)
            } else if let Some((_, rest)) = raw.split_once("token=") {
                let candidate = rest.split(&['&', '#'][..]).next().unwrap_or(rest).trim();
                constant_time_eq(candidate.as_bytes(), current_token.as_bytes())
            } else {
                constant_time_eq(raw.as_bytes(), current_token.as_bytes())
                    || state.bootstrap_codes.consume(raw)
            };
            if !valid {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({
                        "error": "That sign-in link or token is invalid, expired, or already used. Run `pockets url` for a fresh link."
                    }),
                );
            }
            let cookie = session_cookie_header(&current_token, secure_cookie);
            let mut resp = json_response(StatusCode::OK, json!({ "ok": true }));
            if let Ok(val) = cookie.parse() {
                resp.headers_mut().insert(header::SET_COOKIE, val);
            }
            resp
        }

        (&Method::POST, p) if p.starts_with("/api/rpc/") => {
            let current_token = state.current_token();
            if !is_authenticated(req.headers(), &current_token) {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({
                        "error": "Your Pockets session is not authenticated. Open Pockets with `pockets` to refresh your session."
                    }),
                );
            }
            let command = p.trim_start_matches("/api/rpc/").to_owned();
            let body_bytes = match read_bounded_body(req.into_body(), MAX_RPC_BODY_BYTES).await {
                Ok(bytes) => bytes,
                Err(msg) => {
                    return json_response(StatusCode::PAYLOAD_TOO_LARGE, json!({ "error": msg }));
                }
            };
            let body: Value = if body_bytes.is_empty() {
                json!({})
            } else {
                match serde_json::from_slice(&body_bytes) {
                    Ok(value) => value,
                    Err(err) => {
                        return json_response(
                            StatusCode::BAD_REQUEST,
                            json!({ "error": format!("Invalid JSON body: {err}") }),
                        );
                    }
                }
            };
            match rpc::dispatch(&state.ctx, &command, body, remote_client).await {
                Ok(result) => json_response(StatusCode::OK, json!({ "ok": true, "result": result })),
                Err(error) => {
                    json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": error }))
                }
            }
        }

        (&Method::GET, "/api/hub/favicon") => {
            let current_token = state.current_token();
            if !is_authenticated(req.headers(), &current_token) {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({ "error": "Authentication required." }),
                );
            }
            let Some(raw_repo) = extract_query_param(&query, "repo") else {
                return favicon_response(None);
            };
            let repo = percent_decode_query_value(&raw_repo);
            let asset = pockets_core::favicon::resolve_github_repo_favicon(&repo).await;
            favicon_response(asset)
        }

        (&Method::GET, p) if p.starts_with("/api/projects/") && p.ends_with("/favicon") => {
            let current_token = state.current_token();
            if !is_authenticated(req.headers(), &current_token) {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({ "error": "Authentication required." }),
                );
            }
            let id = p
                .trim_start_matches("/api/projects/")
                .trim_end_matches("/favicon")
                .to_owned();
            let asset = pockets_core::favicon::resolve_project_favicon(&state.ctx, &id).await;
            favicon_response(asset)
        }

        (&Method::GET, p) if p.starts_with("/api/projects/") && p.ends_with("/backup") => {
            let current_token = state.current_token();
            if !is_authenticated(req.headers(), &current_token) {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({ "error": "Authentication required." }),
                );
            }
            let id = p
                .trim_start_matches("/api/projects/")
                .trim_end_matches("/backup")
                .to_owned();
            handle_backup_download(&state.ctx, id).await
        }

        (&Method::POST, p) if p.starts_with("/api/projects/") && p.ends_with("/backup") => {
            let current_token = state.current_token();
            if !is_authenticated(req.headers(), &current_token) {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({ "error": "Authentication required." }),
                );
            }
            let id = p
                .trim_start_matches("/api/projects/")
                .trim_end_matches("/backup")
                .to_owned();
            let display_name = req
                .headers()
                .get("x-backup-filename")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            handle_backup_upload(&state.ctx, id, display_name, req.into_body()).await
        }

        (&Method::GET, p) if !p.starts_with("/api/") => serve_static_asset(p),

        _ => json_response(
            StatusCode::NOT_FOUND,
            json!({ "error": "Route not found." }),
        ),
    };

    // Attach CORS headers only when an explicitly configured dev origin matches.
    if let (Some(dev), Some(origin)) = (state.dev_origin.as_deref(), request_origin.as_deref()) {
        if allowed_origin(origin, state.port, Some(dev), &extra_origins) {
            if let Ok(val) = origin.parse() {
                response
                    .headers_mut()
                    .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, val);
                response.headers_mut().insert(
                    header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                    header::HeaderValue::from_static("true"),
                );
            }
        }
    }

    response
}

#[derive(Debug)]
enum UploadStreamError {
    Empty,
    TooLarge(String),
    BadRequest(String),
    Internal(String),
}

async fn create_owner_only_temp_file(
    path: &std::path::Path,
) -> Result<tokio::fs::File, std::io::Error> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create_new(true).write(true).mode(0o600);
    opts.open(path).await
}

async fn stream_body_to_temp_file<B>(
    mut body: B,
    temp_path: &std::path::Path,
    max_bytes: u64,
) -> Result<u64, UploadStreamError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    use tokio::io::AsyncWriteExt;

    let mut file = create_owner_only_temp_file(temp_path)
        .await
        .map_err(|err| UploadStreamError::Internal(format!("Could not stage backup archive: {err}")))?;

    let write_result: Result<u64, UploadStreamError> = async {
        let mut written: u64 = 0;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|err| {
                UploadStreamError::BadRequest(format!("Could not read request body: {err}"))
            })?;
            if let Some(chunk) = frame.data_ref() {
                if chunk.is_empty() {
                    continue;
                }
                let next = written.saturating_add(chunk.len() as u64);
                if next > max_bytes {
                    return Err(UploadStreamError::TooLarge(format!(
                        "Backup archive exceeds maximum supported size of {max_bytes} bytes."
                    )));
                }
                file.write_all(chunk).await.map_err(|err| {
                    UploadStreamError::Internal(format!("Could not stage backup archive: {err}"))
                })?;
                written = next;
            }
        }
        if written == 0 {
            return Err(UploadStreamError::Empty);
        }
        file.flush().await.map_err(|err| {
            UploadStreamError::Internal(format!("Could not flush staged backup archive: {err}"))
        })?;
        file.sync_all().await.map_err(|err| {
            UploadStreamError::Internal(format!("Could not sync staged backup archive: {err}"))
        })?;
        Ok(written)
    }
    .await;

    drop(file);
    if write_result.is_err() {
        let _ = tokio::fs::remove_file(temp_path).await;
    }
    write_result
}

async fn handle_backup_download(ctx: &AppContext, id: String) -> Response<ResponseBody> {
    let temp = std::env::temp_dir().join(format!("pockets-export-{}.tar", Uuid::new_v4().simple()));
    let file_name = match project_lifecycle::export_project_data_to_path(ctx, id, &temp).await {
        Ok(name) => name,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp).await;
            return json_response(StatusCode::BAD_REQUEST, json!({ "error": error }));
        }
    };
    let file = match tokio::fs::File::open(&temp).await {
        Ok(file) => file,
        Err(err) => {
            let _ = tokio::fs::remove_file(&temp).await;
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("Could not open exported backup: {err}") }),
            );
        }
    };
    let len = match file.metadata().await {
        Ok(meta) => meta.len(),
        Err(err) => {
            drop(file);
            let _ = tokio::fs::remove_file(&temp).await;
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("Could not inspect exported backup: {err}") }),
            );
        }
    };
    let safe_name: String = file_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    let disposition = format!(
        "attachment; filename=\"{}\"",
        if safe_name.is_empty() {
            "pocket-backup.tar"
        } else {
            &safe_name
        }
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-tar")
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(header::CACHE_CONTROL, "no-store")
        .body(ResponseBody::temp_file(file, temp, len))
        .unwrap()
}

async fn handle_backup_upload(
    ctx: &AppContext,
    id: String,
    display_name: Option<String>,
    body: Incoming,
) -> Response<ResponseBody> {
    let temp = std::env::temp_dir().join(format!("pockets-import-{}.tar", Uuid::new_v4().simple()));
    if let Err(err) = stream_body_to_temp_file(body, &temp, MAX_BACKUP_ARCHIVE_BYTES).await {
        return match err {
            UploadStreamError::Empty => json_response(
                StatusCode::BAD_REQUEST,
                json!({ "error": "The uploaded backup archive is empty." }),
            ),
            UploadStreamError::TooLarge(error) => {
                json_response(StatusCode::PAYLOAD_TOO_LARGE, json!({ "error": error }))
            }
            UploadStreamError::BadRequest(error) => {
                json_response(StatusCode::BAD_REQUEST, json!({ "error": error }))
            }
            UploadStreamError::Internal(error) => {
                json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error }))
            }
        };
    }
    let outcome =
        project_lifecycle::restore_project_data_from_path(ctx, id, &temp, display_name).await;
    let _ = tokio::fs::remove_file(&temp).await;
    match outcome {
        Ok(result) => json_response(StatusCode::OK, json!({ "ok": true, "result": result })),
        Err(error) => json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": error })),
    }
}

async fn read_bounded_body(mut body: Incoming, max_bytes: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|err| format!("Could not read request body: {err}"))?;
        if let Some(chunk) = frame.data_ref() {
            if out.len().saturating_add(chunk.len()) > max_bytes {
                return Err(format!("Request body exceeds maximum size of {max_bytes} bytes."));
            }
            out.extend_from_slice(chunk);
        }
    }
    Ok(out)
}

fn serve_static_asset(request_path: &str) -> Response<ResponseBody> {
    let trimmed = request_path.trim_start_matches('/');
    let lookup = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    let asset = WEB_ASSETS
        .iter()
        .find(|(path, _)| *path == lookup)
        .or_else(|| {
            // SPA fallback for client-side routes without file extensions.
            if !lookup.contains('.') {
                WEB_ASSETS.iter().find(|(path, _)| *path == "index.html")
            } else {
                None
            }
        });

    if let Some((path, bytes)) = asset {
        let content_type = mime_for_path(path);
        let cache = if *path == "index.html" {
            "no-cache"
        } else {
            "public, max-age=31536000, immutable"
        };
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, cache)
            .body(ResponseBody::full(Bytes::from_static(bytes)))
            .unwrap()
    } else if WEB_ASSETS.is_empty() {
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(ResponseBody::full(Bytes::from_static(
                b"Pockets web UI assets were not bundled in this build. Run `npm run build:web` before `cargo build`.",
            )))
            .unwrap()
    } else {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(ResponseBody::full(Bytes::from_static(b"Not found")))
            .unwrap()
    }
}

fn mime_for_path(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".webmanifest") || path.ends_with(".json") {
        "application/manifest+json; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

fn json_response(status: StatusCode, value: Value) -> Response<ResponseBody> {
    let bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(ResponseBody::full(Bytes::from(bytes)))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_guard_allows_loopback_and_explicit_trusted_origins_only() {
        let none: Vec<TrustedOrigin> = Vec::new();
        assert!(allowed_host("pockets.localhost:18786", 18786, None, &none));
        assert!(allowed_host("127.0.0.1:18786", 18786, None, &none));
        assert!(allowed_host("localhost:18786", 18786, None, &none));
        assert!(!allowed_host("evil.com:18786", 18786, None, &none));
        assert!(!allowed_host("notes.localhost:18786", 18786, None, &none));
        assert!(!allowed_host("pockets.localhost:18788", 18786, None, &none));
        assert!(!allowed_host("localhost:1420", 18786, None, &none));
        assert!(allowed_host(
            "localhost:1420",
            18786,
            Some("http://localhost:1420"),
            &none
        ));

        let ts = parse_trusted_origin("https://cloud-vm.tail1234.ts.net:18786").unwrap();
        let custom = parse_trusted_origin("https://pockets.example.com/").unwrap();
        let extra = vec![ts, custom];
        assert!(allowed_host(
            "cloud-vm.tail1234.ts.net:18786",
            18786,
            None,
            &extra
        ));
        assert!(!allowed_host(
            "cloud-vm.tail1234.ts.net:18788",
            18786,
            None,
            &extra
        ));
        assert!(allowed_host("pockets.example.com", 18786, None, &extra));
        assert!(allowed_host("pockets.example.com:443", 18786, None, &extra));
        assert!(!allowed_host("other.example.com", 18786, None, &extra));
    }

    #[test]
    fn origin_guard_blocks_other_pockets_and_external_sites() {
        let none: Vec<TrustedOrigin> = Vec::new();
        assert!(allowed_origin("http://pockets.localhost:18786", 18786, None, &none));
        assert!(allowed_origin("http://127.0.0.1:18786", 18786, None, &none));
        assert!(!allowed_origin("http://notes.localhost:18788", 18786, None, &none));
        assert!(!allowed_origin("https://evil.example", 18786, None, &none));
        assert!(allowed_origin(
            "http://localhost:1420",
            18786,
            Some("http://localhost:1420"),
            &none
        ));

        let ts = parse_trusted_origin("https://cloud-vm.tail1234.ts.net:18786").unwrap();
        let extra = vec![ts];
        assert!(allowed_origin(
            "https://cloud-vm.tail1234.ts.net:18786",
            18786,
            None,
            &extra
        ));
        assert!(!allowed_origin(
            "https://cloud-vm.tail1234.ts.net:18788",
            18786,
            None,
            &extra
        ));
    }

    #[test]
    fn parse_trusted_origin_rejects_paths_credentials_and_bad_schemes() {
        assert!(parse_trusted_origin("ftp://example.com").is_err());
        assert!(parse_trusted_origin("https://example.com/path").is_err());
        assert!(parse_trusted_origin("https://user@example.com").is_err());
        assert!(parse_trusted_origin("https://example.com:0").is_err());

        let parsed = parse_trusted_origin("HTTPS://My-VPS.Tail1234.ts.net:18786/").unwrap();
        assert_eq!(parsed.origin, "https://my-vps.tail1234.ts.net:18786");
        assert!(parsed.is_https);
        assert_eq!(parsed.hosts, vec!["my-vps.tail1234.ts.net:18786"]);
    }

    #[test]
    fn secure_cookie_flag_applies_to_https_remote_requests() {
        let ts = parse_trusted_origin("https://cloud-vm.tail1234.ts.net:18786").unwrap();
        let extra = vec![ts];
        let headers = HeaderMap::new();
        assert!(!is_https_request(&headers, "pockets.localhost:18786", &extra));
        assert!(is_https_request(
            &headers,
            "cloud-vm.tail1234.ts.net:18786",
            &extra
        ));
        assert!(session_cookie_header("abc", true).contains("; Secure;"));
        assert!(!session_cookie_header("abc", false).contains("; Secure;"));
    }

    #[test]
    fn reexec_args_preserve_data_dir_port_and_remote_flags() {
        let args = build_reexec_args(
            std::path::Path::new("/tmp/pockets-data"),
            18786,
            true,
            Some("https://pockets.example.com"),
            None,
        );
        let strs: Vec<String> = args
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            strs,
            vec![
                "--data-dir",
                "/tmp/pockets-data",
                "serve",
                "--port",
                "18786",
                "--no-bootstrap",
                "--tailscale",
                "--public-url",
                "https://pockets.example.com",
            ]
        );
    }

    #[test]
    fn authentication_accepts_matching_cookie_or_bearer_only() {
        let token = generate_session_token();
        assert_eq!(token.len(), 64);

        let mut headers = HeaderMap::new();
        assert!(!is_authenticated(&headers, &token));

        headers.insert(
            header::COOKIE,
            format!("other=1; pockets_session={token}").parse().unwrap(),
        );
        assert!(is_authenticated(&headers, &token));

        let mut bearer_headers = HeaderMap::new();
        bearer_headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(is_authenticated(&bearer_headers, &token));

        let mut wrong = HeaderMap::new();
        wrong.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!is_authenticated(&wrong, &token));
    }

    #[test]
    fn bootstrap_codes_are_single_use_and_expire() {
        let codes = BootstrapCodes::default();
        let start = Instant::now();
        let code = codes.issue_at(start);
        assert_eq!(code.len(), 64);

        // Consuming once succeeds; replaying fails.
        assert!(codes.consume_at(&code, start + Duration::from_secs(10)));
        assert!(!codes.consume_at(&code, start + Duration::from_secs(11)));

        // Expired codes are rejected.
        let expiring = codes.issue_at(start);
        assert!(!codes.consume_at(&expiring, start + BOOTSTRAP_CODE_TTL));

        // Clearing revokes all pending codes.
        let pending = codes.issue_at(start);
        codes.clear();
        assert!(!codes.consume_at(&pending, start + Duration::from_secs(1)));
    }

    #[tokio::test]
    async fn bootstrap_endpoint_uses_one_time_codes_and_rotation_revokes_sessions() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn raw_http(port: u16, request: &str) -> String {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect test daemon");
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        }

        let temp = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(temp.path().to_path_buf());
        let initial_token = load_or_create_session_token(&ctx).unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel();
        let daemon_ctx = ctx.clone();
        let daemon_token = initial_token.clone();
        let handle = tokio::spawn(async move {
            let _ = run_daemon(
                daemon_ctx,
                DaemonOptions {
                    port: 0,
                    dev_origin: None,
                    public_url: None,
                    enable_tailscale: false,
                    token: daemon_token,
                    token_from_env: false,
                },
                Some(tx),
            )
            .await;
        });

        let ready = rx.await.expect("daemon should become ready");
        let port = ready.port;

        // 1. Old permanent ?token= bootstrap URLs are rejected.
        let legacy_resp = raw_http(
            port,
            &format!(
                "GET /auth/bootstrap?token={initial_token} HTTP/1.1\r\nHost: pockets.localhost:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(
            legacy_resp.starts_with("HTTP/1.1 401"),
            "permanent ?token= URL must be rejected: {legacy_resp}"
        );

        // 2. Minting a one-time code via loopback API succeeds with the session key.
        let code = request_bootstrap_code(port, &initial_token)
            .await
            .expect("mint bootstrap code");

        // 3. First use of ?code= sets the session cookie and redirects to /.
        let boot_resp = raw_http(
            port,
            &format!(
                "GET /auth/bootstrap?code={code} HTTP/1.1\r\nHost: pockets.localhost:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(
            boot_resp.starts_with("HTTP/1.1 302"),
            "first bootstrap must succeed: {boot_resp}"
        );
        assert!(
            boot_resp.contains(&format!("pockets_session={initial_token}")),
            "bootstrap must set session cookie: {boot_resp}"
        );

        // 4. Replaying the same ?code= is rejected.
        let replay_resp = raw_http(
            port,
            &format!(
                "GET /auth/bootstrap?code={code} HTTP/1.1\r\nHost: pockets.localhost:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(
            replay_resp.starts_with("HTTP/1.1 401"),
            "replayed code must be rejected: {replay_resp}"
        );

        // 5. Rotating session.key immediately revokes old cookies and pre-rotation codes.
        let pre_rotation_code = request_bootstrap_code(port, &initial_token).await.unwrap();
        let rotated_token = rotate_session_token(&ctx).unwrap();
        assert_ne!(rotated_token, initial_token);

        let stale_code_resp = raw_http(
            port,
            &format!(
                "GET /auth/bootstrap?code={pre_rotation_code} HTTP/1.1\r\nHost: pockets.localhost:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(
            stale_code_resp.starts_with("HTTP/1.1 401"),
            "pre-rotation code must be revoked: {stale_code_resp}"
        );

        let stale_cookie_resp = raw_http(
            port,
            &format!(
                "POST /api/rpc/get_manifest HTTP/1.1\r\nHost: pockets.localhost:{port}\r\nCookie: pockets_session={initial_token}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        )
        .await;
        assert!(
            stale_cookie_resp.starts_with("HTTP/1.1 401"),
            "old session cookie must be rejected after rotation: {stale_cookie_resp}"
        );

        // 6. New token works immediately without restarting the daemon.
        let new_code = request_bootstrap_code(port, &rotated_token)
            .await
            .expect("rotated token should mint a new bootstrap code");
        let fresh_boot_resp = raw_http(
            port,
            &format!(
                "GET /auth/bootstrap?code={new_code} HTTP/1.1\r\nHost: pockets.localhost:{port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(
            fresh_boot_resp.starts_with("HTTP/1.1 302"),
            "fresh bootstrap code must succeed: {fresh_boot_resp}"
        );
        assert!(
            fresh_boot_resp.contains(&format!("pockets_session={rotated_token}")),
            "cookie must use rotated token: {fresh_boot_resp}"
        );

        handle.abort();
    }

    struct ChunkedTestBody {
        chunks: std::collections::VecDeque<Bytes>,
    }

    impl hyper::body::Body for ChunkedTestBody {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            Poll::Ready(self.chunks.pop_front().map(|chunk| Ok(Frame::data(chunk))))
        }
    }

    #[tokio::test]
    async fn streaming_backup_upload_enforces_limit_owner_only_permissions_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged-ok.tar");

        // 1. Multi-chunk upload streams to disk with 0600 permissions.
        let part_a = vec![b'A'; 70 * 1024];
        let part_b = vec![b'B'; 80 * 1024];
        let mut expected = Vec::with_capacity(part_a.len() + part_b.len());
        expected.extend_from_slice(&part_a);
        expected.extend_from_slice(&part_b);

        let body = ChunkedTestBody {
            chunks: vec![Bytes::from(part_a), Bytes::from(part_b)].into(),
        };
        let written = stream_body_to_temp_file(body, &staged, MAX_BACKUP_ARCHIVE_BYTES)
            .await
            .expect("valid upload should stage cleanly");
        assert_eq!(written, expected.len() as u64);
        assert_eq!(std::fs::read(&staged).unwrap(), expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "staged upload must be owner-only 0600");
        }
        let _ = std::fs::remove_file(&staged);

        // 2. Empty upload fails with Empty and removes temp file.
        let empty_path = dir.path().join("staged-empty.tar");
        let empty_body = ChunkedTestBody {
            chunks: std::collections::VecDeque::new(),
        };
        assert!(matches!(
            stream_body_to_temp_file(empty_body, &empty_path, 1024).await,
            Err(UploadStreamError::Empty)
        ));
        assert!(!empty_path.exists());

        // 3. Oversized upload fails with TooLarge and removes temp file.
        let oversized_path = dir.path().join("staged-oversized.tar");
        let oversized_body = ChunkedTestBody {
            chunks: vec![
                Bytes::from(vec![1u8; 64 * 1024]),
                Bytes::from(vec![2u8; 64 * 1024]),
                Bytes::from(vec![3u8; 1024]),
            ]
            .into(),
        };
        assert!(matches!(
            stream_body_to_temp_file(oversized_body, &oversized_path, 128 * 1024).await,
            Err(UploadStreamError::TooLarge(_))
        ));
        assert!(!oversized_path.exists());

        // 4. Unified archive limit exceeds the old 512 MiB cap.
        assert!(MAX_BACKUP_ARCHIVE_BYTES > 512 * 1024 * 1024);
    }

    #[tokio::test]
    async fn streaming_backup_download_emits_multiple_chunks_and_cleans_up_temp_file() {
        use hyper::body::Body;

        let dir = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0..(200 * 1024)).map(|i| (i % 251) as u8).collect();

        // 1. Full stream emits multiple frames and deletes the temp file on completion.
        let temp_complete = dir.path().join("export-complete.tar");
        tokio::fs::write(&temp_complete, &payload).await.unwrap();
        let file = tokio::fs::File::open(&temp_complete).await.unwrap();
        let mut body = ResponseBody::temp_file(file, temp_complete.clone(), payload.len() as u64);
        assert_eq!(body.size_hint().exact(), Some(payload.len() as u64));

        let mut received = Vec::new();
        let mut frame_count = 0usize;
        while let Some(frame) = body.frame().await {
            let chunk = frame.unwrap().into_data().unwrap();
            frame_count += 1;
            received.extend_from_slice(&chunk);
        }
        assert!(
            frame_count >= 3,
            "200 KiB file should stream across multiple 64 KiB chunks, got {frame_count}"
        );
        assert_eq!(received, payload);
        assert!(
            !temp_complete.exists(),
            "temp export file must be removed after stream completes"
        );

        // 2. Dropping the stream mid-transfer (client disconnect) also deletes the temp file.
        let temp_aborted = dir.path().join("export-aborted.tar");
        tokio::fs::write(&temp_aborted, &payload).await.unwrap();
        let file = tokio::fs::File::open(&temp_aborted).await.unwrap();
        let mut body = ResponseBody::temp_file(file, temp_aborted.clone(), payload.len() as u64);
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(!first.is_empty());
        drop(body);
        assert!(
            !temp_aborted.exists(),
            "temp export file must be removed even if client disconnects early"
        );
    }
}
