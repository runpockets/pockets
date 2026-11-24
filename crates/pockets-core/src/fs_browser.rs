//! Safe, read-only local directory browser for the Web UI folder picker.
//!
//! Returns directories under a requested path (defaulting to the user's home
//! directory) so users can select a parent folder or Pocket source folder in
//! the browser without needing a native OS desktop file dialog.

use std::path::{Path, PathBuf};

use serde::Serialize;

const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
    pub has_pocket_yaml: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryListing {
    pub current_path: String,
    pub parent_path: Option<String>,
    pub has_pocket_yaml: bool,
    pub entries: Vec<DirectoryEntry>,
    pub truncated: bool,
}

/// Resolves the user's home directory without external crates.
pub fn user_home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    }
}

/// Resolves the default Pockets application data directory for the host OS.
///
/// Respects `POCKETS_DATA_DIR` if explicitly set.
/// - macOS: `~/Library/Application Support/dev.pockets.desktop` (preserves
///   existing installations seamlessly)
/// - Linux / Unix: `$XDG_DATA_HOME/pockets` or `~/.local/share/pockets`
/// - Windows: `%LOCALAPPDATA%\pockets`
pub fn default_data_dir() -> Result<PathBuf, String> {
    if let Some(override_dir) = std::env::var_os("POCKETS_DATA_DIR") {
        let path = PathBuf::from(override_dir);
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let home = user_home_dir()
            .ok_or_else(|| "Could not determine HOME directory for Pockets data.".to_owned())?;
        Ok(home
            .join("Library")
            .join("Application Support")
            .join("dev.pockets.desktop"))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            let path = PathBuf::from(xdg);
            if !path.as_os_str().is_empty() {
                return Ok(path.join("pockets"));
            }
        }
        let home = user_home_dir()
            .ok_or_else(|| "Could not determine HOME directory for Pockets data.".to_owned())?;
        Ok(home.join(".local").join("share").join("pockets"))
    }

    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let path = PathBuf::from(local);
            if !path.as_os_str().is_empty() {
                return Ok(path.join("pockets"));
            }
        }
        let home = user_home_dir().ok_or_else(|| {
            "Could not determine USERPROFILE directory for Pockets data.".to_owned()
        })?;
        Ok(home.join("AppData").join("Local").join("pockets"))
    }
}

/// Lists subdirectories inside `path` (or the user's home directory when `None`).
pub fn browse_directory(
    path: Option<&str>,
    show_hidden: bool,
) -> Result<DirectoryListing, String> {
    let target = match path.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => expand_tilde(raw),
        None => user_home_dir()
            .ok_or_else(|| "Could not determine the user home directory.".to_owned())?,
    };

    let canonical = std::fs::canonicalize(&target)
        .map_err(|err| format!("Folder '{}' could not be opened: {err}", target.display()))?;
    if !canonical.is_dir() {
        return Err(format!("'{}' is not a folder.", canonical.display()));
    }

    let read_dir = std::fs::read_dir(&canonical)
        .map_err(|err| format!("Could not read '{}': {err}", canonical.display()))?;

    let mut entries = Vec::new();
    let mut truncated = false;

    for item in read_dir {
        let Ok(entry) = item else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let entry_path = entry.path();
        let is_dir = std::fs::metadata(&entry_path)
            .map(|meta| meta.is_dir())
            .unwrap_or(false);
        if !is_dir {
            continue;
        }
        let has_pocket_yaml = entry_path.join("pocket.yaml").is_file();
        entries.push(DirectoryEntry {
            name,
            path: entry_path.to_string_lossy().into_owned(),
            has_pocket_yaml,
        });
    }

    entries.sort_by(|a, b| {
        b.has_pocket_yaml
            .cmp(&a.has_pocket_yaml)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    if entries.len() > MAX_ENTRIES {
        entries.truncate(MAX_ENTRIES);
        truncated = true;
    }

    let parent_path = canonical
        .parent()
        .filter(|parent| *parent != canonical.as_path())
        .map(|parent| parent.to_string_lossy().into_owned());

    Ok(DirectoryListing {
        current_path: canonical.to_string_lossy().into_owned(),
        parent_path,
        has_pocket_yaml: Path::new(&canonical).join("pocket.yaml").is_file(),
        entries,
        truncated,
    })
}

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = user_home_dir() {
            return home;
        }
    } else if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        if let Some(home) = user_home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_subdirectories_and_flags_pocket_yaml() {
        let temp = tempfile::tempdir().unwrap();
        let app_dir = temp.path().join("my-pocket");
        let other_dir = temp.path().join("alpha");
        let hidden_dir = temp.path().join(".git");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::create_dir_all(&hidden_dir).unwrap();
        std::fs::write(app_dir.join("pocket.yaml"), "kind: Pocket\n").unwrap();
        std::fs::write(temp.path().join("note.txt"), "hi").unwrap();

        let listing = browse_directory(Some(temp.path().to_str().unwrap()), false).unwrap();
        assert_eq!(listing.entries.len(), 2);
        // Pockets directories sort first.
        assert_eq!(listing.entries[0].name, "my-pocket");
        assert!(listing.entries[0].has_pocket_yaml);
        assert_eq!(listing.entries[1].name, "alpha");
        assert!(!listing.entries[1].has_pocket_yaml);

        let with_hidden = browse_directory(Some(temp.path().to_str().unwrap()), true).unwrap();
        assert_eq!(with_hidden.entries.len(), 3);
    }
}
