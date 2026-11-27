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
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Resolves the default Pockets application data directory for the host OS.
///
/// Respects `POCKETS_DATA_DIR` if explicitly set.
/// - macOS: `~/Library/Application Support/dev.pockets.desktop` (preserves
///   existing installations seamlessly)
/// - Linux / Unix: `$XDG_DATA_HOME/pockets` or `~/.local/share/pockets`
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

    #[cfg(not(target_os = "macos"))]
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

    let canonical = std::fs::canonicalize(&target).map_err(|err| {
        format!(
            "Folder '{}' could not be opened: {err}",
            abbreviate_home_dir(&target)
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "'{}' is not a folder.",
            abbreviate_home_dir(&canonical)
        ));
    }

    let read_dir = std::fs::read_dir(&canonical).map_err(|err| {
        format!(
            "Could not read '{}': {err}",
            abbreviate_home_dir(&canonical)
        )
    })?;

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
            path: abbreviate_home_dir(&entry_path),
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
        .map(abbreviate_home_dir);

    Ok(DirectoryListing {
        current_path: abbreviate_home_dir(&canonical),
        parent_path,
        has_pocket_yaml: Path::new(&canonical).join("pocket.yaml").is_file(),
        entries,
        truncated,
    })
}

/// Formats `path` for user-facing display by replacing the current user's home
/// directory prefix with `~`.
pub fn abbreviate_home_dir(path: &Path) -> String {
    let Some(home) = user_home_dir() else {
        return path.to_string_lossy().into_owned();
    };
    if let Some(abbreviated) = abbreviate_home_dir_with(path, &home) {
        return abbreviated;
    }
    if let Ok(canonical_home) = std::fs::canonicalize(&home) {
        if canonical_home != home {
            if let Some(abbreviated) = abbreviate_home_dir_with(path, &canonical_home) {
                return abbreviated;
            }
        }
    }
    path.to_string_lossy().into_owned()
}

fn abbreviate_home_dir_with(path: &Path, home: &Path) -> Option<String> {
    let rest = path.strip_prefix(home).ok()?;
    if rest.as_os_str().is_empty() {
        Some("~".to_owned())
    } else {
        Some(format!(
            "~{}{}",
            std::path::MAIN_SEPARATOR,
            rest.to_string_lossy()
        ))
    }
}

pub fn expand_tilde(raw: &str) -> PathBuf {
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

    #[test]
    fn abbreviates_paths_under_home_directory_without_partial_prefix_matches() {
        let home = PathBuf::from("/Users/alice");
        let sep = std::path::MAIN_SEPARATOR;

        assert_eq!(abbreviate_home_dir_with(&home, &home), Some("~".to_owned()));
        assert_eq!(
            abbreviate_home_dir_with(&home.join("Code").join("Pockets").join("app"), &home),
            Some(format!("~{sep}Code{sep}Pockets{sep}app"))
        );
        // Sibling directory sharing the same string prefix must not match.
        assert_eq!(
            abbreviate_home_dir_with(Path::new("/Users/alice2/Code/Pockets"), &home),
            None
        );
        // Path outside home must not match.
        assert_eq!(
            abbreviate_home_dir_with(Path::new("/Users/Shared/Code/Pockets"), &home),
            None
        );
        assert_eq!(
            abbreviate_home_dir_with(Path::new("/projects/tracker"), &home),
            None
        );
    }

    #[test]
    fn abbreviate_and_expand_roundtrip_under_real_home() {
        let Some(home) = user_home_dir() else {
            return;
        };
        let nested = home.join("Code").join("Pockets").join("demo");
        let abbreviated = abbreviate_home_dir(&nested);
        assert!(abbreviated.starts_with('~'));
        assert_eq!(expand_tilde(&abbreviated), nested);
        assert_eq!(abbreviate_home_dir(&home), "~");
        assert_eq!(expand_tilde("~"), home);
    }
}
