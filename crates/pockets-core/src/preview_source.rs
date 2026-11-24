//! Read-only preview source contract and a private, stable-inode mirror.
//! Neither the runtime nor this filesystem adapter needs an AI session.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

pub(crate) trait PreviewSource: Sync {
    fn root(&self) -> &Path;
    fn resolve(&self, relative: &str) -> Result<PathBuf, String>;
    fn read(&self, relative: &str) -> Result<String, String>;
    fn supervised(&self) -> bool {
        false
    }
    fn files(&self) -> Vec<String> {
        Vec::new()
    }
}

pub(crate) struct MirroredSource {
    root: PathBuf,
    files: Vec<String>,
}

fn real_directory(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() {
        return Err("The preview workspace must be a real directory.".into());
    }
    Ok(())
}

pub(crate) fn resolve(root: &Path, relative: &str) -> Result<PathBuf, String> {
    real_directory(root)?;
    let path = Path::new(relative);
    if relative.is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("Preview file paths must stay inside the workspace.".into());
    }
    let mut absolute = root.to_path_buf();
    for part in path.components() {
        absolute.push(part);
        match std::fs::symlink_metadata(&absolute) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err("Preview paths cannot follow symbolic links.".into())
            }
            Ok(meta) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if meta.is_file() && meta.nlink() != 1 {
                        return Err("Preview files cannot be hard links.".into());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(absolute)
}

/// Inventory every entry in our owned mirror, including hidden leftovers.
/// Reject links before reading or deleting anything; the container has a
/// read-only mount and can never write into this directory.
fn inventory(
    root: &Path,
    directory: &Path,
    files: &mut Vec<String>,
    dirs: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<(), String> {
    real_directory(directory)?;
    if depth > 16 || files.len() + dirs.len() > 4_000 {
        return Err("The preview workspace exceeds its tree limits.".into());
    }
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        if files.len() + dirs.len() > 4_000 {
            return Err("The preview workspace exceeds its tree limits.".into());
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_str()
            .ok_or("Preview paths must be UTF-8.")?
            .to_owned();
        resolve(root, &relative)?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if metadata.is_dir() {
            dirs.push(path.clone());
            inventory(root, &path, files, dirs, depth + 1)?;
        } else if metadata.is_file() {
            files.push(relative);
        } else {
            return Err("The preview workspace contains a special file.".into());
        }
    }
    Ok(())
}

impl MirroredSource {
    /// Publish permitted bytes into an owned directory without replacing its
    /// root inode (Podman bind mounts must keep following the same root).
    pub(crate) fn sync(
        root: &Path,
        files: &[(String, Vec<u8>)],
    ) -> Result<(Self, Vec<String>), String> {
        match std::fs::symlink_metadata(root) {
            Ok(_) => real_directory(root)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    std::fs::DirBuilder::new()
                        .mode(0o700)
                        .create(root)
                        .map_err(|e| e.to_string())?;
                }
                #[cfg(not(unix))]
                std::fs::create_dir(root).map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }
        let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
        let mut old_names = Vec::new();
        let mut directories = Vec::new();
        inventory(&root, &root, &mut old_names, &mut directories, 0)?;
        let previous: BTreeMap<_, _> = crate::source::capture_record_files(&root, &old_names)?
            .into_iter()
            .collect();
        let next: BTreeMap<_, _> = files.iter().map(|(p, b)| (p.clone(), b)).collect();
        let mut changed = BTreeSet::new();
        // Validate every incoming path before touching the old mirror.
        for path in next.keys() {
            if Path::new(path)
                .components()
                .any(|p| !matches!(p, Component::Normal(_)))
                || path.is_empty()
            {
                return Err("Invalid captured preview path.".into());
            }
        }
        for path in previous.keys().filter(|path| !next.contains_key(*path)) {
            std::fs::remove_file(resolve(&root, path)?).map_err(|e| e.to_string())?;
            changed.insert(path.clone());
        }
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for directory in directories {
            if std::fs::read_dir(&directory)
                .map_err(|e| e.to_string())?
                .next()
                .is_none()
            {
                std::fs::remove_dir(directory).map_err(|e| e.to_string())?;
            }
        }
        for (path, bytes) in files {
            if previous.get(path) == Some(bytes) {
                continue;
            }
            let destination = resolve(&root, path)?;
            std::fs::create_dir_all(destination.parent().ok_or("Missing preview parent.")?)
                .map_err(|e| e.to_string())?;
            resolve(&root, path)?;
            let temporary = root.join(format!(".preview-write-{}", uuid::Uuid::new_v4().simple()));
            let result = std::fs::write(&temporary, bytes)
                .and_then(|_| std::fs::rename(&temporary, &destination));
            if let Err(error) = result {
                let _ = std::fs::remove_file(temporary);
                return Err(error.to_string());
            }
            changed.insert(path.clone());
        }
        // A former file can become a directory, or vice versa. Gate its new
        // files, not an obsolete path that now crosses a regular file.
        changed.retain(|path| {
            next.contains_key(path)
                || !next.keys().any(|new| {
                    new.starts_with(&format!("{path}/")) || path.starts_with(&format!("{new}/"))
                })
        });
        Ok((
            Self {
                root,
                files: next.into_keys().collect(),
            },
            changed.into_iter().collect(),
        ))
    }
}

impl PreviewSource for MirroredSource {
    fn root(&self) -> &Path {
        &self.root
    }
    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        resolve(&self.root, path)
    }
    fn read(&self, path: &str) -> Result<String, String> {
        let captured = crate::source::capture_record_files(&self.root, &[path.to_owned()])?;
        String::from_utf8(
            captured
                .into_iter()
                .next()
                .ok_or("Missing preview file.")?
                .1,
        )
        .map_err(|e| e.to_string())
    }
    fn supervised(&self) -> bool {
        true
    }
    fn files(&self) -> Vec<String> {
        self.files.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_keeps_its_root_and_handles_binary_files_removals_and_tree_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mirror");
        let first = vec![
            ("server.js".into(), b"first".to_vec()),
            ("assets".into(), vec![0, 255, 1]),
            ("old.txt".into(), b"remove".to_vec()),
        ];
        let (source, _) = MirroredSource::sync(&root, &first).unwrap();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(source.root()).unwrap().ino()
        };
        let next = vec![
            ("server.js".into(), b"second".to_vec()),
            ("assets/icon.bin".into(), vec![255, 0, 128]),
        ];
        let (source, changed) = MirroredSource::sync(&root, &next).unwrap();
        assert_eq!(source.read("server.js").unwrap(), "second");
        assert_eq!(
            std::fs::read(source.resolve("assets/icon.bin").unwrap()).unwrap(),
            vec![255, 0, 128]
        );
        assert!(!root.join("old.txt").exists());
        assert!(!changed.contains(&"assets".into()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(source.root()).unwrap().ino(), inode);
        }
        let final_files = vec![
            ("server.js".into(), b"second".to_vec()),
            ("assets".into(), b"now a file".to_vec()),
        ];
        let (_, changed) = MirroredSource::sync(&root, &final_files).unwrap();
        assert!(!changed.contains(&"assets/icon.bin".into()));
        assert_eq!(std::fs::read(root.join("assets")).unwrap(), b"now a file");
        assert!(MirroredSource::sync(&root, &final_files)
            .unwrap()
            .1
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn mirror_refuses_symlinks_hard_links_and_escaping_paths_before_writing() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mirror");
        let files = vec![("server.js".into(), b"safe".to_vec())];
        let (source, _) = MirroredSource::sync(&root, &files).unwrap();
        assert!(source.resolve("../outside").is_err());
        assert!(source.resolve("/absolute").is_err());
        let victim = temp.path().join("outside");
        std::fs::write(&victim, "private").unwrap();
        symlink(&victim, root.join("injected")).unwrap();
        assert!(MirroredSource::sync(&root, &files).is_err());
        std::fs::remove_file(root.join("injected")).unwrap();
        std::fs::hard_link(&victim, root.join("injected")).unwrap();
        assert!(MirroredSource::sync(&root, &files).is_err());
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "private");
    }
}
