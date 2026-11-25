//! JSON RPC dispatcher mapping frontend command names to `pockets-core` calls.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};

use pockets_core::{
    creation, development_commands, fs_browser, github_install, project_lifecycle, project_preview,
    resource_monitor, revision_commands, runtime_settings, AppContext,
};

fn parse_args<T: for<'de> Deserialize<'de>>(body: Value) -> Result<T, String> {
    serde_json::from_value(body).map_err(|err| format!("Invalid command arguments: {err}"))
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| format!("Could not serialize response: {err}"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdArg {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectIdArg {
    project_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewStatusArgs {
    project_id: String,
    cursor: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewReviewArgs {
    project_id: String,
    decision: project_preview::ApiDecision,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewNetworkSetArgs {
    project_id: String,
    hosts: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateDevArgs {
    parent_path: String,
    name: String,
    folder_name: String,
    brief: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportDevArgs {
    id: String,
    parent_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkDevArgs {
    #[serde(default)]
    id: Option<String>,
    folder_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportArgs {
    folder_path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestoreRevisionArgs {
    id: String,
    revision_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetVersionNoteArgs {
    id: String,
    revision_id: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestoreVersionArgs {
    id: String,
    revision_id: String,
    subject: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetTailscaleArgs {
    id: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeSettingsArgs {
    id: String,
    environment: runtime_settings::Environment,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangeRuntimeSettingsArgs {
    id: String,
    environment: runtime_settings::Environment,
    change: runtime_settings::Change,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddGitHubArgs {
    source: String,
    #[serde(default)]
    dest_parent: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowseDirectoryArgs {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    show_hidden: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CheckAppUpdateArgs {
    #[serde(default)]
    force: bool,
}

pub async fn dispatch(
    ctx: &AppContext,
    command: &str,
    body: Value,
    remote_client: bool,
) -> Result<Value, String> {
    match command {
        "check_app_update" => {
            let args: CheckAppUpdateArgs = if body.is_null() {
                CheckAppUpdateArgs::default()
            } else {
                parse_args(body)?
            };
            to_json(pockets_core::app_update::check_for_update(args.force).await)
        }
        "apply_app_update" => to_json(pockets_core::app_update::apply_update().await?),
        "get_manifest" => to_json(pockets_core::get_manifest()?),
        "detect_runtime" => to_json(pockets_core::detect_runtime().await),
        "bootstrap_runtime" => to_json(pockets_core::bootstrap_runtime().await),
        "detect_tailscale" => to_json(pockets_core::detect_tailscale(ctx).await?),
        "pocket_status" => to_json(pockets_core::pocket_status(ctx).await?),
        "start_pocket" => to_json(pockets_core::start_pocket(ctx).await?),
        "stop_pocket" => to_json(pockets_core::stop_pocket(ctx).await?),
        "open_pocket" => to_json(pockets_core::open_pocket(ctx, false, remote_client).await?),

        "list_projects" => to_json(creation::list_projects(ctx).await?),
        "import_project" => {
            let args: ImportArgs = parse_args(body)?;
            to_json(creation::import_project(ctx, PathBuf::from(args.folder_path)).await?)
        }
        "build_project" => {
            let args: IdArg = parse_args(body)?;
            to_json(creation::build_project(ctx, args.id).await?)
        }
        "project_build_status" => {
            let args: IdArg = parse_args(body)?;
            to_json(creation::project_build_status(ctx, args.id).await?)
        }

        "development_status" => {
            let args: IdArg = parse_args(body)?;
            to_json(development_commands::development_status(ctx, args.id).await?)
        }
        "create_development_project" => {
            let args: CreateDevArgs = parse_args(body)?;
            to_json(
                development_commands::create_development_project(
                    ctx,
                    PathBuf::from(args.parent_path),
                    args.name,
                    args.folder_name,
                    args.brief,
                )
                .await?,
            )
        }
        "export_development_project" => {
            let args: ExportDevArgs = parse_args(body)?;
            to_json(
                development_commands::export_development_project(
                    ctx,
                    args.id,
                    PathBuf::from(args.parent_path),
                )
                .await?,
            )
        }
        "link_development_folder" => {
            let args: LinkDevArgs = parse_args(body)?;
            to_json(
                development_commands::link_development_folder(
                    ctx,
                    args.id,
                    PathBuf::from(args.folder_path),
                )
                .await?,
            )
        }
        "unlink_development_folder" => {
            let args: IdArg = parse_args(body)?;
            development_commands::unlink_development_folder(ctx, args.id).await?;
            Ok(Value::Null)
        }
        "sync_development_project" => {
            let args: IdArg = parse_args(body)?;
            to_json(development_commands::sync_development_project(ctx, args.id).await?)
        }
        "open_development_folder" => {
            let args: IdArg = parse_args(body)?;
            development_commands::open_development_folder(ctx, args.id).await?;
            Ok(Value::Null)
        }
        "copy_development_location" => {
            let args: IdArg = parse_args(body)?;
            to_json(development_commands::copy_development_location(ctx, args.id).await?)
        }
        "restore_project_version" => {
            let args: RestoreVersionArgs = parse_args(body)?;
            to_json(
                development_commands::restore_project_version(
                    ctx,
                    args.id,
                    args.revision_id,
                    args.subject,
                )
                .await?,
            )
        }

        "project_preview_start" => {
            let args: ProjectIdArg = parse_args(body)?;
            to_json(
                project_preview::project_preview_start(ctx, args.project_id, remote_client).await?,
            )
        }
        "project_preview_status" => {
            let args: PreviewStatusArgs = parse_args(body)?;
            to_json(
                project_preview::project_preview_status(
                    ctx,
                    args.project_id,
                    args.cursor,
                    remote_client,
                )
                .await?,
            )
        }
        "project_preview_stop" => {
            let args: ProjectIdArg = parse_args(body)?;
            to_json(project_preview::project_preview_stop(ctx, args.project_id).await?)
        }
        "project_preview_open" => {
            let args: ProjectIdArg = parse_args(body)?;
            to_json(
                project_preview::project_preview_open(ctx, args.project_id, false, remote_client)
                    .await?,
            )
        }
        "project_preview_logs" => {
            let args: ProjectIdArg = parse_args(body)?;
            to_json(project_preview::project_preview_logs(ctx, args.project_id).await?)
        }
        "project_preview_network" => {
            let args: ProjectIdArg = parse_args(body)?;
            to_json(project_preview::project_preview_network(ctx, args.project_id).await?)
        }
        "set_project_preview_network" => {
            let args: PreviewNetworkSetArgs = parse_args(body)?;
            to_json(
                project_preview::set_project_preview_network(ctx, args.project_id, args.hosts)
                    .await?,
            )
        }
        "project_preview_review_api_access" => {
            let args: PreviewReviewArgs = parse_args(body)?;
            to_json(
                project_preview::project_preview_review_api_access(
                    ctx,
                    args.project_id,
                    args.decision,
                    remote_client,
                )
                .await?,
            )
        }

        "list_project_revisions" => {
            let args: IdArg = parse_args(body)?;
            to_json(revision_commands::list_project_revisions(ctx, args.id).await?)
        }
        "restore_project_revision" => {
            let args: RestoreRevisionArgs = parse_args(body)?;
            to_json(
                revision_commands::restore_project_revision(ctx, args.id, args.revision_id).await?,
            )
        }
        "list_project_versions" => {
            let args: IdArg = parse_args(body)?;
            to_json(revision_commands::list_project_versions(ctx, args.id).await?)
        }
        "set_project_version_note" => {
            let args: SetVersionNoteArgs = parse_args(body)?;
            revision_commands::set_project_version_note(
                ctx,
                args.id,
                args.revision_id,
                args.note,
            )
            .await?;
            Ok(Value::Null)
        }

        "project_status" => {
            let args: IdArg = parse_args(body)?;
            to_json(project_lifecycle::project_status(ctx, args.id).await?)
        }
        "start_project" => {
            let args: IdArg = parse_args(body)?;
            to_json(project_lifecycle::start_project(ctx, args.id).await?)
        }
        "stop_project" => {
            let args: IdArg = parse_args(body)?;
            to_json(project_lifecycle::stop_project(ctx, args.id).await?)
        }
        "delete_project" => {
            let args: IdArg = parse_args(body)?;
            project_lifecycle::delete_project(ctx, args.id).await?;
            Ok(Value::Null)
        }
        "reset_project_data" => {
            let args: IdArg = parse_args(body)?;
            to_json(project_lifecycle::reset_project_data(ctx, args.id).await?)
        }
        "open_project" => {
            let args: IdArg = parse_args(body)?;
            to_json(project_lifecycle::open_project(ctx, args.id, false, remote_client).await?)
        }
        "set_project_tailscale" => {
            let args: SetTailscaleArgs = parse_args(body)?;
            to_json(project_lifecycle::set_project_tailscale(ctx, args.id, args.enabled).await?)
        }

        "project_runtime_settings" => {
            let args: RuntimeSettingsArgs = parse_args(body)?;
            to_json(
                runtime_settings::project_runtime_settings(ctx, args.id, args.environment).await?,
            )
        }
        "change_project_runtime_settings" => {
            let args: ChangeRuntimeSettingsArgs = parse_args(body)?;
            to_json(
                runtime_settings::change_project_runtime_settings(
                    ctx,
                    args.id,
                    args.environment,
                    args.change,
                )
                .await?,
            )
        }

        "resource_snapshot" => to_json(resource_monitor::resource_snapshot(ctx).await?),

        "add_github_pocket" => {
            let args: AddGitHubArgs = parse_args(body)?;
            to_json(
                github_install::add_github_pocket(
                    ctx,
                    &args.source,
                    args.dest_parent.map(PathBuf::from),
                )
                .await?,
            )
        }

        "browse_directory" => {
            let args: BrowseDirectoryArgs = parse_args(body)?;
            tokio::task::spawn_blocking(move || {
                fs_browser::browse_directory(args.path.as_deref(), args.show_hidden)
            })
            .await
            .map_err(|_| "Reading folder contents was interrupted.".to_owned())??
            .pipe(to_json)
        }

        // Web daemon uses deterministic icon/accent cards rather than macOS WKWebView captures.
        "pocket_thumb" => Ok(Value::Null),
        "pocket_thumb_capture" | "project_preview_set_bounds" | "project_preview_hide" => {
            Ok(json!(null))
        }

        other => Err(format!("Unknown Pockets command '{other}'.")),
    }
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}

impl<T> Pipe for T {}
