//! Bounded Podman image builds with observable progress.
//!
//! A build runs `podman build` as an argument array without a shell, with a
//! fixed, security-pinned flag set (no build args, volumes, secrets, host
//! networking or output exports) so only managed tags and the caller's own
//! context directory ever reach Podman. Output is streamed in fixed-size
//! reads into the last [`MAX_LOG_BYTES`] of the shared [`BuildProgress`], the
//! child is killed if it exceeds [`BUILD_TIMEOUT`] or this future is dropped,
//! and success is confirmed by reading back a `sha256:<64 hex>` image ID from
//! the `--iidfile` rather than by parsing build logs.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Hard limit for a whole build: spawn, output streaming, exit and ID read.
const BUILD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Maximum bytes of combined build output retained in the progress log.
const MAX_LOG_BYTES: usize = 64 * 1024;

/// Fixed-size read buffer per stream; output is never buffered unbounded.
const READ_CHUNK_BYTES: usize = 8 * 1024;

/// File `podman build --iidfile` writes the built image ID to.
const IID_FILE_NAME: &str = "image-id.txt";

/// Read-back cap for the image ID file; the ID itself is 71 bytes.
const MAX_IID_FILE_BYTES: u64 = 256;

/// Maximum bytes of collapsed log tail quoted in a failure message.
const MAX_FAILURE_DETAIL_BYTES: usize = 2 * 1024;

/// Lifecycle state of a managed image build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildState {
    /// Created but not started.
    Idle,
    /// The `podman build` child process is running.
    Running,
    /// The child exited successfully and its image ID was verified.
    Succeeded,
    /// Validation, spawn, timeout, exit-status or image-ID failure.
    Failed,
}

/// Serializable build state plus a rolling tail of Podman output.
///
/// `log` holds at most the last [`MAX_LOG_BYTES`] of combined stdout/stderr;
/// `truncated` records whether anything older was dropped.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildProgress {
    pub state: BuildState,
    pub log: String,
    pub truncated: bool,
    pub error: Option<String>,
}

impl Default for BuildProgress {
    fn default() -> Self {
        Self {
            state: BuildState::Idle,
            log: String::new(),
            truncated: false,
            error: None,
        }
    }
}

/// Cheaply clonable handle onto a shared [`BuildProgress`].
///
/// The build task writes to it while running; readers call [`snapshot`] to
/// observe progress without ever holding the lock across an await.
///
/// [`snapshot`]: BuildProgressHandle::snapshot
#[derive(Clone, Default)]
pub struct BuildProgressHandle {
    inner: Arc<Mutex<BuildProgress>>,
}

impl BuildProgressHandle {
    /// A fresh handle in the [`BuildState::Idle`] state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current state, suitable for serializing to the UI.
    pub fn snapshot(&self) -> BuildProgress {
        self.lock().clone()
    }

    /// Records a failure that happened outside [`build_image`] — a
    /// service guard, a staging problem, or a record-save error — so
    /// readers observe [`BuildState::Failed`] with the reason instead of
    /// a stale state. The command wrapper calls this for every service
    /// error; failures already recorded by [`build_image`] carry this
    /// same message, so repeating the call is coherent either way.
    pub(crate) fn fail(&self, message: &str) {
        self.update(|p| {
            p.state = BuildState::Failed;
            p.error = Some(message.to_owned());
        });
    }

    fn update(&self, edit: impl FnOnce(&mut BuildProgress)) {
        let mut guard = self.lock();
        edit(&mut guard);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BuildProgress> {
        // The critical sections are pure field writes; a panic elsewhere must
        // not make the progress permanently unobservable.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Runs a managed `podman build` for `project_id` and returns the verified
/// image ID (`sha256:<64 lowercase hex>`).
///
/// `context` must be an absolute real directory containing a regular
/// (non-symlink) `Containerfile`; `output_dir` must be an existing private
/// (owner-only) directory provided by the caller where `image-id.txt` is
/// written. Progress transitions through [`BuildState::Running`] with a live
/// log tail and ends in [`BuildState::Succeeded`] or
/// [`BuildState::Failed`]. The child never outlives this future: it is killed
/// on drop and when [`BUILD_TIMEOUT`] elapses.
pub async fn build_image(
    bin: &Path,
    context: &Path,
    output_dir: &Path,
    project_id: &str,
    progress: &BuildProgressHandle,
) -> Result<String, String> {
    // Start every build from a clean log so a reused handle cannot show a
    // previous build's output.
    progress.update(|p| {
        p.log.clear();
        p.truncated = false;
        p.error = None;
    });

    let outcome = run_build(bin, context, output_dir, project_id, progress).await;
    match outcome {
        Ok(image_id) => {
            progress.update(|p| {
                p.state = BuildState::Succeeded;
                p.error = None;
            });
            Ok(image_id)
        }
        Err(message) => {
            progress.update(|p| {
                p.state = BuildState::Failed;
                p.error = Some(message.clone());
            });
            Err(message)
        }
    }
}

async fn run_build(
    bin: &Path,
    context: &Path,
    output_dir: &Path,
    project_id: &str,
    progress: &BuildProgressHandle,
) -> Result<String, String> {
    validate_project_id(project_id)?;
    let tag = managed_tag(project_id);
    let containerfile = validate_context(context)?;
    let iidfile = validate_output_dir(output_dir)?;
    let args = build_args(context, &containerfile, &tag, &iidfile);

    let mut command = Command::new(bin);
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|err| format!("Podman build could not be started: {err}"))?;
    progress.update(|p| p.state = BuildState::Running);

    // Dropping the timeout future drops the child, which `kill_on_drop` kills,
    // so a timed-out build never leaves an orphan `podman build` running.
    let status = tokio::time::timeout(BUILD_TIMEOUT, stream_output(&mut child, progress))
        .await
        .map_err(|_| {
            format!(
                "Podman build timed out after {} seconds and was stopped.",
                BUILD_TIMEOUT.as_secs()
            )
        })?
        .map_err(|err| format!("Podman build could not be run: {err}"))?;

    if !status.success() {
        return Err(failure_message(status, &progress.snapshot().log));
    }
    read_image_id(&iidfile)
}

/// Stream stdout and stderr in fixed-size reads into the shared progress log
/// until both close, then reap the child.
async fn stream_output(
    child: &mut tokio::process::Child,
    progress: &BuildProgressHandle,
) -> Result<ExitStatus, std::io::Error> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("stdout was not piped"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("stderr was not piped"))?;

    let mut log = BoundedLog::default();
    let mut out_buf = [0u8; READ_CHUNK_BYTES];
    let mut err_buf = [0u8; READ_CHUNK_BYTES];
    let (mut out_open, mut err_open) = (true, true);

    // At least one branch is always enabled while the loop runs, and each
    // `read` is cancellation-safe, so switching between streams loses nothing.
    while out_open || err_open {
        tokio::select! {
            result = stdout.read(&mut out_buf), if out_open => {
                match result {
                    Ok(0) => out_open = false,
                    Ok(n) => record(&mut log, &out_buf[..n], progress),
                    Err(_) => out_open = false,
                }
            }
            result = stderr.read(&mut err_buf), if err_open => {
                match result {
                    Ok(0) => err_open = false,
                    Ok(n) => record(&mut log, &err_buf[..n], progress),
                    Err(_) => err_open = false,
                }
            }
        }
    }

    child.wait().await
}

fn record(log: &mut BoundedLog, chunk: &[u8], progress: &BuildProgressHandle) {
    log.extend(chunk);
    let text = log.text();
    let truncated = log.truncated;
    progress.update(|p| {
        p.log = text;
        p.truncated = truncated;
    });
}

fn validate_project_id(project_id: &str) -> Result<(), String> {
    if project_id.len() == 32 && is_lower_hex(project_id) {
        Ok(())
    } else {
        Err("Project ID must be exactly 32 lowercase hexadecimal characters.".to_owned())
    }
}

/// Registry name pinned to `localhost/` so a managed build can never name or
/// overwrite an image the user pulled from elsewhere.
fn managed_tag(project_id: &str) -> String {
    format!("localhost/pockets-managed-{project_id}:build")
}

/// Rejects relative paths, symlinked contexts and non-regular Containerfiles;
/// returns the fixed `<context>/Containerfile` path.
fn validate_context(context: &Path) -> Result<std::path::PathBuf, String> {
    if !context.is_absolute() {
        return Err("Build context must be an absolute path.".to_owned());
    }
    let meta = std::fs::symlink_metadata(context)
        .map_err(|err| format!("Build context could not be inspected: {err}"))?;
    if meta.file_type().is_symlink() {
        return Err("Build context must not be a symbolic link.".to_owned());
    }
    if !meta.is_dir() {
        return Err("Build context is not a directory.".to_owned());
    }

    let containerfile = context.join("Containerfile");
    let cf_meta = std::fs::symlink_metadata(&containerfile)
        .map_err(|_| format!("No Containerfile was found in {}.", context.display()))?;
    if cf_meta.file_type().is_symlink() {
        return Err("The Containerfile must not be a symbolic link.".to_owned());
    }
    if !cf_meta.is_file() {
        return Err("The Containerfile is not a regular file.".to_owned());
    }
    Ok(containerfile)
}

/// Requires an existing absolute, non-symlink, owner-only directory provided
/// by the native caller; returns the image ID file path inside it.
fn validate_output_dir(output_dir: &Path) -> Result<std::path::PathBuf, String> {
    if !output_dir.is_absolute() {
        return Err("Build output directory must be an absolute path.".to_owned());
    }
    let meta = std::fs::symlink_metadata(output_dir)
        .map_err(|err| format!("Build output directory could not be inspected: {err}"))?;
    if meta.file_type().is_symlink() {
        return Err("Build output directory must not be a symbolic link.".to_owned());
    }
    if !meta.is_dir() {
        return Err("Build output directory is not a directory.".to_owned());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(
                "Build output directory must be private (no group or other access).".to_owned(),
            );
        }
    }

    let iidfile = output_dir.join(IID_FILE_NAME);
    if let Ok(existing) = std::fs::symlink_metadata(&iidfile) {
        if existing.file_type().is_symlink() || !existing.is_file() {
            return Err(format!(
                "{} must be absent or a regular file.",
                iidfile.display()
            ));
        }
    }
    Ok(iidfile)
}

/// The fixed argument array for every managed build: security flags are
/// hardcoded, paths and the managed tag are the only variables, and nothing
/// user-controlled can append flags (no shell, no build args, no mounts).
fn build_args(context: &Path, containerfile: &Path, tag: &str, iidfile: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "build".into(),
        // Only fetch base images that are genuinely absent; never re-pull.
        "--pull=missing".into(),
        // Never inherit host proxy settings into the build.
        "--http-proxy=false".into(),
        // Fresh private namespace per build; no host networking.
        "--network=private".into(),
        // One stage at a time keeps the log and resource use predictable.
        "--jobs=1".into(),
        "--file".into(),
        containerfile.as_os_str().to_os_string(),
        "--tag".into(),
        tag.into(),
        "--iidfile".into(),
        iidfile.as_os_str().to_os_string(),
    ];
    args.push(context.as_os_str().to_os_string());
    args
}

/// Read back and strictly validate the image ID Podman wrote to the iidfile.
fn read_image_id(iidfile: &Path) -> Result<String, String> {
    let file = std::fs::File::open(iidfile).map_err(|err| {
        format!("The build finished but its image ID file could not be read: {err}")
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_IID_FILE_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|err| {
            format!("The build finished but its image ID file could not be read: {err}")
        })?;

    let value = String::from_utf8_lossy(&bytes).trim().to_owned();
    let valid = match value.strip_prefix("sha256:") {
        Some(hex) => hex.len() == 64 && is_lower_hex(hex),
        None => false,
    };
    if !valid {
        return Err(format!(
            "The build finished but did not produce a usable image ID in {}.",
            iidfile.display()
        ));
    }
    Ok(value)
}

fn failure_message(status: ExitStatus, log: &str) -> String {
    let reason = status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_owned());
    let detail = collapse_tail(log);
    if detail.is_empty() {
        format!("Podman build failed (exit {reason}).")
    } else {
        format!("Podman build failed (exit {reason}): {detail}")
    }
}

/// Collapse the log tail to one line, keeping the end where the failure is.
fn collapse_tail(log: &str) -> String {
    let collapsed = log.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= MAX_FAILURE_DETAIL_BYTES {
        return collapsed;
    }
    let mut start = collapsed.len() - (MAX_FAILURE_DETAIL_BYTES - 3);
    while start < collapsed.len() && !collapsed.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &collapsed[start..])
}

fn is_lower_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Byte log that retains only its last [`MAX_LOG_BYTES`].
#[derive(Default)]
pub(crate) struct BoundedLog {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedLog {
    pub(crate) fn extend(&mut self, chunk: &[u8]) {
        let total = self.bytes.len() + chunk.len();
        if total > MAX_LOG_BYTES {
            self.truncated = true;
            let drop_count = total - MAX_LOG_BYTES;
            let existing = self.bytes.len();
            if drop_count >= existing {
                self.bytes.clear();
                self.bytes
                    .extend_from_slice(&chunk[drop_count - existing..]);
            } else {
                self.bytes.drain(..drop_count);
                self.bytes.extend_from_slice(chunk);
            }
        } else {
            self.bytes.extend_from_slice(chunk);
        }
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PROJECT_ID: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f9";

    #[test]
    fn build_args_pin_security_surface_tag_and_paths() {
        assert!(validate_project_id(PROJECT_ID).is_ok());
        assert!(validate_project_id("0A1b2c3d4e5f60718293a4b5c6d7e8f9").is_err());
        assert!(validate_project_id("deadbeef").is_err());

        let args = build_args(
            &PathBuf::from("/Users/tester/ctx"),
            &PathBuf::from("/Users/tester/ctx/Containerfile"),
            &managed_tag(PROJECT_ID),
            &PathBuf::from("/Users/tester/Library/Private/image-id.txt"),
        );

        assert_eq!(
            args,
            vec![
                OsString::from("build"),
                OsString::from("--pull=missing"),
                OsString::from("--http-proxy=false"),
                OsString::from("--network=private"),
                OsString::from("--jobs=1"),
                OsString::from("--file"),
                OsString::from("/Users/tester/ctx/Containerfile"),
                OsString::from("--tag"),
                OsString::from(format!("localhost/pockets-managed-{PROJECT_ID}:build")),
                OsString::from("--iidfile"),
                OsString::from("/Users/tester/Library/Private/image-id.txt"),
                OsString::from("/Users/tester/ctx"),
            ],
        );

        // The exact array above is the whole security surface; assert the
        // dangerous extension points can never appear as extra arguments.
        for forbidden in ["--build-arg", "--volume", "--secret", "--ssh", "--output"] {
            assert!(!args.iter().any(|arg| arg == forbidden));
        }
    }

    #[test]
    fn bounded_log_keeps_last_64kib_and_flags_truncation() {
        let mut log = BoundedLog::default();
        log.extend(b"step 1/3 ");
        log.extend(b"FROM alpine\n");
        assert_eq!(log.text(), "step 1/3 FROM alpine\n");
        assert!(!log.truncated);

        // Straddling write: the oldest bytes drop and the flag flips.
        let mut log = BoundedLog::default();
        log.extend(&vec![b'a'; MAX_LOG_BYTES - 4]);
        log.extend(&vec![b'b'; 10]);
        assert!(log.truncated);
        assert_eq!(log.bytes.len(), MAX_LOG_BYTES);
        assert!(log.text().starts_with('a'));
        assert!(log.text().ends_with('b'));

        // A single oversized chunk keeps only its own tail.
        let mut log = BoundedLog::default();
        log.extend(&vec![b'x'; MAX_LOG_BYTES * 2]);
        assert!(log.truncated);
        assert_eq!(log.bytes.len(), MAX_LOG_BYTES);
        assert_eq!(log.text(), "x".repeat(MAX_LOG_BYTES));
    }
}
