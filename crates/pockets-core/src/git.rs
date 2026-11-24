//! Minimal, read-mostly use of the user's own `git` for linked folders.
//!
//! Pockets never configures, fetches, pushes or rewrites history. The only
//! write is one ordinary commit when the user restores a version, made
//! with their own name and email, so everything it replaces stays in the
//! folder's history.

use std::path::{Path, PathBuf};
use std::process::Command;

const KNOWN_LOCATIONS: [&str; 3] = [
    "/opt/homebrew/bin/git",
    "/usr/local/bin/git",
    "/usr/bin/git",
];

/// The `git` executable from `PATH` or a well-known install location.
pub(crate) fn find_git() -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("git"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    from_path
        .into_iter()
        .chain(KNOWN_LOCATIONS.iter().map(PathBuf::from))
        .find(|candidate| candidate.is_file())
}

/// Runs `git -C <folder> <args>` without a terminal, pager or prompts and
/// returns its stdout. A non-zero exit is an error carrying git's message.
pub(crate) fn run(git: &Path, folder: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new(git)
        .arg("-C")
        .arg(folder)
        .args(["-c", "core.quotepath=off", "-c", "color.ui=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // Paths are file names, never pathspec patterns or magic.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("git could not be started: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if message.is_empty() {
            format!("git {} failed", args.first().copied().unwrap_or_default())
        } else {
            message
        })
    }
}

/// `run` for commands whose output is a single line of text.
pub(crate) fn run_text(git: &Path, folder: &Path, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&run(git, folder, args)?)
        .trim()
        .to_owned())
}

/// NUL-separated paths from a `-z` listing.
pub(crate) fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect()
}
