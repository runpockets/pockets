//! Curated Pockets Hub catalog with live repository manifest enrichment and
//! offline bundled fallback.
//!
//! By default Pockets fetches `catalog.json` from
//! `https://raw.githubusercontent.com/runpockets/pockets-hub/main/catalog.json`
//! (overridable via `POCKETS_HUB_URL`). Each entry points to an independent
//! public GitHub Pocket repository (`owner/repo`). Pockets enriches every entry
//! from that repository's live `pocket.yaml` so its title, version, and
//! description match the live Pocket card verbatim, falling back gracefully to
//! the bundled catalog metadata when offline.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::development;
use crate::git;
use crate::github_install::{
    fetch_validated_manifest, fetch_validated_manifest_from_bases, find_existing_project_for_repo,
    http_client, parse_github_repo, GitHubRepo,
};
use crate::manifest::{Architecture, PreviewApi, RuntimeVariable};
use crate::AppContext;

pub const DEFAULT_HUB_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/runpockets/pockets-hub/main/catalog.json";
pub const HUB_REPO_URL: &str = "https://github.com/runpockets/pockets-hub";
const BUNDLED_CATALOG_JSON: &str = include_str!("assets/hub-catalog.json");
const HUB_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const HUB_FETCH_TIMEOUT: Duration = Duration::from_secs(6);
const MAX_CATALOG_BYTES: usize = 512 * 1024;
const MAX_CATALOG_ENTRIES: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHubCatalog {
    version: u32,
    #[serde(default)]
    pockets: Vec<RawHubEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawHubEntry {
    repo: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    verified: bool,
}

/// One curated Pocket returned to the New Pocket UI.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HubPocket {
    pub repo: String,
    pub repo_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub verified: bool,
    /// `"repo"` when name/description came directly from the repo's live
    /// `pocket.yaml`, or `"catalog"` when falling back to catalog metadata.
    pub metadata_source: String,
    pub host_supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub architectures: Vec<Architecture>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview_apis: Vec<PreviewApi>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_variables: Vec<RuntimeVariable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_project_id: Option<String>,
}

/// Response returned by `list_hub_pockets`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HubCatalogResponse {
    /// `"remote"` when fetched from the hub repo, `"bundled"` on fallback.
    pub source: String,
    pub hub_url: String,
    pub pockets: Vec<HubPocket>,
}

struct CachedHub {
    fetched_at: Instant,
    response: HubCatalogResponse,
}

static HUB_CACHE: OnceLock<tokio::sync::Mutex<Option<CachedHub>>> = OnceLock::new();

fn hub_cache() -> &'static tokio::sync::Mutex<Option<CachedHub>> {
    HUB_CACHE.get_or_init(|| tokio::sync::Mutex::new(None))
}

fn resolved_hub_url() -> String {
    if let Ok(raw) = std::env::var("POCKETS_HUB_URL") {
        let trimmed = raw.trim();
        if trimmed.starts_with("https://")
            || trimmed.starts_with("http://127.0.0.1:")
            || trimmed.starts_with("http://localhost:")
        {
            return trimmed.to_owned();
        }
    }
    DEFAULT_HUB_CATALOG_URL.to_owned()
}

fn parse_catalog(bytes: &[u8]) -> Result<RawHubCatalog, String> {
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err("Hub catalog exceeds maximum supported size.".into());
    }
    let catalog: RawHubCatalog =
        serde_json::from_slice(bytes).map_err(|err| format!("Invalid hub catalog JSON: {err}"))?;
    if catalog.version != 1 {
        return Err(format!("Unsupported hub catalog version {}.", catalog.version));
    }
    Ok(catalog)
}

fn bundled_catalog() -> RawHubCatalog {
    parse_catalog(BUNDLED_CATALOG_JSON.as_bytes()).expect("bundled hub catalog must be valid")
}

async fn fetch_remote_catalog(client: &reqwest::Client, url: &str) -> Result<RawHubCatalog, String> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|err| format!("Could not reach Pockets Hub: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Pockets Hub returned HTTP {}.",
            response.status().as_u16()
        ));
    }
    if let Some(len) = response.content_length() {
        if len as usize > MAX_CATALOG_BYTES {
            return Err("Hub catalog exceeds maximum supported size.".into());
        }
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("Could not read Pockets Hub response: {err}"))?;
    parse_catalog(&bytes)
}

fn fallback_pocket(repo: &GitHubRepo, entry: &RawHubEntry) -> HubPocket {
    HubPocket {
        repo: repo.display_slug(),
        repo_url: repo.html_url(),
        id: None,
        name: entry
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&repo.repo)
            .to_owned(),
        description: entry
            .description
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_owned(),
        version: None,
        category: entry
            .category
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        tags: entry.tags.clone(),
        verified: entry.verified,
        metadata_source: "catalog".to_owned(),
        host_supported: true,
        host_error: None,
        architectures: Vec::new(),
        preview_apis: Vec::new(),
        runtime_variables: Vec::new(),
        existing_project_id: None,
    }
}

async fn enrich_entry(
    client: &reqwest::Client,
    entry: RawHubEntry,
    custom_bases: Option<(&str, &str)>,
) -> Option<HubPocket> {
    let repo = parse_github_repo(&entry.repo).ok()?;
    let manifest_result = match custom_bases {
        Some((raw_base, api_base)) => {
            fetch_validated_manifest_from_bases(client, raw_base, api_base, &repo).await
        }
        None => fetch_validated_manifest(client, &repo).await,
    };

    match manifest_result {
        Ok(manifest) => {
            let host_error = manifest.validate_host().err();
            let host_supported = host_error.is_none();
            Some(HubPocket {
                repo: repo.display_slug(),
                repo_url: repo.html_url(),
                id: Some(manifest.metadata.id),
                name: manifest.metadata.name,
                description: manifest.metadata.description,
                version: Some(manifest.metadata.version),
                category: entry
                    .category
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                tags: entry.tags,
                verified: entry.verified,
                metadata_source: "repo".to_owned(),
                host_supported,
                host_error,
                architectures: manifest.runtime.architectures,
                preview_apis: manifest.preview_apis,
                runtime_variables: manifest.runtime_variables,
                existing_project_id: None,
            })
        }
        Err(_) => Some(fallback_pocket(&repo, &entry)),
    }
}

async fn build_hub_response(
    client: &reqwest::Client,
    hub_url: &str,
    custom_bases: Option<(String, String)>,
) -> HubCatalogResponse {
    let (source, raw_catalog) = match fetch_remote_catalog(client, hub_url).await {
        Ok(catalog) if !catalog.pockets.is_empty() => ("remote".to_owned(), catalog),
        _ => ("bundled".to_owned(), bundled_catalog()),
    };

    let entries: Vec<RawHubEntry> = raw_catalog
        .pockets
        .into_iter()
        .take(MAX_CATALOG_ENTRIES)
        .collect();

    let mut join_set = tokio::task::JoinSet::new();
    for (idx, entry) in entries.into_iter().enumerate() {
        let client = client.clone();
        let bases = custom_bases.clone();
        join_set.spawn(async move {
            let bases_ref = bases
                .as_ref()
                .map(|(r, a)| (r.as_str(), a.as_str()));
            (idx, enrich_entry(&client, entry, bases_ref).await)
        });
    }

    let mut ordered: Vec<(usize, HubPocket)> = Vec::new();
    while let Some(res) = join_set.join_next().await {
        if let Ok((idx, Some(pocket))) = res {
            ordered.push((idx, pocket));
        }
    }
    ordered.sort_by_key(|(idx, _)| *idx);

    HubCatalogResponse {
        source,
        hub_url: hub_url.to_owned(),
        pockets: ordered.into_iter().map(|(_, p)| p).collect(),
    }
}

fn annotate_existing(ctx: &AppContext, mut response: HubCatalogResponse) -> HubCatalogResponse {
    for pocket in &mut response.pockets {
        if let Ok(repo) = parse_github_repo(&pocket.repo) {
            pocket.existing_project_id = find_existing_project_for_repo(
                ctx,
                &repo,
                pocket.id.as_deref(),
                Some(&pocket.name),
            );
        }
    }
    response
}

/// Lists curated Pockets from the Pockets Hub (with live `pocket.yaml`
/// metadata from each Pocket's repository and bundled offline fallback).
pub async fn list_hub_pockets(ctx: &AppContext, force: bool) -> Result<HubCatalogResponse, String> {
    let cached_response = if !force {
        let guard = hub_cache().lock().await;
        guard
            .as_ref()
            .filter(|entry| entry.fetched_at.elapsed() < HUB_CACHE_TTL)
            .map(|entry| entry.response.clone())
    } else {
        None
    };

    let base_response = match cached_response {
        Some(resp) => resp,
        None => {
            let hub_url = resolved_hub_url();
            let client = http_client(HUB_FETCH_TIMEOUT)?;
            let resp = build_hub_response(&client, &hub_url, None).await;
            let mut guard = hub_cache().lock().await;
            *guard = Some(CachedHub {
                fetched_at: Instant::now(),
                response: resp.clone(),
            });
            resp
        }
    };

    let ctx_clone = ctx.clone();
    let annotated = tokio::task::spawn_blocking(move || annotate_existing(&ctx_clone, base_response))
        .await
        .map_err(|_| "Checking installed Hub Pockets was interrupted.".to_owned())?;
    Ok(annotated)
}

/// One validation check shown in the "Submit to Pockets Hub" drawer.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionCheck {
    pub id: String,
    pub label: String,
    /// `"pass"`, `"warn"`, `"fail"`, or `"pending"`.
    pub status: String,
    pub detail: String,
}

/// Readiness report returned by `check_hub_submission`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HubSubmissionReport {
    pub project_id: String,
    pub pocket_id: String,
    pub pocket_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_repo: Option<String>,
    pub already_in_hub: bool,
    pub ready_to_submit: bool,
    pub remote_verified: bool,
    pub checks: Vec<SubmissionCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_entry_json: Option<String>,
    pub pr_edit_url: String,
    pub issue_submit_url: String,
}

const EXCLUDED_AUDIT_DIRS: &[&str] = &[".git", "node_modules", "data", "dist", "target"];
const FORBIDDEN_SECRET_FILES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    ".env.development",
    "credentials.json",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
];
const FORBIDDEN_SECRET_EXTS: &[&str] = &["pem", "key", "p12", "pfx"];
const STARTER_PLACEHOLDER_DESCS: &[&str] = &[
    "a minimal pocket starter",
    "minimal hello pocket",
    "starter pocket",
];

/// Parses a git remote URL (HTTPS or SSH, including `github-*` SSH config host
/// aliases) into a GitHub `owner/repo` slug.
pub(crate) fn parse_git_remote_github_repo(remote: &str) -> Option<GitHubRepo> {
    let trimmed = remote.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(repo) = parse_github_repo(trimmed) {
        return Some(repo);
    }
    // ssh://git@github.com/owner/repo[.git]
    if let Some(rest) = trimmed
        .strip_prefix("ssh://git@github.com/")
        .or_else(|| trimmed.strip_prefix("ssh://github.com/"))
    {
        return parse_github_repo(rest).ok();
    }
    // git@github.com:owner/repo[.git] or git@github-<alias>:owner/repo[.git]
    if let Some(after_at) = trimmed.strip_prefix("git@") {
        if let Some((host, path)) = after_at.split_once(':') {
            let host_lower = host.to_ascii_lowercase();
            if host_lower == "github.com" || host_lower.starts_with("github-") {
                return parse_github_repo(path).ok();
            }
        }
    }
    None
}

fn detect_github_remote(folder: &Path) -> Option<String> {
    let git_bin = git::find_git()?;
    let url = git::run_text(&git_bin, folder, &["remote", "get-url", "origin"]).ok()?;
    parse_git_remote_github_repo(&url).map(|r| r.display_slug())
}

/// Audits a `Containerfile` against the same strict security rules enforced by
/// Pockets Hub CI (non-root final USER, no curl|sh, no sockets, no hardcoded
/// ENV secrets, no remote ADD).
pub(crate) fn audit_containerfile(text: &str) -> Vec<String> {
    let mut issues = Vec::new();
    if text.trim().is_empty() {
        issues.push("Containerfile is empty.".into());
        return issues;
    }

    let mut logical_lines = Vec::new();
    let mut buf = String::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            if !buf.is_empty() {
                logical_lines.push(std::mem::take(&mut buf));
            }
            continue;
        }
        if let Some(stripped) = trimmed.strip_suffix('\\') {
            buf.push_str(stripped);
            buf.push(' ');
        } else {
            buf.push_str(trimmed);
            logical_lines.push(std::mem::take(&mut buf).trim().to_owned());
        }
    }
    if !buf.trim().is_empty() {
        logical_lines.push(buf.trim().to_owned());
    }

    let mut from_count = 0usize;
    let mut last_user: Option<String> = None;

    for line in logical_lines {
        let mut parts = line.splitn(2, char::is_whitespace);
        let Some(instr_raw) = parts.next() else {
            continue;
        };
        let args = parts.next().unwrap_or("").trim();
        let instr = instr_raw.to_ascii_uppercase();
        let args_lower = args.to_ascii_lowercase();

        match instr.as_str() {
            "FROM" => {
                from_count += 1;
                last_user = None;
            }
            "USER" => {
                if let Some(spec) = args.split_whitespace().next() {
                    last_user = Some(spec.to_owned());
                }
            }
            "ADD" => {
                if args_lower.contains("http://") || args_lower.contains("https://") {
                    issues.push(
                        "Containerfile must not use 'ADD http(s)://...' for unverified remote downloads."
                            .into(),
                    );
                }
            }
            "RUN" => {
                let pipes_shell = (args_lower.contains("curl") || args_lower.contains("wget"))
                    && (args_lower.contains("| sh")
                        || args_lower.contains("|sh")
                        || args_lower.contains("| bash")
                        || args_lower.contains("|bash")
                        || args_lower.contains("| zsh"));
                if pipes_shell {
                    issues.push(
                        "Containerfile must not pipe curl/wget directly into a shell.".into(),
                    );
                }
                if args_lower.contains("docker.sock") || args_lower.contains("podman.sock") {
                    issues.push(
                        "Containerfile must not reference container engine sockets.".into(),
                    );
                }
                if args_lower.contains("--privileged") {
                    issues.push("Containerfile must not use --privileged.".into());
                }
            }
            "ENV" => {
                for token in args.split_whitespace() {
                    if let Some((k, v)) = token.split_once('=') {
                        let ku = k.to_ascii_uppercase();
                        let is_secret = ku.contains("API_KEY")
                            || ku.contains("SECRET")
                            || ku.contains("TOKEN")
                            || ku.contains("PASSWORD")
                            || ku.contains("PRIVATE_KEY");
                        let val = v.trim_matches(|c| c == '"' || c == '\'');
                        if is_secret && !val.is_empty() && !val.starts_with('$') {
                            issues.push(format!(
                                "Containerfile ENV '{k}' hardcodes a credential; declare it in runtimeVariables instead."
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if from_count == 0 {
        issues.push("Containerfile must include at least one FROM instruction.".into());
    }
    match last_user {
        None => issues.push(
            "Containerfile must end with a non-root USER instruction (for example, 'USER node')."
                .into(),
        ),
        Some(ref u) => {
            let user_part = u.split(':').next().unwrap_or("").to_ascii_lowercase();
            if user_part == "root" || user_part == "0" {
                issues.push("Containerfile final USER must be non-root (found root/0).".into());
            }
        }
    }

    issues
}

fn audit_directory_hygiene(root: &Path) -> Vec<String> {
    let mut issues = Vec::new();
    let mut has_readme = false;
    let mut has_license = false;
    let mut pending = vec![(root.to_path_buf(), 0usize)];

    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if meta.is_dir() {
                if !EXCLUDED_AUDIT_DIRS.contains(&name) && depth < 16 {
                    pending.push((path, depth + 1));
                }
                continue;
            }
            if meta.file_type().is_symlink() {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                issues.push(format!(
                    "Remove symbolic link '{}' (Pockets Hub rejects symlinks).",
                    rel.display()
                ));
                continue;
            }
            if depth == 0 {
                if lower == "readme.md" || lower == "readme" {
                    has_readme = true;
                }
                if lower == "license" || lower == "license.md" || lower == "license.txt" {
                    has_license = true;
                }
            }
            if FORBIDDEN_SECRET_FILES.contains(&lower.as_str()) {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                issues.push(format!(
                    "Remove secret file '{}' before publishing to GitHub.",
                    rel.display()
                ));
            }
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if FORBIDDEN_SECRET_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    issues.push(format!(
                        "Remove key/certificate file '{}' before publishing to GitHub.",
                        rel.display()
                    ));
                }
            }
        }
    }

    if !has_readme {
        issues.push("Add a root README.md explaining your Pocket.".into());
    }
    if !has_license {
        issues.push("Add a root LICENSE file (e.g. MIT or Apache-2.0).".into());
    }
    issues
}

fn url_encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Evaluates whether a local Pocket meets all strict Pockets Hub submission
/// requirements, optionally verifying the pushed public GitHub repository.
pub async fn check_hub_submission(
    ctx: &AppContext,
    project_id: &str,
    repo_override: Option<String>,
    verify_remote: bool,
) -> Result<HubSubmissionReport, String> {
    let store = ctx.store();
    let pid = project_id.to_owned();
    let (project, detected_repo, audit_root) = tokio::task::spawn_blocking(move || {
        let project = store.load(&pid)?;
        let linked = development::linked_folder(&store, &project.id).ok().flatten();
        let detected_repo = linked
            .as_deref()
            .filter(|dir| dir.is_dir())
            .and_then(detect_github_remote);
        let audit_root = linked
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(|| store.source_path(&project.id).unwrap_or_default());
        Ok::<_, String>((project, detected_repo, audit_root))
    })
    .await
    .map_err(|_| "Checking Hub submission readiness was interrupted.".to_owned())??;

    let raw_repo = repo_override
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| detected_repo.clone());

    let parsed_repo = match raw_repo.as_deref() {
        Some(raw) => Some(parse_github_repo(raw)),
        None => None,
    };

    let mut checks = Vec::new();

    // 1. Manifest check
    let desc_lower = project.manifest.metadata.description.trim().to_ascii_lowercase();
    let is_placeholder_desc = desc_lower.contains("__pocket_")
        || STARTER_PLACEHOLDER_DESCS
            .iter()
            .any(|p| desc_lower == *p || desc_lower.starts_with(p));
    if is_placeholder_desc {
        checks.push(SubmissionCheck {
            id: "manifest".into(),
            label: "pocket.yaml manifest".into(),
            status: "fail".into(),
            detail: "Update metadata.description in pocket.yaml from the starter placeholder."
                .into(),
        });
    } else if project.manifest.runtime.architectures.len() < 2 {
        checks.push(SubmissionCheck {
            id: "manifest".into(),
            label: "pocket.yaml manifest".into(),
            status: "warn".into(),
            detail: format!(
                "Valid manifest (v{}), currently targets only {}. Consider declaring [arm64, amd64] for Mac & Linux users.",
                project.manifest.metadata.version,
                project
                    .manifest
                    .runtime
                    .architectures
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    } else {
        checks.push(SubmissionCheck {
            id: "manifest".into(),
            label: "pocket.yaml manifest".into(),
            status: "pass".into(),
            detail: format!(
                "Valid manifest (id '{}', v{}, arm64 + amd64).",
                project.manifest.metadata.id, project.manifest.metadata.version
            ),
        });
    }

    // 2. Containerfile & hygiene checks on blocking pool
    let audit_root_clone = audit_root.clone();
    let fallback_cf = project.containerfile.clone();
    let (cf_issues, hygiene_issues) = tokio::task::spawn_blocking(move || {
        let cf_text = std::fs::read_to_string(audit_root_clone.join("Containerfile"))
            .unwrap_or(fallback_cf);
        let cf_issues = audit_containerfile(&cf_text);
        let hygiene_issues = if audit_root_clone.is_dir() {
            audit_directory_hygiene(&audit_root_clone)
        } else {
            vec!["Source folder is unavailable on disk.".into()]
        };
        (cf_issues, hygiene_issues)
    })
    .await
    .map_err(|_| "Auditing Pocket files was interrupted.".to_owned())?;

    if cf_issues.is_empty() {
        checks.push(SubmissionCheck {
            id: "containerfile".into(),
            label: "Containerfile security".into(),
            status: "pass".into(),
            detail: "Non-root USER set; no unsafe build patterns or hardcoded secrets.".into(),
        });
    } else {
        checks.push(SubmissionCheck {
            id: "containerfile".into(),
            label: "Containerfile security".into(),
            status: "fail".into(),
            detail: cf_issues.join(" "),
        });
    }

    if hygiene_issues.is_empty() {
        checks.push(SubmissionCheck {
            id: "hygiene".into(),
            label: "Repository hygiene".into(),
            status: "pass".into(),
            detail: "README.md and LICENSE present; no committed secrets or symlinks.".into(),
        });
    } else {
        checks.push(SubmissionCheck {
            id: "hygiene".into(),
            label: "Repository hygiene".into(),
            status: "fail".into(),
            detail: hygiene_issues.join(" "),
        });
    }

    // 3. GitHub repo & optional live remote verification
    let mut resolved_repo: Option<String> = None;
    let mut remote_verified = false;
    match parsed_repo {
        None => {
            checks.push(SubmissionCheck {
                id: "github".into(),
                label: "Public GitHub repository".into(),
                status: "fail".into(),
                detail: "Enter your public GitHub repository (owner/repo) after pushing this Pocket."
                    .into(),
            });
        }
        Some(Err(err)) => {
            checks.push(SubmissionCheck {
                id: "github".into(),
                label: "Public GitHub repository".into(),
                status: "fail".into(),
                detail: err,
            });
        }
        Some(Ok(repo)) => {
            let slug = repo.display_slug();
            resolved_repo = Some(slug.clone());
            if verify_remote {
                let client = http_client(HUB_FETCH_TIMEOUT)?;
                match fetch_validated_manifest(&client, &repo).await {
                    Ok(remote_manifest) => {
                        if remote_manifest.metadata.id != project.manifest.metadata.id {
                            checks.push(SubmissionCheck {
                                id: "github".into(),
                                label: "Public GitHub repository".into(),
                                status: "fail".into(),
                                detail: format!(
                                    "Remote {slug} has metadata.id '{}' which differs from local '{}'.",
                                    remote_manifest.metadata.id, project.manifest.metadata.id
                                ),
                            });
                        } else {
                            remote_verified = true;
                            checks.push(SubmissionCheck {
                                id: "github".into(),
                                label: "Public GitHub repository".into(),
                                status: "pass".into(),
                                detail: format!(
                                    "Verified public repo {slug} (pocket.yaml v{} + Containerfile).",
                                    remote_manifest.metadata.version
                                ),
                            });
                        }
                    }
                    Err(err) => {
                        checks.push(SubmissionCheck {
                            id: "github".into(),
                            label: "Public GitHub repository".into(),
                            status: "fail".into(),
                            detail: err,
                        });
                    }
                }
            } else {
                checks.push(SubmissionCheck {
                    id: "github".into(),
                    label: "Public GitHub repository".into(),
                    status: "pending".into(),
                    detail: format!(
                        "Using {slug}. Click Verify GitHub repo to check the pushed repository before opening a PR."
                    ),
                });
            }
        }
    }

    // 4. Check whether already listed in Pockets Hub
    let already_in_hub = if let Ok(hub_resp) = list_hub_pockets(ctx, false).await {
        hub_resp.pockets.iter().any(|entry| {
            resolved_repo
                .as_deref()
                .is_some_and(|slug| entry.repo.eq_ignore_ascii_case(slug))
                || entry
                    .id
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(&project.manifest.metadata.id))
                || entry
                    .existing_project_id
                    .as_deref()
                    .is_some_and(|id| id == project.id)
        })
    } else {
        false
    };

    let has_failures = checks.iter().any(|c| c.status == "fail");
    let ready_to_submit = !has_failures && resolved_repo.is_some() && !already_in_hub;

    let catalog_entry_json = resolved_repo.as_deref().map(|slug| {
        serde_json::to_string_pretty(&serde_json::json!({
            "repo": slug,
            "verified": false,
        }))
        .unwrap_or_default()
    });

    let pr_edit_url = format!("{HUB_REPO_URL}/edit/main/catalog.json");
    let title = format!("Submit Pocket: {}", project.manifest.metadata.name);
    let mut issue_submit_url = format!(
        "{HUB_REPO_URL}/issues/new?template=submit-pocket.yml&title={}",
        url_encode_query(&title)
    );
    if let Some(ref slug) = resolved_repo {
        issue_submit_url.push_str(&format!("&repo={}", url_encode_query(slug)));
    }
    issue_submit_url.push_str(&format!(
        "&summary={}",
        url_encode_query(&project.manifest.metadata.description)
    ));

    Ok(HubSubmissionReport {
        project_id: project.id,
        pocket_id: project.manifest.metadata.id,
        pocket_name: project.manifest.metadata.name,
        detected_repo,
        resolved_repo,
        already_in_hub,
        ready_to_submit,
        remote_verified,
        checks,
        catalog_entry_json,
        pr_edit_url,
        issue_submit_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_http(routes: Vec<(&'static str, u16, &'static str)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let (status, body) = routes
                        .iter()
                        .find(|(route, _, _)| *route == path)
                        .map(|(_, s, b)| (*s, *b))
                        .unwrap_or((404, "Not Found"));
                    let reason = if status == 200 { "OK" } else { "Not Found" };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    const REMOTE_CATALOG: &str = r#"{
      "version": 1,
      "pockets": [
        {
          "repo": "runpockets/coinkiwi",
          "name": "Fallback Name",
          "description": "Fallback description",
          "category": "Finance",
          "verified": true
        },
        {
          "repo": "runpockets/offline-app",
          "name": "Offline App",
          "description": "Shown from catalog when repo is unreachable.",
          "category": "Utilities",
          "verified": false
        }
      ]
    }"#;

    const COINKIWI_MANIFEST: &str = "\
apiVersion: pockets.dev/v1alpha1
kind: Pocket
metadata:
  id: coinkiwi
  name: CoinKiwi
  version: 0.4.0
  description: Live description pulled straight from CoinKiwi pocket.yaml.
runtime:
  image: localhost/pockets-coinkiwi:0.4.0
  architectures: [arm64, amd64]
web:
  port: 8080
  healthcheck: /health
storage:
  name: coinkiwi-data
  path: /data
";

    #[test]
    fn bundled_catalog_contains_coinkiwi() {
        let catalog = bundled_catalog();
        assert_eq!(catalog.version, 1);
        assert!(
            catalog
                .pockets
                .iter()
                .any(|p| p.repo == "runpockets/coinkiwi"),
            "bundled catalog must include CoinKiwi"
        );
    }

    #[tokio::test]
    async fn enriches_hub_entries_from_live_repo_manifest_and_falls_back_cleanly() {
        let base = spawn_mock_http(vec![
            ("/catalog.json", 200, REMOTE_CATALOG),
            (
                "/runpockets/coinkiwi/HEAD/pocket.yaml",
                200,
                COINKIWI_MANIFEST,
            ),
            (
                "/runpockets/coinkiwi/HEAD/Containerfile",
                200,
                "FROM node:22-alpine\n",
            ),
        ])
        .await;

        let client = http_client(Duration::from_secs(5)).unwrap();
        let hub_url = format!("{base}/catalog.json");
        let resp = build_hub_response(&client, &hub_url, Some((base.clone(), base.clone()))).await;

        assert_eq!(resp.source, "remote");
        assert_eq!(resp.pockets.len(), 2);

        let coinkiwi = &resp.pockets[0];
        assert_eq!(coinkiwi.repo, "runpockets/coinkiwi");
        assert_eq!(coinkiwi.name, "CoinKiwi");
        assert_eq!(
            coinkiwi.description,
            "Live description pulled straight from CoinKiwi pocket.yaml."
        );
        assert_eq!(coinkiwi.version.as_deref(), Some("0.4.0"));
        assert_eq!(coinkiwi.metadata_source, "repo");
        assert!(coinkiwi.verified);

        let offline = &resp.pockets[1];
        assert_eq!(offline.repo, "runpockets/offline-app");
        assert_eq!(offline.name, "Offline App");
        assert_eq!(
            offline.description,
            "Shown from catalog when repo is unreachable."
        );
        assert_eq!(offline.metadata_source, "catalog");
    }

    #[tokio::test]
    async fn falls_back_to_bundled_catalog_when_hub_is_unreachable() {
        let base = spawn_mock_http(vec![]).await;
        let client = http_client(Duration::from_secs(5)).unwrap();
        let hub_url = format!("{base}/missing.json");
        let resp = build_hub_response(&client, &hub_url, Some((base.clone(), base.clone()))).await;

        assert_eq!(resp.source, "bundled");
        assert_eq!(resp.pockets.len(), 1);
        assert_eq!(resp.pockets[0].repo, "runpockets/coinkiwi");
        assert_eq!(resp.pockets[0].name, "CoinKiwi");
    }

    #[test]
    fn parses_https_ssh_and_alias_github_remotes() {
        assert_eq!(
            parse_git_remote_github_repo("https://github.com/runpockets/coinkiwi.git")
                .unwrap()
                .display_slug(),
            "runpockets/coinkiwi"
        );
        assert_eq!(
            parse_git_remote_github_repo("git@github.com:runpockets/coinkiwi.git")
                .unwrap()
                .display_slug(),
            "runpockets/coinkiwi"
        );
        assert_eq!(
            parse_git_remote_github_repo("git@github-pockets:runpockets/coinkiwi.git")
                .unwrap()
                .display_slug(),
            "runpockets/coinkiwi"
        );
        assert!(parse_git_remote_github_repo("https://gitlab.com/org/repo.git").is_none());
    }

    #[test]
    fn containerfile_audit_enforces_non_root_and_blocks_unsafe_lines() {
        let good = "FROM node:22-alpine\nWORKDIR /app\nCOPY . .\nUSER node\nCMD [\"node\", \"server.js\"]\n";
        assert!(audit_containerfile(good).is_empty());

        let root_user = "FROM node:22-alpine\nUSER root\n";
        assert!(!audit_containerfile(root_user).is_empty());

        let curl_sh = "FROM node:22-alpine\nRUN curl -fsSL https://x.test/a.sh | sh\nUSER node\n";
        assert!(!audit_containerfile(curl_sh).is_empty());

        let secret_env = "FROM node:22-alpine\nENV API_KEY=supersecret\nUSER node\n";
        assert!(!audit_containerfile(secret_env).is_empty());
    }

    #[test]
    fn directory_hygiene_requires_readme_license_and_blocks_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let initial = audit_directory_hygiene(dir.path());
        assert!(initial.iter().any(|m| m.contains("README.md")));
        assert!(initial.iter().any(|m| m.contains("LICENSE")));

        std::fs::write(dir.path().join("README.md"), "# Demo\n").unwrap();
        std::fs::write(dir.path().join("LICENSE"), "MIT\n").unwrap();
        assert!(audit_directory_hygiene(dir.path()).is_empty());

        std::fs::write(dir.path().join(".env"), "SECRET=1\n").unwrap();
        assert!(audit_directory_hygiene(dir.path())
            .iter()
            .any(|m| m.contains(".env")));
    }
}
