//! Native Podman discovery, bounded command execution and detection reporting.
//!
//! Discovery looks in `PATH` plus well-known macOS install locations. Detection
//! runs read-only probe commands (`--version`, `info --format json`); it never
//! starts or modifies a machine/VM. All commands run as argument arrays without
//! a shell and are killed if they exceed [`COMMAND_TIMEOUT`].

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;

/// Hard limit for every runtime command, including engine probes.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of bytes of command output surfaced to users.
const MAX_USER_MESSAGE_BYTES: usize = 2 * 1024;

#[cfg(windows)]
const PODMAN_EXE: &str = "podman.exe";
#[cfg(not(windows))]
const PODMAN_EXE: &str = "podman";

/// Well-known install locations checked in addition to `PATH`.
#[cfg(not(windows))]
const KNOWN_LOCATIONS: [&str; 5] = [
    "/opt/homebrew/bin/podman",
    "/usr/local/bin/podman",
    "/opt/podman/bin/podman",
    "/usr/bin/podman",
    "/bin/podman",
];

#[cfg(windows)]
const KNOWN_LOCATIONS: [&str; 2] = [
    r"C:\Program Files\RedHat\Podman\podman.exe",
    r"C:\Program Files (x86)\RedHat\Podman\podman.exe",
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
}

impl DetectionReport {
    fn new(state: RuntimeState, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
            version: None,
        }
    }
}

/// Locate the `podman` executable in `PATH` plus [`KNOWN_LOCATIONS`].
pub fn find_podman() -> Option<PathBuf> {
    first_executable(&candidate_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ))
}

/// OS-specific Podman installation guidance when `podman` is not found.
pub fn missing_podman_message() -> String {
    #[cfg(target_os = "macos")]
    {
        "Podman was not found on this computer. Install Pockets with Homebrew (`brew install pockets`) or run `brew install podman`, then retry.".to_owned()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "Podman was not found on this computer. Install it via your package manager (e.g. `sudo apt install podman` or `sudo dnf install podman`), then retry.".to_owned()
    }
    #[cfg(windows)]
    {
        "Podman was not found on this computer. Install it with `winget install RedHat.Podman` or from https://podman.io, then retry.".to_owned()
    }
}

/// Probe the runtime without starting or modifying anything.
///
/// Runs `podman --version` to confirm the binary, then
/// `podman info --format json` to confirm the engine is reachable.
pub async fn detect_runtime() -> DetectionReport {
    let Some(bin) = find_podman() else {
        return DetectionReport::new(RuntimeState::Missing, missing_podman_message());
    };

    let version = match run(&bin, &["--version"]).await {
        Ok(output) => parse_version(&output.stdout),
        Err(err) => {
            return DetectionReport::new(
                RuntimeState::Unavailable,
                format!("Podman at {} could not be run. {err}", bin.display()),
            );
        }
    };

    if let Err(err) = run(&bin, &["info", "--format", "json"]).await {
        let label = version.as_deref().unwrap_or("Podman");
        return DetectionReport {
            state: RuntimeState::Unavailable,
            message: format!(
                "{label} is installed but the container engine is not responding. \
                 If you use a Podman machine, start it and try again. {err}"
            ),
            version,
        };
    }

    DetectionReport {
        state: RuntimeState::Ready,
        message: match &version {
            Some(v) => format!("Podman {v} is ready."),
            None => "Podman is ready.".to_owned(),
        },
        version,
    }
}

/// Ensures the local Podman engine is initialized and started when a Podman
/// machine VM is required (macOS / Windows), then returns a fresh detection
/// report. On Linux, Podman runs natively without a VM so this probes directly.
pub async fn bootstrap_runtime() -> DetectionReport {
    let initial = detect_runtime().await;
    if initial.state == RuntimeState::Ready || initial.state == RuntimeState::Missing {
        return initial;
    }

    let Some(bin) = find_podman() else {
        return initial;
    };

    #[cfg(any(target_os = "macos", windows))]
    {
        // Inspect existing Podman machines without using a shell.
        let machines = run(&bin, &["machine", "list", "--format", "json"]).await;
        let has_machine = machines
            .as_ref()
            .ok()
            .and_then(|out| serde_json::from_slice::<serde_json::Value>(&out.stdout).ok())
            .and_then(|val| val.as_array().map(|arr| !arr.is_empty()))
            .unwrap_or(false);

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
            };
        }
    }

    detect_runtime().await
}

async fn run_with_timeout(
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
/// a sibling temporary file, atomically replacing `destination` only after the
/// command succeeds. Cleans up the temp file on any spawn, I/O, timeout, or
/// non-zero exit error so a failed export never leaves a half-written backup.
pub(crate) async fn run_to_file(
    bin: &Path,
    args: &[&str],
    destination: &Path,
    timeout: Duration,
) -> Result<(), String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let label = command_label(bin, args);
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "Could not determine the destination folder for the backup.".to_owned())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_path = parent.join(format!(
        ".pockets-export-{}-{stamp}.tmp",
        std::process::id()
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

    let mut file = tokio::fs::File::create(&temp_path)
        .await
        .map_err(|err| format!("Could not create the backup file: {err}"))?;

    let work = async {
        let copy = tokio::io::copy(&mut stdout, &mut file);
        let read_err = async {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        };
        let (copy_res, err_bytes) = tokio::join!(copy, read_err);
        let status = child
            .wait()
            .await
            .map_err(|err| format!("{label} failed while finishing: {err}"))?;
        if !status.success() {
            let output = Output {
                status,
                stdout: Vec::new(),
                stderr: err_bytes,
            };
            return Err(failure_message(&label, &output));
        }
        copy_res.map_err(|err| format!("Could not write the backup file: {err}"))?;
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

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Extract a version token ("5.2.0") from `podman --version` output.
fn parse_version(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .split_whitespace()
        .rev()
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_owned)
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
}
