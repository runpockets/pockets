//! Self-update check against GitHub Releases and safe local upgrade for Pockets.
//!
//! Checking for updates queries `https://api.github.com/repos/runpockets/pockets/releases/latest`
//! with a 30-minute in-memory cache so normal browsing never hammers GitHub's
//! rate limit and offline machines still see their local version cleanly.
//!
//! Applying an update respects the user's install method:
//! - Homebrew installs run `brew update && brew upgrade [--ignore-dependencies] pockets`
//!   (automatically preserving `--ignore-dependencies` when Homebrew's `podman`
//!   is not installed so Podman Desktop setups are never disturbed) and restart
//!   the background service.
//! - Standalone Unix binary installs (e.g. `packaging/install.sh`) download the
//!   matching release archive + `SHA256SUMS.txt`, verify SHA-256 before
//!   extracting, atomically replace the running binary, and restart the service.
//! - Development builds (`target/debug` or `target/release`) report updates as
//!   informational only and never overwrite a Cargo workspace binary.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::APP_VERSION;

const GITHUB_RELEASES_LATEST_URL: &str =
    "https://api.github.com/repos/runpockets/pockets/releases/latest";
const GITHUB_RELEASE_DOWNLOAD_BASE: &str =
    "https://github.com/runpockets/pockets/releases/download";
const CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RELEASE_NOTES: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallMethod {
    Homebrew {
        prefix: PathBuf,
        has_brew_podman: bool,
    },
    Binary {
        exe_path: PathBuf,
    },
    Development,
}

impl InstallMethod {
    fn label(&self) -> &'static str {
        match self {
            Self::Homebrew { .. } => "homebrew",
            Self::Binary { .. } => "binary",
            Self::Development => "development",
        }
    }

    fn can_auto_update(&self) -> bool {
        match self {
            Self::Homebrew { .. } => true,
            Self::Binary { .. } => release_target_triple().is_some(),
            Self::Development => false,
        }
    }

    fn manual_command(&self) -> String {
        match self {
            Self::Homebrew {
                has_brew_podman: true,
                ..
            } => "brew update && brew upgrade pockets && brew services restart pockets".to_owned(),
            Self::Homebrew {
                has_brew_podman: false,
                ..
            } => "brew update && brew upgrade --ignore-dependencies pockets && brew services restart pockets".to_owned(),
            Self::Binary { .. } => {
                #[cfg(windows)]
                {
                    "Download the latest zip from https://github.com/runpockets/pockets/releases/latest".to_owned()
                }
                #[cfg(not(windows))]
                {
                    "curl -fsSL https://raw.githubusercontent.com/runpockets/pockets/main/packaging/install.sh | sh".to_owned()
                }
            }
            Self::Development => {
                "git pull && npm run build:web && cargo build -p pockets-cli".to_owned()
            }
        }
    }
}

/// Serializable status returned to the Web UI for the footer version indicator
/// and update modal.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateStatus {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub notes: Vec<String>,
    pub published_at: Option<String>,
    pub install_method: String,
    pub can_auto_update: bool,
    pub manual_command: String,
    pub error: Option<String>,
}

/// Result returned when applying an in-app update completes and triggers a
/// daemon restart.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApplyUpdateResult {
    pub updated_to: String,
    pub restarting: bool,
    pub message: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: Option<String>,
    body: Option<String>,
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

struct CachedCheck {
    checked_at: Instant,
    status: AppUpdateStatus,
}

static UPDATE_CACHE: OnceLock<tokio::sync::Mutex<Option<CachedCheck>>> = OnceLock::new();
static APPLY_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn update_cache() -> &'static tokio::sync::Mutex<Option<CachedCheck>> {
    UPDATE_CACHE.get_or_init(|| tokio::sync::Mutex::new(None))
}

fn apply_lock() -> &'static tokio::sync::Mutex<()> {
    APPLY_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn current_install_method() -> InstallMethod {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)));
    match exe {
        Some(path) => detect_install_method(&path),
        None => InstallMethod::Development,
    }
}

fn detect_install_method(exe: &Path) -> InstallMethod {
    let normalized = exe.to_string_lossy().replace('\\', "/");

    if let Some(idx) = normalized.find("/Cellar/pockets/") {
        let prefix = PathBuf::from(&normalized[..idx]);
        let has_brew_podman = prefix.join("opt").join("podman").exists();
        return InstallMethod::Homebrew {
            prefix,
            has_brew_podman,
        };
    }

    if normalized.contains("/target/debug/") || normalized.contains("/target/release/") {
        return InstallMethod::Development;
    }

    InstallMethod::Binary {
        exe_path: exe.to_path_buf(),
    }
}

/// Check GitHub Releases for a newer Pockets version.
///
/// Results are cached in memory for 30 minutes unless `force` is true.
/// Network errors never fail the command; instead they return the current local
/// version with `error` populated so the UI always remains functional offline.
pub async fn check_for_update(force: bool) -> AppUpdateStatus {
    if !force {
        let guard = update_cache().lock().await;
        if let Some(cached) = guard.as_ref() {
            if cached.checked_at.elapsed() < CACHE_TTL {
                return cached.status.clone();
            }
        }
    }

    let method = current_install_method();
    let status = match fetch_latest_release().await {
        Ok(release) => status_from_release(APP_VERSION, &method, release),
        Err(err) => AppUpdateStatus {
            current_version: APP_VERSION.to_owned(),
            latest_version: None,
            update_available: false,
            release_url: Some("https://github.com/runpockets/pockets/releases".to_owned()),
            notes: Vec::new(),
            published_at: None,
            install_method: method.label().to_owned(),
            can_auto_update: method.can_auto_update(),
            manual_command: method.manual_command(),
            error: Some(err),
        },
    };

    if status.error.is_none() {
        let mut guard = update_cache().lock().await;
        *guard = Some(CachedCheck {
            checked_at: Instant::now(),
            status: status.clone(),
        });
    }

    status
}

/// Apply the latest Pockets update in-place (via Homebrew or SHA-256 verified
/// standalone binary swap) and schedule a clean daemon restart so the browser
/// reconnects to the updated version automatically.
pub async fn apply_update() -> Result<ApplyUpdateResult, String> {
    let _guard = apply_lock()
        .try_lock()
        .map_err(|_| "An update is already in progress.".to_owned())?;

    let status = check_for_update(true).await;
    if let Some(err) = status.error {
        return Err(err);
    }
    if !status.update_available {
        return Err(format!(
            "Pockets v{} is already up to date.",
            status.current_version
        ));
    }
    let target_version = status
        .latest_version
        .ok_or_else(|| "Latest version was not reported by GitHub.".to_owned())?;

    let method = current_install_method();
    match method {
        InstallMethod::Homebrew {
            prefix,
            has_brew_podman,
        } => {
            upgrade_homebrew(&prefix, has_brew_podman).await?;
            clear_cache().await;
            schedule_restart(RestartStrategy::Homebrew { prefix });
            Ok(ApplyUpdateResult {
                updated_to: target_version.clone(),
                restarting: true,
                message: format!(
                    "Updated Pockets to v{target_version}. Restarting the daemon…"
                ),
            })
        }
        InstallMethod::Binary { exe_path } => {
            upgrade_standalone_binary(&exe_path, &target_version).await?;
            clear_cache().await;
            schedule_restart(RestartStrategy::ServiceOrExit);
            Ok(ApplyUpdateResult {
                updated_to: target_version.clone(),
                restarting: true,
                message: format!(
                    "Updated Pockets to v{target_version}. Restarting the daemon…"
                ),
            })
        }
        InstallMethod::Development => Err(format!(
            "Automatic updates are disabled for local development builds. Run: {}",
            method.manual_command()
        )),
    }
}

async fn clear_cache() {
    let mut guard = update_cache().lock().await;
    *guard = None;
}

fn http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(format!("pockets/{APP_VERSION}"))
        .build()
        .map_err(|err| format!("Could not initialize HTTPS client: {err}"))
}

async fn fetch_latest_release() -> Result<GitHubRelease, String> {
    let client = http_client(CHECK_TIMEOUT)?;
    let response = client
        .get(GITHUB_RELEASES_LATEST_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|err| format!("Could not reach GitHub Releases: {err}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "GitHub Releases returned HTTP {}.",
            response.status().as_u16()
        ));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("Could not read GitHub Releases response: {err}"))?;
    serde_json::from_slice::<GitHubRelease>(&bytes)
        .map_err(|err| format!("Could not parse GitHub Release metadata: {err}"))
}

fn status_from_release(
    current: &str,
    method: &InstallMethod,
    release: GitHubRelease,
) -> AppUpdateStatus {
    let latest_clean = release.tag_name.trim().trim_start_matches('v').to_owned();
    let update_available =
        !release.draft && !release.prerelease && is_newer_version(current, &latest_clean);
    let release_url = release.html_url.or_else(|| {
        Some(format!(
            "https://github.com/runpockets/pockets/releases/tag/{}",
            release.tag_name.trim()
        ))
    });
    let notes = extract_release_notes(release.body.as_deref().unwrap_or(""));

    AppUpdateStatus {
        current_version: current.to_owned(),
        latest_version: Some(latest_clean),
        update_available,
        release_url,
        notes,
        published_at: release.published_at,
        install_method: method.label().to_owned(),
        can_auto_update: method.can_auto_update(),
        manual_command: method.manual_command(),
        error: None,
    }
}

/// Parse a `MAJOR.MINOR.PATCH` version string (ignoring an optional leading `v`
/// and any pre-release/build metadata suffix).
pub(crate) fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim().trim_start_matches('v');
    let core = trimmed
        .split_once(['-', '+'])
        .map(|(head, _)| head)
        .unwrap_or(trimmed);
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub(crate) fn is_newer_version(current: &str, candidate: &str) -> bool {
    match (parse_semver(current), parse_semver(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        _ => false,
    }
}

/// Extract clean, user-readable highlight bullets from GitHub release notes.
pub(crate) fn extract_release_notes(body: &str) -> Vec<String> {
    let mut notes = Vec::new();
    for raw_line in body.lines() {
        let trimmed = raw_line.trim();
        let bullet = trimmed
            .strip_prefix("* ")
            .or_else(|| trimmed.strip_prefix("- "))
            .or_else(|| trimmed.strip_prefix("• "));
        let Some(content) = bullet else {
            continue;
        };
        // Strip GitHub's auto-generated " by @author in https://..." tail.
        let cleaned = match content.find(" by @") {
            Some(idx) => content[..idx].trim(),
            None => content.trim(),
        };
        let cleaned = cleaned.trim_start_matches("**").trim_end_matches("**").trim();
        if cleaned.is_empty()
            || cleaned.starts_with("Full Changelog:")
            || cleaned.starts_with("http://")
            || cleaned.starts_with("https://")
        {
            continue;
        }
        notes.push(cleaned.to_owned());
        if notes.len() >= MAX_RELEASE_NOTES {
            break;
        }
    }

    if notes.is_empty() {
        notes.push("Improvements and fixes".to_owned());
    }
    notes
}

fn find_brew(prefix: &Path) -> Option<PathBuf> {
    let candidates = [
        prefix.join("bin").join("brew"),
        PathBuf::from("/opt/homebrew/bin/brew"),
        PathBuf::from("/usr/local/bin/brew"),
        PathBuf::from("/home/linuxbrew/.linuxbrew/bin/brew"),
    ];
    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    std::env::var_os("PATH").and_then(|path_env| {
        std::env::split_paths(&path_env)
            .map(|dir| dir.join("brew"))
            .find(|p| p.is_file())
    })
}

async fn run_bounded(bin: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|err| format!("Could not run {}: {err}", bin.display()))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "{} {} timed out after {} seconds.",
                bin.display(),
                args.join(" "),
                timeout.as_secs()
            )
        })?
        .map_err(|err| format!("Failed waiting for {}: {err}", bin.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        return Err(format!(
            "`{} {}` failed: {}",
            bin.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| bin.display().to_string()),
            args.join(" "),
            if detail.is_empty() {
                output.status.to_string()
            } else {
                detail
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn upgrade_homebrew(prefix: &Path, has_brew_podman: bool) -> Result<(), String> {
    let brew = find_brew(prefix).ok_or_else(|| {
        "Could not locate `brew`. Run `brew update && brew upgrade pockets` in your terminal."
            .to_owned()
    })?;

    run_bounded(&brew, &["update"], Duration::from_secs(120)).await?;
    let upgrade_args: &[&str] = if has_brew_podman {
        &["upgrade", "runpockets/tap/pockets"]
    } else {
        &["upgrade", "--ignore-dependencies", "runpockets/tap/pockets"]
    };
    run_bounded(&brew, upgrade_args, Duration::from_secs(180)).await?;
    Ok(())
}

fn release_target_triple() -> Option<&'static str> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Some("aarch64-apple-darwin")
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Some("x86_64-apple-darwin")
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Some("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Some("aarch64-unknown-linux-gnu")
    }
    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    )))]
    {
        None
    }
}

fn parse_checksum_for_asset(sums_text: &str, asset_name: &str) -> Option<String> {
    for line in sums_text.lines() {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        if name == asset_name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

async fn upgrade_standalone_binary(exe_path: &Path, target_version: &str) -> Result<(), String> {
    let triple = release_target_triple().ok_or_else(|| {
        "Automatic binary replacement is not supported on this platform yet.".to_owned()
    })?;
    let tag = format!("v{target_version}");
    let asset = format!("pockets-{tag}-{triple}.tar.gz");
    let sums_url = format!("{GITHUB_RELEASE_DOWNLOAD_BASE}/{tag}/SHA256SUMS.txt");
    let archive_url = format!("{GITHUB_RELEASE_DOWNLOAD_BASE}/{tag}/{asset}");

    let client = http_client(DOWNLOAD_TIMEOUT)?;

    let sums_resp = client
        .get(&sums_url)
        .send()
        .await
        .map_err(|err| format!("Could not download SHA256SUMS.txt: {err}"))?;
    if !sums_resp.status().is_success() {
        return Err(format!(
            "SHA256SUMS.txt download failed (HTTP {}).",
            sums_resp.status().as_u16()
        ));
    }
    let sums_text = sums_resp
        .text()
        .await
        .map_err(|err| format!("Could not read SHA256SUMS.txt: {err}"))?;
    let expected_sha = parse_checksum_for_asset(&sums_text, &asset)
        .ok_or_else(|| format!("SHA256SUMS.txt did not include a checksum for {asset}."))?;

    let archive_resp = client
        .get(&archive_url)
        .send()
        .await
        .map_err(|err| format!("Could not download {asset}: {err}"))?;
    if !archive_resp.status().is_success() {
        return Err(format!(
            "Release archive download failed (HTTP {}).",
            archive_resp.status().as_u16()
        ));
    }
    let archive_bytes = archive_resp
        .bytes()
        .await
        .map_err(|err| format!("Could not read release archive: {err}"))?;

    let mut hasher = Sha256::new();
    hasher.update(&archive_bytes);
    let actual_sha = format!("{:x}", hasher.finalize());
    if actual_sha != expected_sha {
        return Err(format!(
            "Checksum mismatch for {asset} (expected {expected_sha}, got {actual_sha})."
        ));
    }

    let parent = exe_path
        .parent()
        .ok_or_else(|| "Could not resolve executable directory.".to_owned())?;
    let staging = parent.join(format!(".pockets-update-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&staging)
        .map_err(|err| format!("Could not create staging directory (check permissions on {}): {err}", parent.display()))?;

    let result = async {
        let archive_path = staging.join(&asset);
        tokio::fs::write(&archive_path, &archive_bytes)
            .await
            .map_err(|err| format!("Could not write downloaded archive: {err}"))?;

        let tar_bin = if Path::new("/usr/bin/tar").is_file() {
            Path::new("/usr/bin/tar")
        } else {
            Path::new("tar")
        };
        let archive_str = archive_path.to_string_lossy().into_owned();
        let staging_str = staging.to_string_lossy().into_owned();
        run_bounded(
            tar_bin,
            &["-xzf", &archive_str, "-C", &staging_str, "pockets"],
            Duration::from_secs(30),
        )
        .await?;

        let extracted = staging.join("pockets");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o755))
                .map_err(|err| format!("Could not mark updated binary executable: {err}"))?;
        }
        std::fs::rename(&extracted, exe_path).map_err(|err| {
            format!(
                "Could not replace {} (check write permissions): {err}",
                exe_path.display()
            )
        })?;
        Ok(())
    }
    .await;

    let _ = std::fs::remove_dir_all(&staging);
    result
}

enum RestartStrategy {
    Homebrew { prefix: PathBuf },
    ServiceOrExit,
}

fn schedule_restart(strategy: RestartStrategy) {
    tokio::spawn(async move {
        // Give the HTTP response a moment to flush cleanly to the browser.
        tokio::time::sleep(Duration::from_millis(500)).await;
        match strategy {
            RestartStrategy::Homebrew { prefix } => {
                if let Some(brew) = find_brew(&prefix) {
                    let _ = tokio::process::Command::new(brew)
                        .args(["services", "restart", "pockets"])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                    // Wait briefly for launchd to stop us; if running in
                    // foreground instead of under brew services, exit cleanly.
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                std::process::exit(0);
            }
            RestartStrategy::ServiceOrExit => {
                #[cfg(all(unix, not(target_os = "macos")))]
                {
                    let _ = tokio::process::Command::new("systemctl")
                        .args(["--user", "restart", "pockets.service"])
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                std::process::exit(0);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parsing_and_comparison_work() {
        assert_eq!(parse_semver("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver("v0.1.1"), Some((0, 1, 1)));
        assert_eq!(parse_semver("v1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("invalid"), None);

        assert!(is_newer_version("0.1.0", "0.1.1"));
        assert!(is_newer_version("0.1.9", "0.2.0"));
        assert!(is_newer_version("0.9.9", "1.0.0"));
        assert!(!is_newer_version("0.1.1", "0.1.1"));
        assert!(!is_newer_version("0.2.0", "0.1.9"));
    }

    #[test]
    fn extracts_clean_release_note_bullets() {
        let body = "## What's Changed\n\
            * Add Pockets brand logo by @thetoasteth in https://github.com/runpockets/pockets/pull/1\n\
            - Prefer active Podman binary across installs\n\
            **Full Changelog**: https://github.com/runpockets/pockets/compare/v0.1.0...v0.1.1\n";
        assert_eq!(
            extract_release_notes(body),
            vec![
                "Add Pockets brand logo".to_owned(),
                "Prefer active Podman binary across installs".to_owned(),
            ]
        );
        assert_eq!(
            extract_release_notes(""),
            vec!["Improvements and fixes".to_owned()]
        );
    }

    #[test]
    fn detects_homebrew_dev_and_binary_installs() {
        assert_eq!(
            detect_install_method(Path::new(
                "/opt/homebrew/Cellar/pockets/0.1.0/bin/pockets"
            )),
            InstallMethod::Homebrew {
                prefix: PathBuf::from("/opt/homebrew"),
                has_brew_podman: Path::new("/opt/homebrew/opt/podman").exists(),
            }
        );
        assert_eq!(
            detect_install_method(Path::new("/tmp/pockets/target/debug/pockets")),
            InstallMethod::Development
        );
        assert_eq!(
            detect_install_method(Path::new("/home/user/.local/bin/pockets")),
            InstallMethod::Binary {
                exe_path: PathBuf::from("/home/user/.local/bin/pockets"),
            }
        );
    }

    #[test]
    fn parses_sha256sums_lines_strictly() {
        let sums = "\
e2f1dce03c197dec075d92db60f91d53cfea51aa8c535f618b1dfe8816116280  pockets-v0.1.0-aarch64-apple-darwin.tar.gz\n\
2ef4217bfb41a38c1f8f9bf36b8aac3106f6c382b0cfa85a4e7598332211bbe5  pockets-v0.1.0-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            parse_checksum_for_asset(sums, "pockets-v0.1.0-aarch64-apple-darwin.tar.gz").as_deref(),
            Some("e2f1dce03c197dec075d92db60f91d53cfea51aa8c535f618b1dfe8816116280")
        );
        assert_eq!(parse_checksum_for_asset(sums, "missing.tar.gz"), None);
    }
}
