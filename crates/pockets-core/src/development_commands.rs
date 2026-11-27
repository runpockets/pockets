//! Serialization boundary for external development commands.

use std::path::PathBuf;

use crate::development::{self, DevelopmentStatus};
use crate::projects::{Project, Store};
use crate::AppContext;

pub async fn create_development_project(
    ctx: &AppContext,
    parent: PathBuf,
    name: String,
    folder_name: String,
    id: Option<String>,
) -> Result<Project, String> {
    let manifest_id = id.unwrap_or_else(|| folder_name.clone());
    crate::scaffold::inputs(&name, &manifest_id)?;
    crate::scaffold::inputs(&name, &folder_name)?;
    mutate(ctx, None, false, move |store| {
        crate::scaffold::create_with_id(&store, &parent, &name, &manifest_id, &folder_name)
    })
    .await
}

/// Copies text to the host clipboard when a native clipboard utility is present.
async fn copy_text(text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        write_stdin_command("/usr/bin/pbcopy", &[], text).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        for (bin, args) in [
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
            ("xsel", &["--clipboard", "--input"][..]),
        ] {
            if write_stdin_command(bin, args, text).await.is_ok() {
                return Ok(());
            }
        }
        Err("No Linux clipboard tool (wl-copy, xclip, or xsel) was available.".into())
    }
}

async fn write_stdin_command(bin: &str, args: &[&str], text: &str) -> Result<(), String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let payload = text.as_bytes().to_vec();
    let operation = async {
        let mut child = tokio::process::Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Could not open the clipboard: {e}"))?;
        let mut input = child
            .stdin
            .take()
            .ok_or("The clipboard input was unavailable.")?;
        input
            .write_all(&payload)
            .await
            .map_err(|e| format!("Could not copy: {e}"))?;
        drop(input);
        let status = child
            .wait()
            .await
            .map_err(|e| format!("Could not copy: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err("The clipboard refused the copy. Try again.".into())
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), operation)
        .await
        .map_err(|_| "The clipboard did not respond. Try again.".to_owned())?
}

/// Resolves the registered linked folder path, copies it to the host clipboard
/// when available, and returns the path so a browser client can copy it too.
pub async fn copy_development_location(ctx: &AppContext, id: String) -> Result<String, String> {
    let store = ctx.store();
    let folder = tokio::task::spawn_blocking(move || development::folder(&store, &id))
        .await
        .map_err(|_| "Reading the linked folder was interrupted.".to_owned())??;
    let path = folder
        .to_str()
        .ok_or("The source location must be UTF-8.")?
        .to_owned();
    let _ = copy_text(&path).await;
    Ok(path)
}

// Storage changes share the build and lifecycle guards.
async fn mutate<T, F>(
    ctx: &AppContext,
    id: Option<&str>,
    require_stopped: bool,
    operation: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(Store) -> Result<T, String> + Send + 'static,
{
    let _building = ctx
        .build_jobs
        .try_claim()
        .ok_or("Wait for the current Build/Update to finish.")?;
    let _lifecycle = ctx.controller.lifecycle_guard().await;
    let store = ctx.store();
    if let Some(id) = id {
        if require_stopped && ctx.project_previews.active(id) {
            return Err(
                "Stop the development preview before relinking or unlinking its source folder."
                    .into(),
            );
        }
    }
    tokio::task::spawn_blocking(move || operation(store))
        .await
        .map_err(|_| {
            "The development operation was interrupted. Check its status and retry.".to_owned()
        })?
}

pub async fn export_development_project(
    ctx: &AppContext,
    id: String,
    parent: PathBuf,
) -> Result<Project, String> {
    let project_id = id.clone();
    mutate(ctx, Some(&project_id), true, move |store| {
        development::export(&store, &id, &parent)
    })
    .await
}

pub async fn development_status(
    ctx: &AppContext,
    id: String,
) -> Result<DevelopmentStatus, String> {
    let store = ctx.store();
    tokio::task::spawn_blocking(move || development::status(&store, &id))
        .await
        .map_err(|_| "Reading the development link was interrupted.".to_owned())?
}

pub async fn link_development_folder(
    ctx: &AppContext,
    id: Option<String>,
    folder: PathBuf,
) -> Result<Project, String> {
    let project_id = id.clone();
    mutate(ctx, project_id.as_deref(), true, move |store| {
        development::link(&store, id.as_deref(), &folder)
    })
    .await
}

pub async fn inspect_pocketize_folder(
    ctx: &AppContext,
    folder: PathBuf,
) -> Result<crate::pocketize::PocketizeInspection, String> {
    let store = ctx.store();
    tokio::task::spawn_blocking(move || crate::pocketize::inspect(&store, &folder))
        .await
        .map_err(|_| "Inspecting the selected folder was interrupted.".to_owned())?
}

pub async fn pocketize_development_folder(
    ctx: &AppContext,
    folder: PathBuf,
    name: String,
    id: String,
) -> Result<crate::pocketize::PocketizeResult, String> {
    crate::scaffold::inputs(&name, &id)?;
    mutate(ctx, None, false, move |store| {
        crate::pocketize::pocketize(&store, &folder, &name, &id)
    })
    .await
}

pub async fn unlink_development_folder(ctx: &AppContext, id: String) -> Result<(), String> {
    let project_id = id.clone();
    mutate(ctx, Some(&project_id), true, move |store| {
        development::unlink(&store, &id)
    })
    .await
}

pub async fn sync_development_project(ctx: &AppContext, id: String) -> Result<Project, String> {
    let project_id = id.clone();
    mutate(ctx, Some(&project_id), false, move |store| {
        development::sync(&store, &id)
    })
    .await
}

/// Puts a recorded version back into the linked folder as a git commit,
/// then syncs it. The installed app changes only with a later Update.
pub async fn restore_project_version(
    ctx: &AppContext,
    id: String,
    revision_id: String,
    subject: String,
) -> Result<crate::versions::FolderRestore, String> {
    let project_id = id.clone();
    mutate(ctx, Some(&project_id), false, move |store| {
        crate::versions::restore_into_folder(&store, &id, &revision_id, &subject)
    })
    .await
}

pub async fn open_development_folder(ctx: &AppContext, id: String) -> Result<(), String> {
    let store = ctx.store();
    let folder = tokio::task::spawn_blocking(move || development::folder(&store, &id))
        .await
        .map_err(|_| "Opening the linked folder was interrupted.".to_owned())??;
    crate::open_path_in_os(&folder).await
}
