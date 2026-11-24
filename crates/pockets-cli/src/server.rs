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
use std::sync::Arc;

use bytes::Bytes;
use http::{header, HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use uuid::Uuid;

use pockets_core::{project_lifecycle, AppContext};

use crate::rpc;

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

pub const DEFAULT_PORT: u16 = 18786;
const COOKIE_NAME: &str = "pockets_session";
const MAX_RPC_BODY_BYTES: usize = 1024 * 1024;
const MAX_BACKUP_UPLOAD_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonRecord {
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

#[derive(Clone)]
pub struct ServerState {
    pub ctx: AppContext,
    pub port: u16,
    pub token: String,
    pub dev_origin: Option<String>,
}

pub fn generate_session_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub fn session_key_path(ctx: &AppContext) -> PathBuf {
    ctx.data_dir().join("session.key")
}

/// Loads a persistent per-machine 256-bit session token from `session.key`
/// (`0600` perms), creating one on first run so browsers and Dock PWAs stay
/// signed in across daemon restarts.
pub fn load_or_create_session_token(ctx: &AppContext) -> Result<String, String> {
    if let Ok(env_token) = std::env::var("POCKETS_AUTH_TOKEN") {
        let trimmed = env_token.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }
    let path = session_key_path(ctx);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if trimmed.len() >= 32 {
            return Ok(trimmed.to_owned());
        }
    }
    std::fs::create_dir_all(ctx.data_dir())
        .map_err(|err| format!("Could not create Pockets data directory: {err}"))?;
    let token = generate_session_token();
    let temp = ctx
        .data_dir()
        .join(format!(".session-{}.tmp", Uuid::new_v4().simple()));
    {
        use std::io::Write;
        let mut file = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&temp)
            }
            #[cfg(not(unix))]
            {
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temp)
            }
        }
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
        let mut file = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&temp)
            }
            #[cfg(not(unix))]
            {
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temp)
            }
        }
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

pub fn bootstrap_url(port: u16, token: &str) -> String {
    format!("http://pockets.localhost:{port}/auth/bootstrap?token={token}")
}

pub fn dashboard_url(port: u16) -> String {
    format!("http://pockets.localhost:{port}/")
}

pub async fn run_daemon(
    ctx: AppContext,
    port: u16,
    dev_origin: Option<String>,
    token: String,
    on_ready: Option<tokio::sync::oneshot::Sender<u16>>,
) -> Result<(), String> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|err| format!("Could not bind Pockets daemon on {addr}: {err}"))?;
    let bound_port = listener
        .local_addr()
        .map_err(|err| format!("Could not read bound address: {err}"))?
        .port();

    let pid = std::process::id();
    write_daemon_record(
        &ctx,
        &DaemonRecord {
            pid,
            port: bound_port,
            token: token.clone(),
        },
    )?;

    let housekeeping_ctx = ctx.clone();
    tokio::spawn(async move {
        housekeeping_ctx.startup_housekeeping().await;
    });

    if let Some(tx) = on_ready {
        let _ = tx.send(bound_port);
    }

    let state = Arc::new(ServerState {
        ctx: ctx.clone(),
        port: bound_port,
        token,
        dev_origin,
    });

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
            _ = shutdown_signal() => {
                break;
            }
        }
    }

    remove_daemon_record(&ctx, pid);
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn allowed_host(host: &str, port: u16) -> bool {
    let lower = host.trim().to_ascii_lowercase();
    let expected = [
        format!("pockets.localhost:{port}"),
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
    ];
    expected.iter().any(|candidate| candidate == &lower)
}

fn allowed_origin(origin: &str, port: u16, dev_origin: Option<&str>) -> bool {
    let lower = origin.trim().to_ascii_lowercase();
    let expected = [
        format!("http://pockets.localhost:{port}"),
        format!("http://localhost:{port}"),
        format!("http://127.0.0.1:{port}"),
    ];
    if expected.iter().any(|candidate| candidate == &lower) {
        return true;
    }
    if let Some(dev) = dev_origin {
        if lower == dev.trim().trim_end_matches('/').to_ascii_lowercase() {
            return true;
        }
    }
    false
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

async fn handle_request(state: Arc<ServerState>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|host| allowed_host(host, state.port));
    if !host_ok {
        return json_response(
            StatusCode::FORBIDDEN,
            json!({ "error": "Refused request with disallowed Host header." }),
        );
    }

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
            if allowed_origin(origin, state.port, Some(dev)) {
                return Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
                    .header(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true")
                    .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
                    .header(
                        header::ACCESS_CONTROL_ALLOW_HEADERS,
                        "Content-Type, Authorization, X-Backup-Filename",
                    )
                    .body(Full::new(Bytes::new()))
                    .unwrap();
            }
        }
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Full::new(Bytes::new()))
            .unwrap();
    }

    let request_origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    if method == Method::POST {
        if let Some(origin) = request_origin.as_deref() {
            if !allowed_origin(origin, state.port, state.dev_origin.as_deref()) {
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
            json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
        ),

        (&Method::GET, "/auth/bootstrap") => {
            let provided = extract_query_param(&query, "token").unwrap_or_default();
            if !constant_time_eq(provided.as_bytes(), state.token.as_bytes()) {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({ "error": "Invalid Pockets session token." }),
                );
            }
            let cookie = format!(
                "{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000",
                state.token
            );
            Response::builder()
                .status(StatusCode::FOUND)
                .header(header::SET_COOKIE, cookie)
                .header(header::LOCATION, "/")
                .header(header::CACHE_CONTROL, "no-store")
                .body(Full::new(Bytes::new()))
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
                        json!({ "error": "Enter your Pockets session token or sign-in URL." }),
                    );
                }
            };
            // Accept either the raw token or a pasted full bootstrap URL.
            let raw = parsed.token.trim();
            let candidate = raw
                .split_once("token=")
                .map(|(_, rest)| rest.split(&['&', '#'][..]).next().unwrap_or(rest))
                .unwrap_or(raw)
                .trim();
            if !constant_time_eq(candidate.as_bytes(), state.token.as_bytes()) {
                return json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({ "error": "That session token did not match the running Pockets daemon." }),
                );
            }
            let cookie = format!(
                "{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000",
                state.token
            );
            let mut resp = json_response(StatusCode::OK, json!({ "ok": true }));
            if let Ok(val) = cookie.parse() {
                resp.headers_mut().insert(header::SET_COOKIE, val);
            }
            resp
        }

        (&Method::POST, p) if p.starts_with("/api/rpc/") => {
            if !is_authenticated(req.headers(), &state.token) {
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
            match rpc::dispatch(&state.ctx, &command, body).await {
                Ok(result) => json_response(StatusCode::OK, json!({ "ok": true, "result": result })),
                Err(error) => {
                    json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": error }))
                }
            }
        }

        (&Method::GET, p) if p.starts_with("/api/projects/") && p.ends_with("/backup") => {
            if !is_authenticated(req.headers(), &state.token) {
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
            if !is_authenticated(req.headers(), &state.token) {
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
        if allowed_origin(origin, state.port, Some(dev)) {
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

async fn handle_backup_download(ctx: &AppContext, id: String) -> Response<Full<Bytes>> {
    let temp = std::env::temp_dir().join(format!("pockets-export-{}.tar", Uuid::new_v4().simple()));
    let file_name = match project_lifecycle::export_project_data_to_path(ctx, id, &temp).await {
        Ok(name) => name,
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return json_response(StatusCode::BAD_REQUEST, json!({ "error": error }));
        }
    };
    let bytes = match tokio::fs::read(&temp).await {
        Ok(bytes) => bytes,
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("Could not read exported backup: {err}") }),
            );
        }
    };
    let _ = tokio::fs::remove_file(&temp).await;
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
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .unwrap()
}

async fn handle_backup_upload(
    ctx: &AppContext,
    id: String,
    display_name: Option<String>,
    body: Incoming,
) -> Response<Full<Bytes>> {
    let bytes = match read_bounded_body(body, MAX_BACKUP_UPLOAD_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return json_response(StatusCode::PAYLOAD_TOO_LARGE, json!({ "error": error }));
        }
    };
    if bytes.is_empty() {
        return json_response(
            StatusCode::BAD_REQUEST,
            json!({ "error": "The uploaded backup archive is empty." }),
        );
    }
    let temp = std::env::temp_dir().join(format!("pockets-import-{}.tar", Uuid::new_v4().simple()));
    if let Err(err) = tokio::fs::write(&temp, &bytes).await {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": format!("Could not stage backup archive: {err}") }),
        );
    }
    let outcome = project_lifecycle::restore_project_data_from_path(ctx, id, &temp, display_name).await;
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

fn serve_static_asset(request_path: &str) -> Response<Full<Bytes>> {
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
            .body(Full::new(Bytes::from_static(bytes)))
            .unwrap()
    } else if WEB_ASSETS.is_empty() {
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Full::new(Bytes::from_static(
                b"Pockets web UI assets were not bundled in this build. Run `npm run build:web` before `cargo build`.",
            )))
            .unwrap()
    } else {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Full::new(Bytes::from_static(b"Not found")))
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

fn json_response(status: StatusCode, value: Value) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_guard_allows_only_loopback_dashboard_hosts() {
        assert!(allowed_host("pockets.localhost:18786", 18786));
        assert!(allowed_host("127.0.0.1:18786", 18786));
        assert!(allowed_host("localhost:18786", 18786));
        assert!(!allowed_host("evil.com:18786", 18786));
        assert!(!allowed_host("notes.localhost:18786", 18786));
        assert!(!allowed_host("pockets.localhost:18788", 18786));
    }

    #[test]
    fn origin_guard_blocks_other_pockets_and_external_sites() {
        assert!(allowed_origin("http://pockets.localhost:18786", 18786, None));
        assert!(allowed_origin("http://127.0.0.1:18786", 18786, None));
        assert!(!allowed_origin("http://notes.localhost:18788", 18786, None));
        assert!(!allowed_origin("https://evil.example", 18786, None));
        assert!(allowed_origin(
            "http://localhost:1420",
            18786,
            Some("http://localhost:1420")
        ));
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
}
