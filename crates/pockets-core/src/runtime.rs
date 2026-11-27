//! Native Podman discovery, bounded command execution and detection reporting.
//!
//! Discovery looks in `PATH` plus well-known macOS install locations. Detection
//! runs read-only probe commands (`--version`, `info --format json`); it never
//! starts or modifies a machine/VM. All commands run as argument arrays without
//! a shell and are killed if they exceed [`COMMAND_TIMEOUT`].

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

/// Hard limit for every runtime command, including engine probes.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of bytes of command output surfaced to users.
const MAX_USER_MESSAGE_BYTES: usize = 2 * 1024;

const PODMAN_EXE: &str = "podman";

/// Well-known install locations checked in addition to `PATH`.
const KNOWN_LOCATIONS: [&str; 5] = [
    "/opt/homebrew/bin/podman",
    "/usr/local/bin/podman",
    "/opt/podman/bin/podman",
    "/usr/bin/podman",
    "/bin/podman",
];

/// Lifecycle state of the local Podman runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeState {
    /// The `podman` executable was not found anywhere.
    Missing,
    /// The executable exists but cannot be run, or the engine is not responding.
    Unavailable,
    /// The executable runs and the container engine answered.
    Ready,
}

/// Serializable result of a runtime detection pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionReport {
    pub state: RuntimeState,
    pub message: String,
    pub version: Option<String>,
    /// Number of virtual CPU cores available to the Podman engine (from
    /// `podman info --format json` -> `.host.cpus`), when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    /// Number of logical CPU cores on the host machine, when readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cpu_cores: Option<u32>,
}

impl DetectionReport {
    fn new(state: RuntimeState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
            version: None,
            cpu_cores: None,
            host_cpu_cores: None,
        }
    }
}

/// Last `podman` executable confirmed to have a responding engine (or chosen
/// during machine bootstrap), so multi-install setups (e.g. Homebrew Podman
/// alongside Podman Desktop) consistently route commands to the active binary.
static PREFERRED_PODMAN: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

fn preferred_podman() -> Option<PathBuf> {
    let lock = PREFERRED_PODMAN.get_or_init(|| RwLock::new(None));
    let guard = lock.read().ok()?;
    guard.clone().filter(|path| is_executable_file(path))
}

fn set_preferred_podman(path: Option<PathBuf>) {
    let lock = PREFERRED_PODMAN.get_or_init(|| RwLock::new(None));
    if let Ok(mut guard) = lock.write() {
        *guard = path;
    }
}

/// Locate the `podman` executable in `PATH` plus [`KNOWN_LOCATIONS`],
/// preferring a binary previously confirmed to be ready by [`detect_runtime`].
pub fn find_podman() -> Option<PathBuf> {
    if let Some(preferred) = preferred_podman() {
        return Some(preferred);
    }
    first_executable(&candidate_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ))
}

/// Locate all distinct executable `podman` binaries in `PATH` plus [`KNOWN_LOCATIONS`].
pub fn find_all_podman() -> Vec<PathBuf> {
    all_executables(&candidate_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ))
}

/// OS-specific Podman installation guidance when `podman` is not found.
pub fn missing_podman_message() -> String {
    #[cfg(target_os = "macos")]
    {
        "Podman was not found on this computer. Install Pockets with Homebrew (`brew install runpockets/tap/pockets`) or run `brew install podman`, then retry.".to_owned()
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Podman was not found on this computer. Install it via your package manager (e.g. `sudo apt install podman` or `sudo dnf install podman`), then retry.".to_owned()
    }
}

/// Probe a single `podman` binary without starting or modifying anything.
async fn probe_binary(bin: &Path) -> DetectionReport {
    let version = match run(bin, &["--version"]).await {
        Ok(output) => parse_version(&output.stdout),
        Err(err) => {
            return DetectionReport::new(
                RuntimeState::Unavailable,
                format!("Podman at {} could not be run. {err}", bin.display()),
            );
        }
    };

    let info_output = match run(bin, &["info", "--format", "json"]).await {
        Ok(output) => output,
        Err(err) => {
            let label = version.as_deref().unwrap_or("Podman");
            return DetectionReport {
                state: RuntimeState::Unavailable,
                message: format!(
                    "{label} is installed but the container engine is not responding. \
                     If you use a Podman machine, start it and try again. {err}"
                ),
                version,
                cpu_cores: None,
                host_cpu_cores: host_cpu_cores(),
            };
        }
    };

    let cpu_cores = parse_cpu_cores(&info_output.stdout);

    DetectionReport {
        state: RuntimeState::Ready,
        message: match &version {
            Some(v) => format!("Podman {v} is ready."),
            None => "Podman is ready.".to_owned(),
        },
        version,
        cpu_cores,
        host_cpu_cores: host_cpu_cores(),
    }
}

/// Probe all discovered executable candidates in order and select the first one
/// whose container engine responds, falling back to the first failure report.
async fn detect_from_candidates(candidates: &[PathBuf]) -> (Option<PathBuf>, DetectionReport) {
    let executables = all_executables(candidates);
    if executables.is_empty() {
        return (
            None,
            DetectionReport::new(RuntimeState::Missing, missing_podman_message()),
        );
    }

    let mut first_failure: Option<DetectionReport> = None;
    for bin in executables {
        let report = probe_binary(&bin).await;
        if report.state == RuntimeState::Ready {
            return (Some(bin), report);
        }
        if first_failure.is_none() {
            first_failure = Some(report);
        }
    }

    (
        None,
        first_failure.unwrap_or_else(|| {
            DetectionReport::new(RuntimeState::Missing, missing_podman_message())
        }),
    )
}

/// Probe the runtime without starting or modifying anything.
///
/// Checks every discovered `podman` binary (e.g. Homebrew and Podman Desktop
/// installs), runs `podman --version` and `podman info --format json`, and
/// remembers the first binary whose container engine responds.
pub async fn detect_runtime() -> DetectionReport {
    let candidates = candidate_paths(&std::env::var_os("PATH").unwrap_or_default());
    let (ready_bin, report) = detect_from_candidates(&candidates).await;
    set_preferred_podman(ready_bin);
    report
}

/// Ensures the local Podman engine is initialized and started when a Podman
/// machine VM is required (macOS), then returns a fresh detection report. On
/// Linux, Podman runs natively without a VM so this probes directly.
pub async fn bootstrap_runtime() -> DetectionReport {
    let initial = detect_runtime().await;
    if initial.state == RuntimeState::Ready || initial.state == RuntimeState::Missing {
        return initial;
    }

    #[cfg(target_os = "macos")]
    {
        let binaries = find_all_podman();
        if binaries.is_empty() {
            return initial;
        }

        // Prefer a binary that already has an initialized Podman machine so we
        // reuse an existing Podman Desktop / installer VM instead of creating a
        // duplicate one.
        let mut chosen_bin: Option<PathBuf> = None;
        let mut has_machine = false;
        for candidate in &binaries {
            let machines = run(candidate, &["machine", "list", "--format", "json"]).await;
            let candidate_has_machine = machines
                .as_ref()
                .ok()
                .and_then(|out| serde_json::from_slice::<serde_json::Value>(&out.stdout).ok())
                .and_then(|val| val.as_array().map(|arr| !arr.is_empty()))
                .unwrap_or(false);
            if candidate_has_machine {
                chosen_bin = Some(candidate.clone());
                has_machine = true;
                break;
            }
        }

        let bin = chosen_bin.unwrap_or_else(|| binaries[0].clone());
        set_preferred_podman(Some(bin.clone()));

        if !has_machine {
            // Initial machine setup may download a ~500MB VM image on first run.
            if let Err(err) = run_with_timeout(
                &bin,
                &["machine", "init"],
                Duration::from_secs(300),
            )
            .await
            {
                return DetectionReport {
                    state: RuntimeState::Unavailable,
                    message: format!("Could not initialize the Podman VM automatically. {err}"),
                    version: initial.version,
                    cpu_cores: None,
                    host_cpu_cores: host_cpu_cores(),
                };
            }
        }

        if let Err(err) = run_with_timeout(
            &bin,
            &["machine", "start"],
            Duration::from_secs(120),
        )
        .await
        {
            return DetectionReport {
                state: RuntimeState::Unavailable,
                message: format!("Could not start the Podman VM automatically. {err}"),
                version: initial.version,
                cpu_cores: None,
                host_cpu_cores: host_cpu_cores(),
            };
        }
    }

    detect_runtime().await
}

pub(crate) async fn run_with_timeout(
    bin: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, String> {
    let label = command_label(bin, args);
    let mut command = Command::new(bin);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|err| format!("{label} could not be started: {err}"))?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("{label} timed out after {} seconds.", timeout.as_secs()))?
        .map_err(|err| format!("{label} could not be started: {err}"))?;
    if !output.status.success() {
        return Err(failure_message(&label, &output));
    }
    Ok(output)
}

/// Run a runtime command with a bounded lifetime: no shell, killed on drop,
/// and aborted with an error if it exceeds [`COMMAND_TIMEOUT`].
///
/// Errors on spawn failure, timeout, or a non-zero exit status with a
/// truncated, user-safe message built from the command's stderr.
pub(crate) async fn run(bin: &Path, args: &[&str]) -> Result<Output, String> {
    let label = command_label(bin, args);

    let mut command = Command::new(bin);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let child = command
        .spawn()
        .map_err(|err| format!("{label} could not be started: {err}"))?;

    // Dropping the timeout future drops the child, which `kill_on_drop` kills.
    let output = tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "{label} timed out after {} seconds.",
                COMMAND_TIMEOUT.as_secs()
            )
        })?
        .map_err(|err| format!("{label} could not be started: {err}"))?;

    if !output.status.success() {
        return Err(failure_message(&label, &output));
    }
    Ok(output)
}

/// Run a runtime command and stream its stdout directly into `destination` via
/// a sibling temporary file with owner-only (`0600` on Unix) permissions,
/// atomically replacing `destination` only after the command succeeds. Cleans
/// up the temp file on any spawn, I/O, size-limit, timeout, or non-zero exit
/// error so a failed export never leaves a half-written backup.
pub(crate) async fn run_to_file(
    bin: &Path,
    args: &[&str],
    destination: &Path,
    timeout: Duration,
    max_bytes: Option<u64>,
) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let label = command_label(bin, args);
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "Could not determine the destination folder for the backup.".to_owned())?;
    let temp_path = parent.join(format!(
        ".pockets-export-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));

    let mut command = Command::new(bin);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|err| format!("{label} could not be started: {err}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{label} did not expose stdout."))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{label} did not expose stderr."))?;

    let mut open_opts = tokio::fs::OpenOptions::new();
    open_opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        open_opts.mode(0o600);
    }
    let mut file = open_opts
        .open(&temp_path)
        .await
        .map_err(|err| format!("Could not create the backup file: {err}"))?;

    let work = async {
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                if buf.len() < MAX_USER_MESSAGE_BYTES * 2 {
                    let take = (MAX_USER_MESSAGE_BYTES * 2 - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            buf
        });

        let copy_res: Result<u64, String> = async {
            let mut buf = vec![0u8; 64 * 1024];
            let mut written: u64 = 0;
            loop {
                let n = stdout
                    .read(&mut buf)
                    .await
                    .map_err(|err| format!("Could not read exported backup data: {err}"))?;
                if n == 0 {
                    break;
                }
                let next = written.saturating_add(n as u64);
                if let Some(limit) = max_bytes {
                    if next > limit {
                        return Err(format!(
                            "Backup archive exceeds the maximum supported size of {limit} bytes."
                        ));
                    }
                }
                file.write_all(&buf[..n])
                    .await
                    .map_err(|err| format!("Could not write the backup file: {err}"))?;
                written = next;
            }
            Ok(written)
        }
        .await;

        if let Err(err) = copy_res {
            let _ = child.kill().await;
            let _ = child.wait().await;
            err_task.abort();
            return Err(err);
        }

        let status = child
            .wait()
            .await
            .map_err(|err| format!("{label} failed while finishing: {err}"))?;
        let err_bytes = err_task.await.unwrap_or_default();
        if !status.success() {
            let output = Output {
                status,
                stdout: Vec::new(),
                stderr: err_bytes,
            };
            return Err(failure_message(&label, &output));
        }
        file.flush()
            .await
            .map_err(|err| format!("Could not flush the backup file: {err}"))?;
        file.sync_all()
            .await
            .map_err(|err| format!("Could not sync the backup file: {err}"))?;
        Ok(())
    };

    let result = match tokio::time::timeout(timeout, work).await {
        Ok(inner) => inner,
        Err(_) => Err(format!(
            "{label} timed out after {} seconds.",
            timeout.as_secs()
        )),
    };
    drop(file);

    if let Err(err) = result {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(err);
    }

    if let Err(err) = tokio::fs::rename(&temp_path, destination).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(format!("Could not save the backup file: {err}"));
    }
    Ok(())
}

/// Run a runtime command and stream `source` into its stdin, bounded by
/// `timeout`. Errors on open failure, spawn failure, timeout, or non-zero exit.
pub(crate) async fn run_from_file(
    bin: &Path,
    args: &[&str],
    source: &Path,
    timeout: Duration,
) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let label = command_label(bin, args);
    let mut file = tokio::fs::File::open(source)
        .await
        .map_err(|err| format!("Could not open the backup file: {err}"))?;

    let mut command = Command::new(bin);
    command
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|err| format!("{label} could not be started: {err}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{label} did not expose stdin."))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{label} did not expose stdout."))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{label} did not expose stderr."))?;

    let work = async {
        let write_in = async {
            let copied = tokio::io::copy(&mut file, &mut stdin).await;
            let _ = stdin.shutdown().await;
            drop(stdin);
            copied
        };
        let read_out = async {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        };
        let (write_res, out_bytes, err_bytes) = tokio::join!(write_in, read_out, read_err);
        let status = child
            .wait()
            .await
            .map_err(|err| format!("{label} failed while finishing: {err}"))?;
        if !status.success() {
            let output = Output {
                status,
                stdout: out_bytes,
                stderr: err_bytes,
            };
            return Err(failure_message(&label, &output));
        }
        write_res.map_err(|err| format!("Could not send the backup file to Podman: {err}"))?;
        Ok(())
    };

    match tokio::time::timeout(timeout, work).await {
        Ok(inner) => inner,
        Err(_) => Err(format!(
            "{label} timed out after {} seconds.",
            timeout.as_secs()
        )),
    }
}

/// Runtime commands that may handle application credentials must never echo
/// the arguments, input, or engine diagnostics on failure.
pub(crate) async fn run_private(bin: &Path, args: &[&str], input: Option<&[u8]>) -> bool {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let operation = async {
        let mut child = Command::new(bin)
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        if let Some((data, mut stdin)) = input.zip(child.stdin.take()) {
            stdin.write_all(data).await?;
        }
        child.wait().await
    };
    matches!(tokio::time::timeout(COMMAND_TIMEOUT, operation).await, Ok(Ok(status)) if status.success())
}

/// Build the ordered, de-duplicated candidate list from a raw `PATH` value.
fn candidate_paths(path_env: &OsStr) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(path_env) {
        let candidate = dir.join(PODMAN_EXE);
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    for known in KNOWN_LOCATIONS {
        let candidate = PathBuf::from(known);
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

/// First candidate that is an existing file with the executable bit set.
fn first_executable(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|path| is_executable_file(path))
        .cloned()
}

/// All distinct executable candidates in order, de-duplicated by canonical path
/// so symlinked aliases (e.g. `/usr/local/bin/podman -> /opt/podman/bin/podman`)
/// are only probed once.
fn all_executables(candidates: &[PathBuf]) -> Vec<PathBuf> {
    let mut executables = Vec::new();
    let mut seen_canonical = Vec::new();
    for path in candidates {
        if !is_executable_file(path) {
            continue;
        }
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if seen_canonical.contains(&canonical) {
            continue;
        }
        seen_canonical.push(canonical);
        executables.push(path.clone());
    }
    executables
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

/// Extract a version token ("5.2.0") from `podman --version` output.
fn parse_version(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .split_whitespace()
        .rev()
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_owned)
}

/// Extract `.host.cpus` from `podman info --format json` output.
/// Returns `None` if the JSON is unreadable or the field is missing/zero.
fn parse_cpu_cores(stdout: &[u8]) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let cpus = value.get("host")?.get("cpus")?.as_u64()?;
    u32::try_from(cpus).ok().filter(|&count| count > 0)
}

/// Logical CPU count on the host machine.
fn host_cpu_cores() -> Option<u32> {
    std::thread::available_parallelism()
        .ok()
        .and_then(|count| u32::try_from(count.get()).ok())
        .filter(|&count| count > 0)
}

fn command_label(bin: &Path, args: &[&str]) -> String {
    let name = bin
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| bin.display().to_string());
    if args.is_empty() {
        name
    } else {
        format!("{name} {}", args.join(" "))
    }
}

/// Collapse stderr to a single truncated line suitable for display.
fn failure_message(label: &str, output: &Output) -> String {
    let detail = sanitize_text(&output.stderr);
    let reason = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_owned());
    if detail.is_empty() {
        format!("{label} failed (exit {reason}).")
    } else {
        format!("{label} failed (exit {reason}): {detail}")
    }
}

fn sanitize_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= MAX_USER_MESSAGE_BYTES {
        collapsed
    } else {
        let mut end = MAX_USER_MESSAGE_BYTES - 3;
        while end > 0 && !collapsed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &collapsed[..end])
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pockets-runtime-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_fake_podman(dir: &Path, mode: u32) -> PathBuf {
        let path = dir.join(PODMAN_EXE);
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write fake podman");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .expect("chmod fake podman");
        path
    }

    #[test]
    fn candidate_paths_lists_path_dirs_then_known_locations_without_duplicates() {
        let path_env = std::env::join_paths(["/custom/bin", "/opt/homebrew/bin"]).unwrap();
        let candidates = candidate_paths(&path_env);

        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/custom/bin").join(PODMAN_EXE),
                PathBuf::from("/opt/homebrew/bin").join(PODMAN_EXE),
                PathBuf::from("/usr/local/bin/podman"),
                PathBuf::from("/opt/podman/bin/podman"),
                PathBuf::from("/usr/bin/podman"),
                PathBuf::from("/bin/podman"),
            ],
        );
    }

    #[test]
    fn resolution_skips_non_executables_and_prefers_earlier_path_dirs() {
        let shadowed = temp_dir("shadowed");
        let picked = temp_dir("picked");

        let blocked = write_fake_podman(&shadowed, 0o644); // not executable
        let chosen = write_fake_podman(&picked, 0o755);

        let path_env = std::env::join_paths([&shadowed, &picked]).unwrap();
        // Non-executable first candidate is skipped.
        assert_eq!(
            first_executable(&candidate_paths(OsStr::new(&path_env))),
            Some(chosen.clone()),
        );

        // Once executable, the earlier `PATH` directory wins deterministically.
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            first_executable(&candidate_paths(OsStr::new(&path_env))),
            Some(blocked.clone()),
        );

        let _ = std::fs::remove_dir_all(&shadowed);
        let _ = std::fs::remove_dir_all(&picked);
    }

    fn write_script_podman(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join(PODMAN_EXE);
        std::fs::write(&path, script).expect("write fake podman script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake podman script");
        path
    }

    #[test]
    fn all_executables_deduplicates_symlinks_to_same_binary() {
        let real_dir = temp_dir("symlink-real");
        let alias_dir = temp_dir("symlink-alias");
        let real_bin = write_fake_podman(&real_dir, 0o755);
        let alias_bin = alias_dir.join(PODMAN_EXE);
        std::os::unix::fs::symlink(&real_bin, &alias_bin).expect("symlink fake podman");

        let found = all_executables(&[alias_bin.clone(), real_bin.clone()]);
        assert_eq!(found, vec![alias_bin]);

        let _ = std::fs::remove_dir_all(&real_dir);
        let _ = std::fs::remove_dir_all(&alias_dir);
    }

    #[tokio::test]
    async fn detect_from_candidates_falls_back_to_ready_binary() {
        let broken_dir = temp_dir("fallback-broken");
        let healthy_dir = temp_dir("fallback-healthy");

        let broken = write_script_podman(
            &broken_dir,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'podman version 6.2.0'; exit 0; fi\necho 'Cannot connect to Podman' >&2\nexit 125\n",
        );
        let healthy = write_script_podman(
            &healthy_dir,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'podman version 5.4.1'; exit 0; fi\necho '{\"host\":{\"cpus\":6}}'\nexit 0\n",
        );

        let (selected, report) = detect_from_candidates(&[broken, healthy.clone()]).await;
        assert_eq!(selected, Some(healthy));
        assert_eq!(report.state, RuntimeState::Ready);
        assert_eq!(report.version.as_deref(), Some("5.4.1"));
        assert_eq!(report.cpu_cores, Some(6));
        assert!(report.host_cpu_cores.is_some());

        let _ = std::fs::remove_dir_all(&broken_dir);
        let _ = std::fs::remove_dir_all(&healthy_dir);
    }

    #[test]
    fn parse_cpu_cores_extracts_positive_host_cpus() {
        assert_eq!(parse_cpu_cores(br#"{"host":{"cpus":9}}"#), Some(9));
        assert_eq!(parse_cpu_cores(br#"{"host":{"cpus":1}}"#), Some(1));
        assert_eq!(parse_cpu_cores(br#"{"host":{"cpus":0}}"#), None);
        assert_eq!(parse_cpu_cores(br#"{"host":{"cpus":-2}}"#), None);
        assert_eq!(parse_cpu_cores(br#"{"host":{"cpus":"4"}}"#), None);
        assert_eq!(parse_cpu_cores(br#"{"host":{}}"#), None);
        assert_eq!(parse_cpu_cores(b"not json"), None);
    }

    #[tokio::test]
    async fn detect_from_candidates_returns_first_failure_when_none_ready() {
        let first_dir = temp_dir("all-fail-first");
        let second_dir = temp_dir("all-fail-second");

        let first = write_script_podman(
            &first_dir,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'podman version 6.2.0'; exit 0; fi\necho 'first machine stopped' >&2\nexit 125\n",
        );
        let second = write_script_podman(
            &second_dir,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'podman version 5.4.1'; exit 0; fi\necho 'second machine stopped' >&2\nexit 125\n",
        );

        let (selected, report) = detect_from_candidates(&[first, second]).await;
        assert_eq!(selected, None);
        assert_eq!(report.state, RuntimeState::Unavailable);
        assert_eq!(report.version.as_deref(), Some("6.2.0"));
        assert!(report.message.contains("first machine stopped"));

        let _ = std::fs::remove_dir_all(&first_dir);
        let _ = std::fs::remove_dir_all(&second_dir);
    }
}
