//! Resolves a Pocket's browser-tab logo (favicon) for display on Pocket cards.
//!
//! Resolution order:
//! 1. `<link rel="...icon..." href="...">` declarations (including inline
//!    `data:image/...` URIs with raw `<svg>` tags or escaped quotes) across the
//!    Pocket's source files (`index.html`, `server.js`, and nested templates/JS),
//!    plus static `favicon.*` / `icon.*` image files anywhere in the Pocket's
//!    managed `source/` snapshot (or linked dev folder). Works even while the
//!    Pocket is stopped.
//! 2. If not found statically and the Pocket has a pinned loopback port in
//!    `local-sites.json`, probes the running Pocket's `/` HTML `<link rel="icon">`
//!    and common `/favicon.*` paths over `127.0.0.1`.
//!
//! Returns `None` when no valid image logo exists so the UI falls back to a
//! clean single-letter browser-tab monogram.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::github_install::{http_client, parse_github_repo, GitHubRepo};
use crate::{development, local_sites, AppContext, APP_VERSION};

const MAX_FAVICON_BYTES: usize = 256 * 1024;
const MAX_HTML_SCAN_BYTES: usize = 512 * 1024;
const MAX_WALK_DEPTH: usize = 4;
const MAX_SCANNED_TEXT_FILES: usize = 40;
const LIVE_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const REMOTE_FAVICON_TIMEOUT: Duration = Duration::from_secs(5);
const REMOTE_FAVICON_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_REMOTE_FAVICON_CACHE_ENTRIES: usize = 64;
const RAW_GITHUB_BASE: &str = "https://raw.githubusercontent.com";

const REMOTE_STATIC_FAVICON_PATHS: &[&str] = &[
    "public/favicon.svg",
    "favicon.svg",
    "public/favicon.png",
    "favicon.png",
    "public/favicon.ico",
    "favicon.ico",
    "static/favicon.svg",
    "web/favicon.svg",
    "src/favicon.svg",
    "client/public/favicon.svg",
];

const EXCLUDED_DIRS: &[&str] = &[".git", "node_modules", "data", "target", ".next"];

const PRIORITY_TEXT_FILES: &[&str] = &[
    "index.html",
    "public/index.html",
    "static/index.html",
    "web/index.html",
    "client/index.html",
    "src/index.html",
    "views/index.html",
    "server.js",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaviconAsset {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
}

fn content_type_for_extension(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "svg" => Some("image/svg+xml"),
        "png" => Some("image/png"),
        "ico" => Some("image/x-icon"),
        "webp" => Some("image/webp"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

fn normalize_image_mime(raw: &str) -> Option<&'static str> {
    let base = raw.split(';').next()?.trim().to_ascii_lowercase();
    match base.as_str() {
        "image/svg+xml" => Some("image/svg+xml"),
        "image/png" => Some("image/png"),
        "image/x-icon" | "image/vnd.microsoft.icon" | "image/ico" => Some("image/x-icon"),
        "image/webp" => Some("image/webp"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        _ => None,
    }
}

fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.is_empty() || bytes.len() > MAX_FAVICON_BYTES {
        return None;
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.len() >= 4
        && (bytes.starts_with(&[0x00, 0x00, 0x01, 0x00])
            || bytes.starts_with(&[0x00, 0x00, 0x02, 0x00]))
    {
        return Some("image/x-icon");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        let trimmed = text.trim_start_matches('\u{feff}').trim_start();
        let head = &trimmed[..trimmed.len().min(512)];
        let head_lower = head.to_ascii_lowercase();
        if head_lower.contains("<svg") && !head_lower.contains("<html") && !head_lower.contains("<!doctype html") {
            return Some("image/svg+xml");
        }
    }
    None
}

fn looks_like_image(bytes: &[u8], content_type: &str) -> bool {
    if bytes.is_empty() || bytes.len() > MAX_FAVICON_BYTES {
        return false;
    }
    match content_type {
        "image/svg+xml" => sniff_image_mime(bytes) == Some("image/svg+xml"),
        "image/png" => sniff_image_mime(bytes) == Some("image/png"),
        "image/x-icon" => matches!(
            sniff_image_mime(bytes),
            Some("image/x-icon") | Some("image/png")
        ),
        "image/gif" => sniff_image_mime(bytes) == Some("image/gif"),
        "image/jpeg" => sniff_image_mime(bytes) == Some("image/jpeg"),
        "image/webp" => sniff_image_mime(bytes) == Some("image/webp"),
        _ => false,
    }
}

fn read_bounded_image(path: &Path) -> Option<FaviconAsset> {
    let expected_type = content_type_for_extension(path)?;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() as usize > MAX_FAVICON_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if !looks_like_image(&bytes, expected_type) {
        return None;
    }
    Some(FaviconAsset {
        bytes,
        content_type: expected_type,
    })
}

fn safe_relative_path(raw: &str) -> Option<PathBuf> {
    let without_query = raw.split(['?', '#']).next().unwrap_or(raw).trim();
    let trimmed = without_query.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.contains('\\') {
        return None;
    }
    let mut clean = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if clean.as_os_str().is_empty() {
        None
    } else {
        Some(clean)
    }
}

fn percent_decode(input: &str) -> Vec<u8> {
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
    out
}

fn decode_data_uri(uri: &str) -> Option<FaviconAsset> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let is_base64 = meta
        .split(';')
        .skip(1)
        .any(|part| part.trim().eq_ignore_ascii_case("base64"));
    let content_type = normalize_image_mime(meta)?;
    let bytes = if is_base64 {
        let compact: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(compact)
            .ok()?
    } else {
        percent_decode(payload)
    };
    if !looks_like_image(&bytes, content_type) {
        return None;
    }
    Some(FaviconAsset {
        bytes,
        content_type,
    })
}

fn extract_attr(tag: &str, attr_name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let needle = attr_name.as_bytes();
    let mut pos = 0;
    while pos + needle.len() < bytes.len() {
        let Some(found) = lower[pos..].find(attr_name) else {
            return None;
        };
        let start = pos + found;
        let before_ok = start == 0
            || bytes[start - 1].is_ascii_whitespace()
            || bytes[start - 1] == b'<';
        let after = start + needle.len();
        if before_ok {
            let rest = &tag[after..];
            let trimmed_eq = rest.trim_start();
            if let Some(after_eq) = trimmed_eq.strip_prefix('=') {
                let val_part = after_eq.trim_start();
                if let Some(quoted) = val_part.strip_prefix('"') {
                    if let Some(end) = quoted.find('"') {
                        return Some(quoted[..end].to_owned());
                    }
                } else if let Some(quoted) = val_part.strip_prefix('\'') {
                    if let Some(end) = quoted.find('\'') {
                        return Some(quoted[..end].to_owned());
                    }
                } else {
                    let end = val_part
                        .find(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                        .unwrap_or(val_part.len());
                    if end > 0 {
                        return Some(val_part[..end].to_owned());
                    }
                }
            }
        }
        pos = after;
    }
    None
}

/// Finds the closing `>` of an HTML tag starting at `start` (right after `<link`),
/// ignoring `>` characters that appear inside single- or double-quoted attribute
/// values (such as inline `data:image/svg+xml,<svg ...>...</svg>` URIs).
fn find_tag_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = start;
    let limit = (start + MAX_FAVICON_BYTES + 1024).min(bytes.len());
    while i < limit {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some(i),
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// Extracts candidate `href` values from `<link rel="...icon..." href="...">`
/// tags inside an HTML document or JS template string (including strings with
/// JS-escaped quotes `\"` or `\'` and unencoded `<svg>` tags inside `href`).
fn extract_icon_hrefs(raw_text: &str) -> Vec<String> {
    let normalized;
    let text = if raw_text.contains("\\\"") || raw_text.contains("\\'") {
        normalized = raw_text.replace("\\\"", "\"").replace("\\'", "'");
        normalized.as_str()
    } else {
        raw_text
    };

    let lower = text.to_ascii_lowercase();
    let mut hrefs = Vec::new();
    let mut offset = 0;
    while let Some(idx) = lower[offset..].find("<link") {
        let tag_start = offset + idx;
        let after_link = tag_start + 5;
        if after_link >= text.len() || !text.as_bytes()[after_link].is_ascii_whitespace() {
            offset = after_link;
            continue;
        }
        let Some(tag_end) = find_tag_end(text, after_link) else {
            offset = after_link;
            continue;
        };
        let tag = &text[tag_start..=tag_end];
        offset = tag_end + 1;

        if let Some(rel) = extract_attr(tag, "rel") {
            let is_icon = rel.split_whitespace().any(|token| {
                token.eq_ignore_ascii_case("icon") || token.eq_ignore_ascii_case("apple-touch-icon")
            });
            if is_icon {
                if let Some(href) = extract_attr(tag, "href") {
                    let trimmed = href.trim();
                    if !trimmed.is_empty() && !trimmed.contains("${") {
                        hrefs.push(trimmed.to_owned());
                    }
                }
            }
        }
    }
    hrefs
}

fn favicon_filename_priority(file_name: &str) -> Option<u8> {
    let lower = file_name.to_ascii_lowercase();
    match lower.as_str() {
        "favicon.svg" => Some(0),
        "favicon.png" => Some(1),
        "favicon.ico" => Some(2),
        "favicon.webp" | "favicon.jpg" | "favicon.jpeg" | "favicon.gif" => Some(3),
        "apple-touch-icon.png" => Some(4),
        "icon.svg" => Some(5),
        "logo.svg" => Some(6),
        "icon.png" => Some(7),
        _ if lower.starts_with("favicon-") && content_type_for_extension(Path::new(&lower)).is_some() => {
            Some(4)
        }
        _ => None,
    }
}

fn is_scannable_text_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "html" | "htm" | "js" | "mjs" | "cjs" | "jsx" | "tsx" | "ts" | "ejs" | "hbs"
    )
}

fn collect_candidates(
    root: &Path,
    dir: &Path,
    depth: usize,
    text_files: &mut Vec<PathBuf>,
    image_files: &mut Vec<(u8, usize, PathBuf)>,
) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut sorted: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    sorted.sort();
    for path in sorted {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if meta.is_dir() {
            if !EXCLUDED_DIRS.contains(&name) {
                collect_candidates(root, &path, depth + 1, text_files, image_files);
            }
        } else if meta.is_file() {
            if let Some(prio) = favicon_filename_priority(name) {
                image_files.push((prio, depth, path));
            } else if is_scannable_text_file(&path)
                && meta.len() <= MAX_HTML_SCAN_BYTES as u64
                && text_files.len() < MAX_SCANNED_TEXT_FILES
            {
                text_files.push(path);
            }
        }
    }
}

fn resolve_href_in_root(root: &Path, href: &str) -> Option<FaviconAsset> {
    if href.starts_with("data:") {
        return decode_data_uri(href);
    }
    if href.contains("://") || href.starts_with("//") {
        return None;
    }
    let rel = safe_relative_path(href)?;
    for base in [
        root.to_path_buf(),
        root.join("public"),
        root.join("static"),
        root.join("assets"),
        root.join("web"),
        root.join("client"),
        root.join("src"),
    ] {
        if let Some(asset) = read_bounded_image(&base.join(&rel)) {
            return Some(asset);
        }
    }
    None
}

fn resolve_from_directory(root: &Path) -> Option<FaviconAsset> {
    if !root.is_dir() {
        return None;
    }

    // 1. Scan high-priority entrypoints first (`index.html`, `server.js`, etc.).
    for candidate in PRIORITY_TEXT_FILES {
        let path = root.join(candidate);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() as usize > MAX_HTML_SCAN_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for href in extract_icon_hrefs(&text) {
            if let Some(asset) = resolve_href_in_root(root, &href) {
                return Some(asset);
            }
        }
    }

    // 2. Walk the project tree for static favicon/icon files and nested HTML/JS
    //    templates that declare `<link rel="icon">`.
    let mut text_files = Vec::new();
    let mut image_files = Vec::new();
    collect_candidates(root, root, 0, &mut text_files, &mut image_files);

    for path in text_files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for href in extract_icon_hrefs(&text) {
            if let Some(asset) = resolve_href_in_root(root, &href) {
                return Some(asset);
            }
        }
    }

    image_files.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    for (_, _, path) in image_files {
        if let Some(asset) = read_bounded_image(&path) {
            return Some(asset);
        }
    }

    None
}

async fn fetch_loopback_image(
    client: &reqwest::Client,
    url: &str,
    fallback_path: &str,
) -> Option<FaviconAsset> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_FAVICON_BYTES {
            return None;
        }
    }
    let header_mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_image_mime);
    let bytes = resp.bytes().await.ok()?;
    let content_type = header_mime
        .or_else(|| sniff_image_mime(&bytes))
        .or_else(|| content_type_for_extension(Path::new(fallback_path)))?;
    if !looks_like_image(&bytes, content_type) {
        return None;
    }
    Some(FaviconAsset {
        bytes: bytes.to_vec(),
        content_type,
    })
}

async fn probe_running_pocket_favicon(port: u16) -> Option<FaviconAsset> {
    let client = reqwest::Client::builder()
        .timeout(LIVE_PROBE_TIMEOUT)
        .user_agent(format!("pockets/{APP_VERSION}"))
        .build()
        .ok()?;
    let base = format!("http://127.0.0.1:{port}");

    // Try reading `<link rel="icon">` from the root page first.
    if let Ok(resp) = client.get(format!("{base}/")).send().await {
        if resp.status().is_success() {
            let is_html = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.to_ascii_lowercase().contains("html"))
                .unwrap_or(true);
            if is_html {
                if let Ok(bytes) = resp.bytes().await {
                    if bytes.len() <= MAX_HTML_SCAN_BYTES {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            for href in extract_icon_hrefs(text) {
                                if href.starts_with("data:") {
                                    if let Some(asset) = decode_data_uri(&href) {
                                        return Some(asset);
                                    }
                                } else if !href.contains("://") && !href.starts_with("//") {
                                    let rel = href.trim_start_matches('/');
                                    if !rel.is_empty() && !rel.contains("..") {
                                        let url = format!("{base}/{rel}");
                                        if let Some(asset) =
                                            fetch_loopback_image(&client, &url, rel).await
                                        {
                                            return Some(asset);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    for path in [
        "favicon.svg",
        "favicon.ico",
        "favicon.png",
        "icon.svg",
        "icon.png",
        "apple-touch-icon.png",
    ] {
        let url = format!("{base}/{path}");
        if let Some(asset) = fetch_loopback_image(&client, &url, path).await {
            return Some(asset);
        }
    }
    None
}

/// Resolves the browser-tab logo for persisted project `id`, if one exists.
pub async fn resolve_project_favicon(ctx: &AppContext, id: &str) -> Option<FaviconAsset> {
    let store = ctx.store();
    let project_id = id.to_owned();
    let (static_asset, pinned_port) = tokio::task::spawn_blocking(move || {
        let Ok(project) = store.load(&project_id) else {
            return (None, None);
        };
        let pinned_port = local_sites::pinned_port_for_project(&store, &project.id);
        let mut roots = Vec::new();
        if let Ok(source_dir) = store.source_path(&project.id) {
            roots.push(source_dir);
        }
        if let Ok(Some(dev_folder)) = development::linked_folder(&store, &project.id) {
            roots.push(dev_folder);
        }
        for root in roots {
            if let Some(asset) = resolve_from_directory(&root) {
                return (Some(asset), pinned_port);
            }
        }
        (None, pinned_port)
    })
    .await
    .ok()?;

    if let Some(asset) = static_asset {
        return Some(asset);
    }
    if let Some(port) = pinned_port {
        return probe_running_pocket_favicon(port).await;
    }
    None
}

struct CachedRemoteFavicon {
    fetched_at: Instant,
    asset: Option<FaviconAsset>,
}

static REMOTE_FAVICON_CACHE: OnceLock<tokio::sync::Mutex<HashMap<String, CachedRemoteFavicon>>> =
    OnceLock::new();

fn remote_favicon_cache() -> &'static tokio::sync::Mutex<HashMap<String, CachedRemoteFavicon>> {
    REMOTE_FAVICON_CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

async fn fetch_remote_image(
    client: &reqwest::Client,
    url: &str,
    path_hint: &str,
) -> Option<FaviconAsset> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    if let Some(len) = response.content_length() {
        if len as usize > MAX_FAVICON_BYTES {
            return None;
        }
    }
    let header_mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_image_mime);
    let bytes = response.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > MAX_FAVICON_BYTES {
        return None;
    }
    let content_type = sniff_image_mime(&bytes)
        .or(header_mime)
        .or_else(|| content_type_for_extension(Path::new(path_hint)))?;
    if !looks_like_image(&bytes, content_type) {
        return None;
    }
    Some(FaviconAsset {
        bytes: bytes.to_vec(),
        content_type,
    })
}

async fn fetch_remote_text(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    if let Some(len) = response.content_length() {
        if len as usize > MAX_HTML_SCAN_BYTES {
            return None;
        }
    }
    let bytes = response.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > MAX_HTML_SCAN_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

async fn resolve_github_repo_favicon_from_base(
    client: &reqwest::Client,
    raw_base: &str,
    repo: &GitHubRepo,
) -> Option<FaviconAsset> {
    let repo_prefix = format!("{raw_base}/{}/{}/HEAD", repo.owner, repo.repo);

    // 1. Check common static favicon paths concurrently and keep highest priority.
    let mut static_set = tokio::task::JoinSet::new();
    for (idx, path) in REMOTE_STATIC_FAVICON_PATHS.iter().enumerate() {
        let client = client.clone();
        let url = format!("{repo_prefix}/{path}");
        let path_owned = (*path).to_owned();
        static_set.spawn(async move {
            (idx, fetch_remote_image(&client, &url, &path_owned).await)
        });
    }
    let mut best_static: Option<(usize, FaviconAsset)> = None;
    while let Some(res) = static_set.join_next().await {
        if let Ok((idx, Some(asset))) = res {
            if best_static.as_ref().is_none_or(|(best_idx, _)| idx < *best_idx) {
                best_static = Some((idx, asset));
            }
        }
    }
    if let Some((_, asset)) = best_static {
        return Some(asset);
    }

    // 2. Scan priority HTML/JS entrypoints for `<link rel="icon">` (including
    //    inline `data:image/...` URIs or relative icon paths).
    let mut text_set = tokio::task::JoinSet::new();
    for (idx, path) in PRIORITY_TEXT_FILES.iter().enumerate() {
        let client = client.clone();
        let url = format!("{repo_prefix}/{path}");
        text_set.spawn(async move { (idx, fetch_remote_text(&client, &url).await) });
    }
    let mut texts: Vec<(usize, String)> = Vec::new();
    while let Some(res) = text_set.join_next().await {
        if let Ok((idx, Some(text))) = res {
            texts.push((idx, text));
        }
    }
    texts.sort_by_key(|(idx, _)| *idx);

    for (_, text) in texts {
        for href in extract_icon_hrefs(&text) {
            if href.starts_with("data:") {
                if let Some(asset) = decode_data_uri(&href) {
                    return Some(asset);
                }
                continue;
            }
            if href.contains("://") || href.starts_with("//") {
                continue;
            }
            let Some(rel) = safe_relative_path(&href) else {
                continue;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            for prefix in ["", "public/", "static/", "assets/", "web/", "client/", "src/"] {
                let candidate = format!("{prefix}{rel_str}");
                let url = format!("{repo_prefix}/{candidate}");
                if let Some(asset) = fetch_remote_image(client, &url, &candidate).await {
                    return Some(asset);
                }
            }
        }
    }

    None
}

/// Resolves a public GitHub Pocket repository's favicon from raw GitHub files,
/// caching both hits and misses in memory.
pub async fn resolve_github_repo_favicon(raw_repo: &str) -> Option<FaviconAsset> {
    let repo = parse_github_repo(raw_repo).ok()?;
    let cache_key = repo.display_slug().to_ascii_lowercase();

    {
        let guard = remote_favicon_cache().lock().await;
        if let Some(entry) = guard.get(&cache_key) {
            if entry.fetched_at.elapsed() < REMOTE_FAVICON_CACHE_TTL {
                return entry.asset.clone();
            }
        }
    }

    let client = http_client(REMOTE_FAVICON_TIMEOUT).ok()?;
    let asset = resolve_github_repo_favicon_from_base(&client, RAW_GITHUB_BASE, &repo).await;

    let mut guard = remote_favicon_cache().lock().await;
    if guard.len() >= MAX_REMOTE_FAVICON_CACHE_ENTRIES && !guard.contains_key(&cache_key) {
        guard.retain(|_, entry| entry.fetched_at.elapsed() < REMOTE_FAVICON_CACHE_TTL);
        if guard.len() >= MAX_REMOTE_FAVICON_CACHE_ENTRIES {
            guard.clear();
        }
    }
    guard.insert(
        cache_key,
        CachedRemoteFavicon {
            fetched_at: Instant::now(),
            asset: asset.clone(),
        },
    );
    asset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_percent_encoded_inline_svg_data_uri_from_server_js() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("server.js"),
            r#"const html = `<head><link rel="icon" type="image/svg+xml" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Ccircle cx='8' cy='8' r='8' fill='%236bcb8b'/%3E%3C/svg%3E"></head>`;"#,
        )
        .unwrap();

        let asset = resolve_from_directory(dir.path()).expect("should resolve inline svg");
        assert_eq!(asset.content_type, "image/svg+xml");
        let text = String::from_utf8(asset.bytes).unwrap();
        assert!(text.starts_with("<svg"));
    }

    #[test]
    fn extracts_raw_unencoded_svg_data_uri_inside_quoted_href() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            r#"<head><link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'><text y='24'>🥝</text></svg>"></head>"#,
        )
        .unwrap();

        let asset = resolve_from_directory(dir.path()).expect("should resolve unencoded inline svg");
        assert_eq!(asset.content_type, "image/svg+xml");
        let text = String::from_utf8(asset.bytes).unwrap();
        assert!(text.contains("<text y='24'>🥝</text>"));
    }

    #[test]
    fn resolves_static_favicon_in_nested_directory_and_ignores_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_from_directory(dir.path()).is_none());

        std::fs::create_dir_all(dir.path().join("web/img")).unwrap();
        std::fs::write(
            dir.path().join("web/img/favicon.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"></svg>"#,
        )
        .unwrap();

        let asset = resolve_from_directory(dir.path()).expect("should resolve nested favicon.svg");
        assert_eq!(asset.content_type, "image/svg+xml");
    }

    #[test]
    fn resolves_relative_link_href_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(
            dir.path().join("assets/brand.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            r#"<link href="../escape.svg" rel="icon"><link href="/assets/brand.svg?v=2" rel="shortcut icon">"#,
        )
        .unwrap();

        let asset = resolve_from_directory(dir.path()).expect("should resolve /assets/brand.svg");
        assert_eq!(asset.content_type, "image/svg+xml");
    }

    async fn spawn_mock_raw_server(
        routes: Vec<(String, &'static str, Vec<u8>)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes = std::sync::Arc::new(routes);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("");
                    if let Some((_, content_type, body)) =
                        routes.iter().find(|(p, _, _)| p == path)
                    {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(body).await;
                    } else {
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                    }
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn resolves_remote_github_static_favicon_svg() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle cx="8" cy="8" r="8"/></svg>"#.to_vec();
        let (base, handle) = spawn_mock_raw_server(vec![(
            "/runpockets/coinkiwi/HEAD/public/favicon.svg".to_owned(),
            "image/svg+xml",
            svg.clone(),
        )])
        .await;
        let client = http_client(Duration::from_secs(2)).unwrap();
        let repo = GitHubRepo {
            owner: "runpockets".to_owned(),
            repo: "coinkiwi".to_owned(),
        };
        let asset = resolve_github_repo_favicon_from_base(&client, &base, &repo)
            .await
            .expect("should resolve public/favicon.svg");
        assert_eq!(asset.content_type, "image/svg+xml");
        assert_eq!(asset.bytes, svg);
        handle.abort();
    }

    #[tokio::test]
    async fn resolves_remote_github_inline_svg_from_index_html_and_returns_none_when_missing() {
        let html = br#"<head><link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'><rect width='16' height='16'/></svg>"></head>"#.to_vec();
        let (base, handle) = spawn_mock_raw_server(vec![(
            "/runpockets/coinkiwi/HEAD/public/index.html".to_owned(),
            "text/html",
            html,
        )])
        .await;
        let client = http_client(Duration::from_secs(2)).unwrap();
        let repo = GitHubRepo {
            owner: "runpockets".to_owned(),
            repo: "coinkiwi".to_owned(),
        };
        let asset = resolve_github_repo_favicon_from_base(&client, &base, &repo)
            .await
            .expect("should resolve inline svg data uri");
        assert_eq!(asset.content_type, "image/svg+xml");

        let empty_repo = GitHubRepo {
            owner: "runpockets".to_owned(),
            repo: "no-icon".to_owned(),
        };
        assert!(
            resolve_github_repo_favicon_from_base(&client, &base, &empty_repo)
                .await
                .is_none()
        );
        handle.abort();
    }
}
