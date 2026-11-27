//! Trusted, text-only starter creation. Parents come from the native picker;
//! the frontend provides bounded display text and a single directory slug.
//! Existing destinations are never reused, overwritten, or deleted.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::manifest::{self, Architecture};
use crate::projects::{Project, Store};

include!(concat!(env!("OUT_DIR"), "/basic_pocket.rs"));

pub(crate) fn inputs(name: &str, slug: &str) -> Result<(), String> {
    if name.trim().is_empty()
        || name.chars().count() > 80
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
    {
        return Err("Choose a project name of 1–80 characters on one line.".into());
    }
    if slug.is_empty()
        || slug.len() > 63
        || !slug.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
    {
        return Err("Folder name must use lowercase letters/numbers separated by single hyphens, at most 63 characters.".into());
    }
    Ok(())
}

pub(crate) fn render(name: &str, slug: &str) -> Result<BTreeMap<String, String>, String> {
    inputs(name, slug)?;
    let arch = Architecture::from_host()?;
    let mut files = BTreeMap::new();
    for (path, source) in TEMPLATE_FILES {
        let display_name = if *path == "pocket.yaml" {
            serde_json::to_string(name.trim()).map_err(|e| e.to_string())?
        } else {
            name.trim().to_owned()
        };
        let text = source
            .replace("__POCKET_ID__", slug)
            .replace("__POCKET_NAME__", &display_name)
            .replace("__POCKET_ARCH__", arch.as_str());
        if text.contains("__POCKET_") {
            return Err(
                "The starter has an unresolved template value. Choose a different name.".into(),
            );
        }
        files.insert((*path).to_owned(), text);
    }
    let manifest = manifest::parse_manifest(
        files
            .get("pocket.yaml")
            .ok_or("Starter manifest is missing.")?,
    )?;
    manifest.validate_host()?;
    Ok(files)
}

/// Create source and register a linked draft. Failure after directory creation
/// leaves the user's new source in place with explicit recovery instructions.
pub(crate) fn create(
    store: &Store,
    selected_parent: &Path,
    name: &str,
    slug: &str,
) -> Result<Project, String> {
    let files = render(name, slug)?;
    let destination = write_new_folder(
        store,
        selected_parent,
        slug,
        files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_bytes())),
    )?;
    crate::development::link(store, None, &destination).map_err(|error| {
        format!(
            "{error} Source was left at {}. Review it, then use Link project to register it.",
            destination.display()
        )
    })
}

/// Write only validated snapshot paths into a newly allocated user folder.
pub(crate) fn write_new_folder<'a>(
    store: &Store,
    selected_parent: &Path,
    slug: &str,
    files: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Result<PathBuf, String> {
    let metadata = std::fs::symlink_metadata(selected_parent)
        .map_err(|e| format!("Cannot read the chosen parent folder: {e}"))?;
    if !metadata.is_dir() {
        return Err("Choose a real parent directory, not a symbolic link.".into());
    }
    let parent = std::fs::canonicalize(selected_parent).map_err(|e| e.to_string())?;
    let destination: PathBuf = parent.join(slug);
    crate::development::reject_managed_folder(store, &destination)?;
    std::fs::create_dir(&destination).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            "That folder already exists. Choose another folder name, or use Link project for existing source.".to_owned()
        } else {
            format!("Cannot create the project folder: {e}")
        }
    })?;
    let result = (|| {
        for (relative, bytes) in files {
            let path = destination.join(relative);
            std::fs::create_dir_all(path.parent().ok_or("Starter path has no parent.")?)
                .map_err(|e| e.to_string())?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| e.to_string())?;
            file.write_all(bytes).map_err(|e| e.to_string())?;
        }
        Ok(())
    })();
    result.map_err(|error: String| {
        format!(
            "{error} Review the partially written source at {} before linking it.",
            destination.display()
        )
    })?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_a_linked_portable_starter_with_safe_name_without_execution() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("my projects");
        std::fs::create_dir(&parent).unwrap();
        let store = Store::new(temp.path().join("app-data/projects"));
        let project = create(
            &store,
            &parent,
            "Alex's \"Tracker\": Δ",
            "crypto-tracker",
        )
        .unwrap();
        assert_eq!(project.manifest.metadata.name, "Alex's \"Tracker\": Δ");
        assert_eq!(project.manifest.metadata.id, "crypto-tracker");
        assert!(project.installed.is_none());
        assert!(project.built_image_id.is_none());
        let folder = crate::development::folder(&store, &project.id).unwrap();
        assert!(folder.ends_with("my projects/crypto-tracker"));
        assert!(!folder.join("POCKETS_BRIEF.md").exists());
        let agent_instructions = std::fs::read_to_string(folder.join("AGENTS.md")).unwrap();
        assert!(agent_instructions.contains("POCKETS.md"));
        assert!(!agent_instructions.contains("POCKETS_BRIEF.md"));
        assert!(agent_instructions.contains("POCKETS_CHANGES.md"));
        assert!(
            std::fs::read_to_string(folder.join(crate::versions::CHANGES_FILE))
                .unwrap()
                .contains("\n- ")
        );
        let runtime_instructions = std::fs::read_to_string(folder.join("POCKETS.md")).unwrap();
        assert!(runtime_instructions.contains("npm test"));
        assert!(runtime_instructions.contains("previewApis"));
        assert!(runtime_instructions.contains("POCKETS_HTTPS_GATEWAY"));
        assert!(runtime_instructions.contains("fetchApi"));
        assert!(runtime_instructions.contains("read-only"));
        assert!(runtime_instructions.contains("lib/cache.js"));
        assert!(runtime_instructions.contains("stale-while-revalidate"));
        let containerfile = std::fs::read_to_string(folder.join("Containerfile")).unwrap();
        assert!(
            crate::hub::audit_containerfile(&containerfile).is_empty(),
            "starter Containerfile should pass Hub security audit"
        );
        let gitignore = std::fs::read_to_string(folder.join(".gitignore")).unwrap();
        assert!(gitignore.contains(".env"));
        assert!(folder.join("lib/cache.js").is_file());
        assert!(folder.join("test/cache.test.js").is_file());
        assert!(folder.join("test/server.test.js").is_file());
        assert!(folder.join(".gitignore").is_file());
        assert!(!folder.join("node_modules").exists());
        assert!(!folder.join("data").exists());
        assert!(!temp.path().join("app-data/ai").exists());
        assert_eq!(
            crate::development::link(&store, None, &folder).unwrap().id,
            project.id
        );
    }

    #[test]
    fn refuses_existing_destinations_bad_inputs_and_managed_locations_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("app-data/projects"));
        let taken = temp.path().join("taken");
        std::fs::create_dir(&taken).unwrap();
        std::fs::write(taken.join("keep"), "original").unwrap();
        assert!(create(&store, temp.path(), "Taken", "taken")
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(
            std::fs::read_to_string(taken.join("keep")).unwrap(),
            "original"
        );
        for slug in ["../escape", "bad/name", "bad--name", "Upper", ".", ""] {
            assert!(create(&store, temp.path(), "Name", slug).is_err());
        }
        assert!(create(&store, temp.path(), "Bad\nName", "bad-name").is_err());
        assert!(create(&store, temp.path(), "Bad\u{2028}Name", "bad-name").is_err());
        assert!(
            render("My __POCKET_CUSTOM__ App", "valid-slug")
                .unwrap_err()
                .contains("unresolved template value")
        );
        let max_name = "a".repeat(80);
        let max_slug = "a".repeat(63);
        assert!(inputs(&max_name, &max_slug).is_ok());
        assert!(inputs(&"a".repeat(81), &max_slug).is_err());
        assert!(inputs(&max_name, &"a".repeat(64)).is_err());
        std::fs::create_dir_all(store.root()).unwrap();
        assert!(create(&store, store.root(), "Name", "managed").is_err());
        assert!(!store.root().join("managed").exists());
        assert!(store.list().unwrap().is_empty());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&taken, temp.path().join("linked")).unwrap();
            assert!(create(&store, temp.path(), "Name", "linked").is_err());
            assert!(create(&store, &temp.path().join("linked"), "Name", "new").is_err());
            assert!(!taken.join("new").exists());
        }
    }

    #[test]
    fn registration_failure_leaves_new_source_available_to_recover_by_linking() {
        let temp = tempfile::tempdir().unwrap();
        let invalid_store = temp.path().join("projects-file");
        std::fs::write(&invalid_store, "keep existing file").unwrap();
        let store = Store::new(invalid_store.clone());
        let error = create(&store, temp.path(), "Recoverable", "recoverable").unwrap_err();
        assert!(error.contains("Source was left at"));
        assert!(error.contains("Link project"));
        assert!(temp.path().join("recoverable/server.js").is_file());
        assert_eq!(
            std::fs::read_to_string(invalid_store).unwrap(),
            "keep existing file"
        );
    }
}
