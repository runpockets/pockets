//! Non-destructive in-place "Pocketize" for existing local project folders.
//!
//! Adds only the missing Pocket contract files (`pocket.yaml`, `Containerfile`,
//! `POCKETS.md`, `POCKETS_CHANGES.md`, `.containerignore`, and `AGENTS.md` —
//! appending a Pocket section if `AGENTS.md` already exists without referencing
//! `POCKETS.md`), never overwrites existing source code, executes no code, and
//! rolls back every file change if linking fails.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::manifest::Architecture;
use crate::projects::{Project, Store};
use crate::{development, fs_browser, scaffold, source};

/// Marker comment placed at the top of a generated placeholder `Containerfile`.
/// Cleared once the user or their coding tool adapts `Containerfile` for the
/// repository's actual stack.
pub const PLACEHOLDER_CONTAINERFILE_MARKER: &str = "# POCKETS-PLACEHOLDER-CONTAINERFILE";

const MAX_AGENTS_BYTES: u64 = 512 * 1024;

const AGENTS_APPEND_SECTION: &str = "## Working on this Pocket\n\
\n\
Read `POCKETS.md` for the Pockets runtime requirements and verification steps.\n\
Work in this folder and preserve `metadata.id` in `pocket.yaml`.\n\
\n\
When you finish a change, add one line to the end of `POCKETS_CHANGES.md`\n\
saying what changed, written like a commit message (for example\n\
`- Add price alerts to the watchlist`). Pockets uses these lines to name the\n\
next version the user makes live, so keep each to one short line.\n";

const POCKETIZE_GUIDE_SECTION: &str = "\n\
## Adapting an existing project\n\
\n\
This folder was pocketized from an existing project. Complete these steps so\n\
Pockets can build and run it cleanly:\n\
\n\
1. **Replace the placeholder `Containerfile`** (or verify an existing one):\n\
   build and run this repository's stack inside a single container as a\n\
   non-root user, preparing a writable `/data` directory for the persistent\n\
   volume. If a `Dockerfile` is present, adapt its steps into `Containerfile`.\n\
2. **Bind `0.0.0.0` on `PORT`** (matching `web.port` in `pocket.yaml`) and\n\
   serve `GET /health` returning HTTP `200` when the app is ready.\n\
3. **Move persistent state to `DATA_DIR`** (defaults to `/data`), including\n\
   databases, JSON files, uploads, or caches.\n\
4. **Declare environment variables** under `runtimeVariables` in `pocket.yaml`\n\
   (never commit `.env` values; `.env*` files are excluded from snapshots).\n\
5. **Remove `# POCKETS-PLACEHOLDER-CONTAINERFILE`** from `Containerfile` once\n\
   adapted, then add a one-line summary to `POCKETS_CHANGES.md`.\n\
\n\
> Note: Live Preview currently runs dependency-free root `server.js` apps.\n\
> For frameworks, npm dependencies, or other languages (Python, Go, Rust,\n\
> etc.), use **Sync source → Build & start** in Pockets.\n";

const DEFAULT_CHANGES_MD: &str = "# Changes\n\
\n\
- Pocketize existing project\n";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PocketizeInspection {
    pub folder_name: String,
    pub folder_path: String,
    pub already_pocket: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub existing_id: Option<String>,
    pub suggested_name: String,
    pub suggested_id: String,
    pub suggested_subfolder: String,
    pub detected_stack: Option<String>,
    pub has_server_js: bool,
    pub has_dockerfile: bool,
    pub has_containerfile: bool,
    pub files_to_add: Vec<String>,
    pub files_to_update: Vec<String>,
    pub existing_files_kept: Vec<String>,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PocketizeResult {
    pub project: Project,
    pub files_added: Vec<String>,
    pub files_updated: Vec<String>,
    pub existing_files_kept: Vec<String>,
    pub ai_prompt: String,
}

/// Returns `true` when `containerfile` is still Pockets' unadapted placeholder.
pub fn is_placeholder_containerfile(containerfile: &str) -> bool {
    containerfile.contains(PLACEHOLDER_CONTAINERFILE_MARKER)
}

/// Refuses to build a Pocket whose `Containerfile` is still the placeholder.
pub fn ensure_containerfile_adapted(containerfile: &str) -> Result<(), String> {
    if is_placeholder_containerfile(containerfile) {
        return Err(
            "This Pocket still has a placeholder Containerfile. Adapt it in your coding tool \
             (see POCKETS.md) and remove the placeholder header, then click Build & start again."
                .into(),
        );
    }
    Ok(())
}

/// Turns an arbitrary folder name into a valid Pocket ID slug (`[a-z0-9-]+`).
pub fn slugify_folder_name(raw: &str) -> String {
    let mut slug = String::new();
    let mut prev_hyphen = true;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_hyphen = false;
        } else if !prev_hyphen {
            slug.push('-');
            prev_hyphen = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    let mut bounded: String = trimmed.chars().take(63).collect();
    while bounded.ends_with('-') {
        bounded.pop();
    }
    if bounded.is_empty() {
        "my-pocket".to_owned()
    } else {
        bounded
    }
}

/// Turns a folder name into a clean display name (1–80 chars, single line).
pub fn title_case_name(raw: &str) -> String {
    let words: Vec<String> = raw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    let mut word = first.to_uppercase().to_string();
                    word.push_str(chars.as_str());
                    word
                }
                None => String::new(),
            }
        })
        .collect();
    let joined = words.join(" ");
    let bounded: String = joined.chars().take(80).collect();
    let trimmed = bounded.trim();
    if trimmed.is_empty() {
        "My Pocket".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Lightweight, read-only stack hint derived from root marker files.
fn detect_stack(dir: &Path) -> Option<&'static str> {
    if dir.join("package.json").is_file() {
        Some("Node.js")
    } else if dir.join("pyproject.toml").is_file()
        || dir.join("requirements.txt").is_file()
        || dir.join("Pipfile").is_file()
    {
        Some("Python")
    } else if dir.join("go.mod").is_file() {
        Some("Go")
    } else if dir.join("Cargo.toml").is_file() {
        Some("Rust")
    } else if dir.join("Gemfile").is_file() {
        Some("Ruby")
    } else if dir.join("composer.json").is_file() {
        Some("PHP")
    } else if dir.join("Dockerfile").is_file() {
        Some("Docker")
    } else if dir.join("index.html").is_file() {
        Some("Static Web")
    } else {
        None
    }
}

/// Builds a concise, tailored handoff prompt for the user's coding tool.
pub fn build_ai_prompt(
    name: &str,
    has_dockerfile: bool,
    has_existing_containerfile: bool,
    detected_stack: Option<&str>,
) -> String {
    let stack_clause = match detected_stack {
        Some(stack) => format!(" ({stack})"),
        None => String::new(),
    };
    let container_step = if has_existing_containerfile {
        "verify the existing Containerfile runs as a non-root user with a writable /data directory"
    } else if has_dockerfile {
        "replace the placeholder Containerfile by adapting the existing Dockerfile into a non-root container with a writable /data directory (removing the # POCKETS-PLACEHOLDER-CONTAINERFILE header)"
    } else {
        "replace the placeholder Containerfile with a clean non-root container for this app and a writable /data directory (removing the # POCKETS-PLACEHOLDER-CONTAINERFILE header)"
    };
    format!(
        "Read POCKETS.md and adapt this existing {name}{stack_clause} project to run as a Pocket: \
         ensure the HTTP server binds 0.0.0.0 on PORT (matching web.port in pocket.yaml), \
         serves GET /health returning 200, persists app state under DATA_DIR (/data), \
         declares any required environment variables under runtimeVariables in pocket.yaml, \
         and {container_step}. Keep metadata.id in pocket.yaml unchanged and record a one-line \
         summary in POCKETS_CHANGES.md when done."
    )
}

fn placeholder_containerfile(
    name: &str,
    has_dockerfile: bool,
    detected_stack: Option<&str>,
) -> String {
    let stack_note = match detected_stack {
        Some(stack) => format!("# Detected stack: {stack}\n"),
        None => String::new(),
    };
    let dockerfile_note = if has_dockerfile {
        "# An existing Dockerfile was detected in this folder — adapt its steps here.\n"
    } else {
        ""
    };
    format!(
        "{PLACEHOLDER_CONTAINERFILE_MARKER}\n\
         # {name}: placeholder Containerfile created by `pockets init` / Pocketize.\n\
         {stack_note}{dockerfile_note}\
         # Ask your coding tool to read POCKETS.md, replace this file with a real\n\
         # non-root Containerfile for your project (preparing a writable /data\n\
         # directory), and remove the marker line above before clicking Build & start.\n\
         FROM docker.io/library/node:24-alpine\n\
         \n\
         ENV HOST=0.0.0.0 \\\n\
             PORT=8080 \\\n\
             DATA_DIR=/data\n\
         \n\
         WORKDIR /app\n\
         RUN mkdir -p /data && chown node:node /data\n\
         USER node\n\
         EXPOSE 8080\n\
         CMD [\"node\", \"-e\", \"console.error('Adapt Containerfile for your app before building.'); process.exit(1)\"]\n"
    )
}

fn render_pocket_yaml(name: &str, id: &str) -> Result<String, String> {
    let arch = Architecture::from_host()?;
    let template = scaffold::template_file("pocket.yaml")
        .ok_or("Bundled starter pocket.yaml is missing.")?;
    let yaml_name = serde_json::to_string(name.trim()).map_err(|e| e.to_string())?;
    let rendered = template
        .replace("__POCKET_ID__", id)
        .replace("__POCKET_NAME__", &yaml_name)
        .replace("__POCKET_ARCH__", arch.as_str());
    if rendered.contains("__POCKET_") {
        return Err("The manifest has an unresolved template value. Choose a different name.".into());
    }
    let manifest = crate::manifest::parse_manifest(&rendered)?;
    manifest.validate_host()?;
    Ok(rendered)
}

fn render_pockets_md() -> Result<String, String> {
    let base =
        scaffold::template_file("POCKETS.md").ok_or("Bundled starter POCKETS.md is missing.")?;
    Ok(format!("{base}{POCKETIZE_GUIDE_SECTION}"))
}

fn render_containerignore() -> String {
    let base = scaffold::template_file(".containerignore").unwrap_or(
        ".git\n.env\n.env.*\ncredentials.json\n*.pem\n*.key\n*.p12\n*.pfx\ndata\nnode_modules\n",
    );
    let mut out = base.to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.lines().any(|line| line.trim() == "venv") {
        out.push_str("venv\n");
    }
    if !out.lines().any(|line| line.trim() == "__pycache__") {
        out.push_str("__pycache__\n");
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentsPlan {
    Create,
    Append,
    Keep,
}

fn classify_agents_md(dir: &Path) -> AgentsPlan {
    let path = dir.join("AGENTS.md");
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return AgentsPlan::Create;
    };
    if !meta.is_file() || meta.len() > MAX_AGENTS_BYTES {
        return AgentsPlan::Keep;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return AgentsPlan::Keep;
    };
    if text.contains("POCKETS.md") {
        AgentsPlan::Keep
    } else {
        AgentsPlan::Append
    }
}

fn path_exists_any(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Read-only preflight inspection of an existing local folder before pocketizing.
pub fn inspect(store: &Store, folder: &Path) -> Result<PocketizeInspection, String> {
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    let meta = std::fs::symlink_metadata(folder)
        .map_err(|err| format!("Could not read '{}': {err}", folder.display()))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(format!(
            "'{}' is not a folder. Choose a real project directory.",
            folder.display()
        ));
    }

    let canonical = std::fs::canonicalize(folder)
        .map_err(|err| format!("Could not resolve '{}': {err}", folder.display()))?;
    if let Err(err) = development::reject_managed_folder(store, &canonical) {
        blockers.push(err);
    }
    let pocket_yaml_path = canonical.join("pocket.yaml");
    let nested_pocket_dir = canonical.join("pocket");
    let nested_pocket_yaml = nested_pocket_dir.join("pocket.yaml");

    let mut effective_dir = canonical.clone();
    let mut is_nested_pocket = false;
    let already_pocket = match std::fs::symlink_metadata(&pocket_yaml_path) {
        Ok(m) if m.is_file() => true,
        Ok(_) => {
            blockers.push(
                "Root pocket.yaml exists but is not a regular file. Remove or fix it before continuing."
                    .into(),
            );
            false
        }
        Err(_) => match std::fs::symlink_metadata(&nested_pocket_yaml) {
            Ok(m) if m.is_file() => {
                effective_dir = nested_pocket_dir;
                is_nested_pocket = true;
                true
            }
            _ => false,
        },
    };

    let folder_path = fs_browser::abbreviate_home_dir(&effective_dir);

    let raw_name = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("my-pocket");
    let folder_name = if is_nested_pocket {
        format!("{raw_name}/pocket")
    } else {
        raw_name.to_owned()
    };
    let suggested_name = title_case_name(raw_name);
    let suggested_id = slugify_folder_name(raw_name);
    let suggested_subfolder = if !path_exists_any(&canonical.join("pocket")) {
        "pocket".to_owned()
    } else {
        let fallback = slugify_folder_name(&format!("{suggested_id}-pocket"));
        if !path_exists_any(&canonical.join(&fallback)) {
            fallback
        } else {
            "pocket-app".to_owned()
        }
    };

    let detected_stack = detect_stack(&canonical).map(str::to_owned);
    let has_server_js = effective_dir.join("server.js").is_file();
    let has_dockerfile = canonical.join("Dockerfile").is_file();
    let has_containerfile = effective_dir.join("Containerfile").is_file();

    let mut existing_name = None;
    let mut existing_id = None;

    if already_pocket {
        let manifest_path = effective_dir.join("pocket.yaml");
        match std::fs::read_to_string(&manifest_path) {
            Ok(yaml) => match crate::manifest::parse_manifest(&yaml) {
                Ok(manifest) => {
                    existing_name = Some(manifest.metadata.name.clone());
                    existing_id = Some(manifest.metadata.id.clone());
                    if let Err(err) = manifest.validate_host() {
                        blockers.push(err);
                    }
                }
                Err(err) => blockers.push(err),
            },
            Err(err) => blockers.push(format!("Could not read pocket.yaml: {err}")),
        }
    }

    let mut files_to_add = Vec::new();
    let mut files_to_update = Vec::new();
    let mut existing_files_kept = Vec::new();

    if !already_pocket {
        for filename in [
            "pocket.yaml",
            "Containerfile",
            ".containerignore",
            "POCKETS.md",
            "POCKETS_CHANGES.md",
        ] {
            if path_exists_any(&canonical.join(filename)) {
                existing_files_kept.push(filename.to_owned());
            } else {
                files_to_add.push(filename.to_owned());
            }
        }

        match classify_agents_md(&canonical) {
            AgentsPlan::Create => files_to_add.push("AGENTS.md".to_owned()),
            AgentsPlan::Append => files_to_update.push("AGENTS.md".to_owned()),
            AgentsPlan::Keep => existing_files_kept.push("AGENTS.md".to_owned()),
        }

        if !has_containerfile && has_dockerfile {
            warnings.push(
                "Existing Dockerfile detected — your coding tool can adapt it into Containerfile."
                    .into(),
            );
        }

        if !has_server_js {
            warnings.push(
                "Live Preview requires a dependency-free root server.js. For other stacks, use Build & start after adapting."
                    .into(),
            );
        }
    }

    Ok(PocketizeInspection {
        folder_name,
        folder_path,
        already_pocket,
        existing_name,
        existing_id,
        suggested_name,
        suggested_id,
        suggested_subfolder,
        detected_stack,
        has_server_js,
        has_dockerfile,
        has_containerfile,
        files_to_add,
        files_to_update,
        existing_files_kept,
        warnings,
        blockers,
    })
}

fn write_new_file(
    path: &Path,
    bytes: &[u8],
    created: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("Could not write {}: {err}", path.display()))?;
    created.push(path.to_path_buf());
    file.write_all(bytes)
        .map_err(|err| format!("Could not write {}: {err}", path.display()))
}

fn rollback(created: &[PathBuf], agents_backup: &Option<(PathBuf, Vec<u8>)>) {
    if let Some((path, original)) = agents_backup {
        let _ = std::fs::write(path, original);
    }
    for path in created.iter().rev() {
        let _ = std::fs::remove_file(path);
    }
}

/// Non-destructively adds missing Pocket contract files to an existing folder,
/// links it as a Draft Pocket, and rolls back every filesystem change on error.
pub fn pocketize(
    store: &Store,
    folder: &Path,
    name: &str,
    id: &str,
) -> Result<PocketizeResult, String> {
    scaffold::inputs(name, id)?;

    let meta = std::fs::symlink_metadata(folder)
        .map_err(|err| format!("Could not read '{}': {err}", folder.display()))?;
    if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
        return Err(format!(
            "'{}' is not a folder. Choose a real project directory.",
            folder.display()
        ));
    }

    let canonical = std::fs::canonicalize(folder)
        .map_err(|err| format!("Could not resolve '{}': {err}", folder.display()))?;
    development::reject_managed_folder(store, &canonical)?;

    if path_exists_any(&canonical.join("pocket.yaml")) {
        return Err(
            "This folder already contains pocket.yaml. Use Link existing project instead.".into(),
        );
    }

    // Dry-run source rules before writing anything so obvious blockers fail cleanly.
    source::source_signature(&canonical)?;

    let detected_stack = detect_stack(&canonical);
    let has_dockerfile = canonical.join("Dockerfile").is_file();
    let has_containerfile = canonical.join("Containerfile").is_file();

    let manifest_text = render_pocket_yaml(name, id)?;
    let pockets_md = render_pockets_md()?;
    let containerignore = render_containerignore();
    let starter_agents = scaffold::template_file("AGENTS.md")
        .ok_or("Bundled starter AGENTS.md is missing.")?;

    let mut created_paths: Vec<PathBuf> = Vec::new();
    let mut agents_backup: Option<(PathBuf, Vec<u8>)> = None;
    let mut files_added: Vec<String> = Vec::new();
    let mut files_updated: Vec<String> = Vec::new();
    let mut existing_files_kept: Vec<String> = Vec::new();

    let write_outcome: Result<(), String> = (|| {
        // 1. pocket.yaml (always newly created; checked absent above)
        write_new_file(
            &canonical.join("pocket.yaml"),
            manifest_text.as_bytes(),
            &mut created_paths,
        )?;
        files_added.push("pocket.yaml".to_owned());

        // 2. Containerfile
        let containerfile_path = canonical.join("Containerfile");
        if path_exists_any(&containerfile_path) {
            existing_files_kept.push("Containerfile".to_owned());
        } else {
            let content = placeholder_containerfile(name.trim(), has_dockerfile, detected_stack);
            write_new_file(&containerfile_path, content.as_bytes(), &mut created_paths)?;
            files_added.push("Containerfile".to_owned());
        }

        // 3. .containerignore
        let ignore_path = canonical.join(".containerignore");
        if path_exists_any(&ignore_path) {
            existing_files_kept.push(".containerignore".to_owned());
        } else {
            write_new_file(&ignore_path, containerignore.as_bytes(), &mut created_paths)?;
            files_added.push(".containerignore".to_owned());
        }

        // 4. POCKETS.md
        let pockets_md_path = canonical.join("POCKETS.md");
        if path_exists_any(&pockets_md_path) {
            existing_files_kept.push("POCKETS.md".to_owned());
        } else {
            write_new_file(&pockets_md_path, pockets_md.as_bytes(), &mut created_paths)?;
            files_added.push("POCKETS.md".to_owned());
        }

        // 5. POCKETS_CHANGES.md
        let changes_path = canonical.join("POCKETS_CHANGES.md");
        if path_exists_any(&changes_path) {
            existing_files_kept.push("POCKETS_CHANGES.md".to_owned());
        } else {
            write_new_file(
                &changes_path,
                DEFAULT_CHANGES_MD.as_bytes(),
                &mut created_paths,
            )?;
            files_added.push("POCKETS_CHANGES.md".to_owned());
        }

        // 6. AGENTS.md (create if missing, append if present without POCKETS.md)
        let agents_path = canonical.join("AGENTS.md");
        match classify_agents_md(&canonical) {
            AgentsPlan::Create => {
                write_new_file(&agents_path, starter_agents.as_bytes(), &mut created_paths)?;
                files_added.push("AGENTS.md".to_owned());
            }
            AgentsPlan::Append => {
                let original = std::fs::read(&agents_path)
                    .map_err(|err| format!("Could not read AGENTS.md: {err}"))?;
                let existing_text = String::from_utf8(original.clone())
                    .map_err(|_| "Existing AGENTS.md is not valid UTF-8.".to_owned())?;
                agents_backup = Some((agents_path.clone(), original));
                let separator = if existing_text.is_empty() {
                    ""
                } else if existing_text.ends_with("\n\n") {
                    ""
                } else if existing_text.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                let updated = format!("{existing_text}{separator}{AGENTS_APPEND_SECTION}");
                std::fs::write(&agents_path, updated.as_bytes())
                    .map_err(|err| format!("Could not update AGENTS.md: {err}"))?;
                files_updated.push("AGENTS.md".to_owned());
            }
            AgentsPlan::Keep => {
                existing_files_kept.push("AGENTS.md".to_owned());
            }
        }

        Ok(())
    })();

    if let Err(err) = write_outcome {
        rollback(&created_paths, &agents_backup);
        return Err(err);
    }

    let project = match development::link(store, None, &canonical) {
        Ok(project) => project,
        Err(err) => {
            rollback(&created_paths, &agents_backup);
            return Err(err);
        }
    };

    let ai_prompt = build_ai_prompt(
        name.trim(),
        has_dockerfile,
        has_containerfile,
        detected_stack,
    );

    Ok(PocketizeResult {
        project,
        files_added,
        files_updated,
        existing_files_kept,
        ai_prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_and_title_cases_folder_names() {
        assert_eq!(slugify_folder_name("My_Budget App!"), "my-budget-app");
        assert_eq!(slugify_folder_name("---"), "my-pocket");
        assert_eq!(title_case_name("my_budget-app"), "My Budget App");
        assert_eq!(title_case_name("___"), "My Pocket");
    }

    #[test]
    fn inspects_and_pocketizes_python_repo_with_existing_agents_and_venv() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("store/projects"));
        let repo = temp.path().join("budget_tracker");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("requirements.txt"), "fastapi\n").unwrap();
        std::fs::write(repo.join("app.py"), "print('hi')\n").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "# Repo rules\nUse pytest.\n").unwrap();
        // Excluded Python artifacts (including a symlink inside venv) must not block capture.
        std::fs::create_dir_all(repo.join("venv/bin")).unwrap();
        std::fs::create_dir_all(repo.join("__pycache__")).unwrap();
        std::fs::write(repo.join("__pycache__/app.cpython-312.pyc"), b"pyc").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/usr/bin/python3", repo.join("venv/bin/python")).unwrap();

        let preflight = inspect(&store, &repo).unwrap();
        assert!(!preflight.already_pocket);
        assert_eq!(preflight.suggested_name, "Budget Tracker");
        assert_eq!(preflight.suggested_id, "budget-tracker");
        assert_eq!(preflight.detected_stack.as_deref(), Some("Python"));
        assert!(preflight.blockers.is_empty(), "{:?}", preflight.blockers);
        assert!(preflight.files_to_add.contains(&"pocket.yaml".to_owned()));
        assert!(preflight.files_to_add.contains(&"Containerfile".to_owned()));
        assert_eq!(preflight.files_to_update, vec!["AGENTS.md".to_owned()]);

        let result = pocketize(&store, &repo, "Budget Tracker", "budget-tracker").unwrap();
        assert_eq!(result.project.manifest.metadata.id, "budget-tracker");
        assert_eq!(result.project.manifest.metadata.name, "Budget Tracker");
        assert!(is_placeholder_containerfile(&result.project.containerfile));
        assert!(ensure_containerfile_adapted(&result.project.containerfile).is_err());
        assert_eq!(result.files_updated, vec!["AGENTS.md".to_owned()]);
        assert!(result.ai_prompt.contains("Python"));
        assert!(result.ai_prompt.contains("POCKETS.md"));

        // Existing app code is untouched and AGENTS.md kept its original content.
        assert_eq!(
            std::fs::read_to_string(repo.join("app.py")).unwrap(),
            "print('hi')\n"
        );
        let agents = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
        assert!(agents.starts_with("# Repo rules\nUse pytest.\n\n## Working on this Pocket"));
        assert!(agents.contains("POCKETS.md"));
        assert!(std::fs::read_to_string(repo.join("POCKETS.md"))
            .unwrap()
            .contains("Adapting an existing project"));
    }

    #[test]
    fn preserves_existing_containerfile_and_already_updated_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("store/projects"));
        let repo = temp.path().join("go-service");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("go.mod"), "module example.com/svc\n").unwrap();
        let custom_containerfile = "FROM golang:1.23-alpine\nUSER nobody\nCMD [\"/app\"]\n";
        std::fs::write(repo.join("Containerfile"), custom_containerfile).unwrap();
        let existing_agents = "# Notes\nSee POCKETS.md.\n";
        std::fs::write(repo.join("AGENTS.md"), existing_agents).unwrap();

        let preflight = inspect(&store, &repo).unwrap();
        assert!(preflight.has_containerfile);
        assert!(preflight
            .existing_files_kept
            .contains(&"Containerfile".to_owned()));
        assert!(preflight
            .existing_files_kept
            .contains(&"AGENTS.md".to_owned()));
        assert!(preflight.files_to_update.is_empty());

        let result = pocketize(&store, &repo, "Go Service", "go-service").unwrap();
        assert!(!is_placeholder_containerfile(&result.project.containerfile));
        assert!(ensure_containerfile_adapted(&result.project.containerfile).is_ok());
        assert_eq!(
            std::fs::read_to_string(repo.join("Containerfile")).unwrap(),
            custom_containerfile
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
            existing_agents
        );
    }

    #[test]
    fn rolls_back_created_files_and_agents_append_when_link_fails() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("store/projects"));
        // Seed an existing project with the same manifest ID so linking a second
        // folder with that ID fails or — even simpler — force an invalid existing
        // Containerfile (empty) so `development::link` fails during `capture_approved`!
        let repo = temp.path().join("broken-containerfile");
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("Containerfile"), "").unwrap();
        let original_agents = "# Original instructions\n";
        std::fs::write(repo.join("AGENTS.md"), original_agents).unwrap();

        let err = pocketize(&store, &repo, "Broken", "broken").unwrap_err();
        assert!(err.contains("Containerfile"), "unexpected error: {err}");

        // Every created file must be rolled back and AGENTS.md restored verbatim.
        assert!(!repo.join("pocket.yaml").exists());
        assert!(!repo.join("POCKETS.md").exists());
        assert!(!repo.join("POCKETS_CHANGES.md").exists());
        assert!(!repo.join(".containerignore").exists());
        assert_eq!(
            std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
            original_agents
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("Containerfile")).unwrap(),
            ""
        );
    }

    #[test]
    fn inspect_reports_blockers_for_symlinks_and_already_pocket_folders() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("store/projects"));

        // Already a pocket:
        let existing = temp.path().join("already");
        std::fs::create_dir(&existing).unwrap();
        let manifest_text = render_pocket_yaml("Notes Pocket", "notes-pocket").unwrap();
        std::fs::write(existing.join("pocket.yaml"), manifest_text).unwrap();
        let info = inspect(&store, &existing).unwrap();
        assert!(info.already_pocket);
        assert_eq!(info.folder_name, "already");
        assert_eq!(info.existing_name.as_deref(), Some("Notes Pocket"));
        assert_eq!(info.existing_id.as_deref(), Some("notes-pocket"));
        assert!(info.blockers.is_empty(), "{:?}", info.blockers);
        assert!(pocketize(&store, &existing, "Already", "already").is_err());

        // Already pocketized in a `pocket/` subfolder:
        let parent_with_nested = temp.path().join("dabn");
        std::fs::create_dir_all(parent_with_nested.join("pocket")).unwrap();
        let nested_manifest = render_pocket_yaml("Dabn", "dabn").unwrap();
        std::fs::write(
            parent_with_nested.join("pocket/pocket.yaml"),
            nested_manifest,
        )
        .unwrap();
        let nested_info = inspect(&store, &parent_with_nested).unwrap();
        assert!(nested_info.already_pocket);
        assert_eq!(nested_info.folder_name, "dabn/pocket");
        assert!(nested_info.folder_path.ends_with("dabn/pocket"));
        assert_eq!(nested_info.existing_name.as_deref(), Some("Dabn"));
        assert_eq!(nested_info.existing_id.as_deref(), Some("dabn"));
        assert!(nested_info.blockers.is_empty(), "{:?}", nested_info.blockers);

        // Symlink inside repo does not block clean-room inspection,
        // but in-place pocketize (`pockets init`) still refuses it before writing anything.
        #[cfg(unix)]
        {
            let with_link = temp.path().join("with-link");
            std::fs::create_dir(&with_link).unwrap();
            std::fs::write(with_link.join("a.txt"), "hello").unwrap();
            std::os::unix::fs::symlink(with_link.join("a.txt"), with_link.join("b.txt")).unwrap();
            let info = inspect(&store, &with_link).unwrap();
            assert!(info.blockers.is_empty());
            assert!(pocketize(&store, &with_link, "With Link", "with-link").is_err());
            assert!(!with_link.join("pocket.yaml").exists());
        }
    }
}
