//! Pockets CLI & cross-platform local web dashboard daemon.

mod rpc;
mod server;
mod service;

use std::path::PathBuf;

use pockets_core::{
    creation, development_commands, fs_browser, project_lifecycle, runtime::RuntimeState,
    AppContext,
};

struct Cli {
    data_dir: Option<PathBuf>,
    command: Option<Commands>,
}

enum Commands {
    Help,
    Version,
    Open {
        port: u16,
        no_bootstrap: bool,
    },
    Serve {
        port: u16,
        dev_origin: Option<String>,
        no_bootstrap: bool,
    },
    Doctor {
        bootstrap: bool,
    },
    List,
    New {
        name: String,
        parent: Option<PathBuf>,
        folder: Option<String>,
        brief: String,
    },
    Link {
        path: PathBuf,
        id: Option<String>,
    },
    Start {
        id: String,
    },
    Stop {
        id: String,
    },
    Status {
        id: String,
    },
    Service {
        action: ServiceAction,
        port: u16,
    },
    Url,
}

enum ServiceAction {
    Print,
    Install,
    Uninstall,
}

const HELP_TEXT: &str = "\
Pockets — Local-first home for personal self-hosted web apps.

Usage:
  pockets [OPTIONS] [COMMAND]

Commands:
  open [--port PORT] [--no-bootstrap]     Start daemon (if needed) and open dashboard (default)
  serve [--port PORT] [--dev-origin URL]  Run local web dashboard daemon in foreground
  doctor [--bootstrap]                    Check Podman, Tailscale, and local environment health
  list                                    List registered Pockets
  new <NAME> [--parent DIR] [--folder SLUG] [--brief TEXT]
                                          Create a new Pocket starter folder and register it
  link [PATH] [--id ID]                   Link an existing Pocket folder (default: .)
  start <ID>                              Start an installed Pocket
  stop <ID>                               Stop a running Pocket
  status <ID>                             Show status for an installed Pocket
  service [install|uninstall|print] [--port PORT]
                                          Install/uninstall/print background service (launchd/systemd)
  url                                     Print authenticated sign-in URL for any browser

Global options:
  --data-dir <DIR>                        Override Pockets data directory (or POCKETS_DATA_DIR)
  -h, --help                              Show this help
  -V, --version                           Show version
";

fn parse_cli(raw: &[String]) -> Result<Cli, String> {
    let mut data_dir = std::env::var_os("POCKETS_DATA_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty());
    let mut rest = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        if arg == "--data-dir" {
            let val = raw
                .get(i + 1)
                .ok_or("Missing value for --data-dir")?;
            data_dir = Some(PathBuf::from(val));
            i += 2;
        } else if let Some(val) = arg.strip_prefix("--data-dir=") {
            data_dir = Some(PathBuf::from(val));
            i += 1;
        } else {
            rest.push(arg.clone());
            i += 1;
        }
    }

    if rest.is_empty() {
        return Ok(Cli {
            data_dir,
            command: None,
        });
    }

    if rest.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Cli {
            data_dir,
            command: Some(Commands::Help),
        });
    }
    if rest[0] == "-V" || rest[0] == "--version" {
        return Ok(Cli {
            data_dir,
            command: Some(Commands::Version),
        });
    }

    let sub = rest[0].as_str();
    let args = &rest[1..];
    let command = match sub {
        "open" => {
            let (port, no_bootstrap, _) = parse_server_flags(args)?;
            Commands::Open { port, no_bootstrap }
        }
        "serve" => {
            let (port, no_bootstrap, dev_origin) = parse_server_flags(args)?;
            Commands::Serve {
                port,
                dev_origin,
                no_bootstrap,
            }
        }
        "doctor" => {
            let mut bootstrap = false;
            for arg in args {
                if arg == "--bootstrap" {
                    bootstrap = true;
                } else {
                    return Err(format!("Unknown option for doctor: {arg}"));
                }
            }
            Commands::Doctor { bootstrap }
        }
        "list" => Commands::List,
        "new" => {
            let mut name = None;
            let mut parent = None;
            let mut folder = None;
            let mut brief = String::new();
            let mut idx = 0;
            while idx < args.len() {
                match args[idx].as_str() {
                    "--parent" => {
                        parent = Some(PathBuf::from(
                            args.get(idx + 1).ok_or("Missing value for --parent")?,
                        ));
                        idx += 2;
                    }
                    "--folder" => {
                        folder = Some(
                            args.get(idx + 1)
                                .ok_or("Missing value for --folder")?
                                .clone(),
                        );
                        idx += 2;
                    }
                    "--brief" => {
                        brief = args
                            .get(idx + 1)
                            .ok_or("Missing value for --brief")?
                            .clone();
                        idx += 2;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("Unknown option for new: {other}"));
                    }
                    positional => {
                        if name.is_some() {
                            return Err(format!("Unexpected argument: {positional}"));
                        }
                        name = Some(positional.to_owned());
                        idx += 1;
                    }
                }
            }
            Commands::New {
                name: name.ok_or("Usage: pockets new <NAME> [--parent DIR] [--folder SLUG]")?,
                parent,
                folder,
                brief,
            }
        }
        "link" => {
            let mut path = PathBuf::from(".");
            let mut id = None;
            let mut idx = 0;
            while idx < args.len() {
                match args[idx].as_str() {
                    "--id" => {
                        id = Some(args.get(idx + 1).ok_or("Missing value for --id")?.clone());
                        idx += 2;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("Unknown option for link: {other}"));
                    }
                    positional => {
                        path = PathBuf::from(positional);
                        idx += 1;
                    }
                }
            }
            Commands::Link { path, id }
        }
        "start" => Commands::Start {
            id: args
                .first()
                .ok_or("Usage: pockets start <ID>")?
                .clone(),
        },
        "stop" => Commands::Stop {
            id: args.first().ok_or("Usage: pockets stop <ID>")?.clone(),
        },
        "status" => Commands::Status {
            id: args
                .first()
                .ok_or("Usage: pockets status <ID>")?
                .clone(),
        },
        "service" => {
            let (action, rest_args) = match args.first().map(String::as_str) {
                Some("install") => (ServiceAction::Install, &args[1..]),
                Some("uninstall") => (ServiceAction::Uninstall, &args[1..]),
                Some("print") => (ServiceAction::Print, &args[1..]),
                _ => (ServiceAction::Print, args),
            };
            let (port, _, _) = parse_server_flags(rest_args)?;
            Commands::Service { action, port }
        }
        "url" => Commands::Url,
        other => return Err(format!("Unknown command '{other}'. Run `pockets --help`.")),
    };

    Ok(Cli {
        data_dir,
        command: Some(command),
    })
}

fn parse_server_flags(args: &[String]) -> Result<(u16, bool, Option<String>), String> {
    let mut port = server::DEFAULT_PORT;
    let mut no_bootstrap = false;
    let mut dev_origin = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--port" => {
                let raw = args.get(idx + 1).ok_or("Missing value for --port")?;
                port = raw
                    .parse::<u16>()
                    .map_err(|_| format!("Invalid port '{raw}'"))?;
                idx += 2;
            }
            "--no-bootstrap" => {
                no_bootstrap = true;
                idx += 1;
            }
            "--dev-origin" => {
                dev_origin = Some(
                    args.get(idx + 1)
                        .ok_or("Missing value for --dev-origin")?
                        .clone(),
                );
                idx += 2;
            }
            other => return Err(format!("Unknown option: {other}")),
        }
    }
    Ok((port, no_bootstrap, dev_origin))
}

#[tokio::main]
async fn main() {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_cli(&raw_args) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(2);
        }
    };

    if matches!(cli.command, Some(Commands::Help)) {
        print!("{HELP_TEXT}");
        return;
    }
    if matches!(cli.command, Some(Commands::Version)) {
        println!("pockets {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let ctx = match cli.data_dir {
        Some(dir) => AppContext::new(dir),
        None => match AppContext::from_default_data_dir() {
            Ok(ctx) => ctx,
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        },
    };

    let result = match cli.command {
        None => run_open(ctx, server::DEFAULT_PORT, false).await,
        Some(Commands::Help | Commands::Version) => Ok(()),
        Some(Commands::Open { port, no_bootstrap }) => run_open(ctx, port, no_bootstrap).await,
        Some(Commands::Serve {
            port,
            dev_origin,
            no_bootstrap,
        }) => run_serve(ctx, port, dev_origin, no_bootstrap).await,
        Some(Commands::Doctor { bootstrap }) => run_doctor(ctx, bootstrap).await,
        Some(Commands::List) => run_list(ctx).await,
        Some(Commands::New {
            name,
            parent,
            folder,
            brief,
        }) => run_new(ctx, name, parent, folder, brief).await,
        Some(Commands::Link { path, id }) => run_link(ctx, path, id).await,
        Some(Commands::Start { id }) => run_start(ctx, id).await,
        Some(Commands::Stop { id }) => run_stop(ctx, id).await,
        Some(Commands::Status { id }) => run_status(ctx, id).await,
        Some(Commands::Service { action, port }) => run_service(ctx, action, port).await,
        Some(Commands::Url) => run_url(ctx).await,
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn maybe_bootstrap_podman(no_bootstrap: bool) {
    if no_bootstrap {
        return;
    }
    let initial = pockets_core::detect_runtime().await;
    if initial.state == RuntimeState::Unavailable {
        eprintln!("Starting local Podman runtime...");
        let after = pockets_core::bootstrap_runtime().await;
        if after.state == RuntimeState::Ready {
            eprintln!("{}", after.message);
        } else {
            eprintln!("warning: {}", after.message);
        }
    }
}

async fn run_url(ctx: AppContext) -> Result<(), String> {
    if let Some(existing) = server::read_daemon_record(&ctx) {
        if server::probe_running_daemon(&existing).await {
            println!("{}", server::bootstrap_url(existing.port, &existing.token));
            return Ok(());
        }
    }
    let token = server::load_or_create_session_token(&ctx)?;
    println!(
        "{} (start the daemon first with `pockets`)",
        server::bootstrap_url(server::DEFAULT_PORT, &token)
    );
    Ok(())
}

async fn run_open(ctx: AppContext, port: u16, no_bootstrap: bool) -> Result<(), String> {
    if let Some(existing) = server::read_daemon_record(&ctx) {
        if server::probe_running_daemon(&existing).await {
            let url = server::bootstrap_url(existing.port, &existing.token);
            println!("Pockets is already running at {}", server::dashboard_url(existing.port));
            println!("Open in any browser: {url}");
            if let Err(err) = pockets_core::open_in_browser(&url).await {
                println!("Could not open default browser automatically ({err})");
            }
            return Ok(());
        }
    }

    maybe_bootstrap_podman(no_bootstrap).await;

    let token = server::load_or_create_session_token(&ctx)?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let open_token = token.clone();
    tokio::spawn(async move {
        if let Ok(bound_port) = rx.await {
            let url = server::bootstrap_url(bound_port, &open_token);
            println!("Pockets dashboard running at {}", server::dashboard_url(bound_port));
            println!("Open in any browser: {url}");
            println!("Press Ctrl+C to stop the dashboard daemon (installed Pockets keep running).");
            if let Err(err) = pockets_core::open_in_browser(&url).await {
                println!("Could not open default browser automatically ({err})");
            }
        }
    });

    server::run_daemon(ctx, port, None, token, Some(tx)).await
}

async fn run_serve(
    ctx: AppContext,
    port: u16,
    dev_origin: Option<String>,
    no_bootstrap: bool,
) -> Result<(), String> {
    maybe_bootstrap_podman(no_bootstrap).await;
    let token = server::load_or_create_session_token(&ctx)?;

    let (tx, rx) = tokio::sync::oneshot::channel();
    let print_token = token.clone();
    tokio::spawn(async move {
        if let Ok(bound_port) = rx.await {
            println!("Pockets daemon listening on http://127.0.0.1:{bound_port}");
            println!("Dashboard URL: {}", server::bootstrap_url(bound_port, &print_token));
        }
    });

    server::run_daemon(ctx, port, dev_origin, token, Some(tx)).await
}

async fn run_doctor(ctx: AppContext, bootstrap: bool) -> Result<(), String> {
    println!("Pockets v{}", env!("CARGO_PKG_VERSION"));
    println!("OS: {} ({})", std::env::consts::OS, std::env::consts::ARCH);
    println!("Data directory: {}", ctx.data_dir().display());
    if let Some(home) = fs_browser::user_home_dir() {
        println!("Home directory: {}", home.display());
    }

    let runtime = if bootstrap {
        pockets_core::bootstrap_runtime().await
    } else {
        pockets_core::detect_runtime().await
    };
    println!("Podman: {:?} — {}", runtime.state, runtime.message);

    let tailscale = pockets_core::detect_tailscale(&ctx).await?;
    println!(
        "Tailscale: {:?} — {} (dns={:?})",
        tailscale.state, tailscale.message, tailscale.dns_name
    );

    if let Some(record) = server::read_daemon_record(&ctx) {
        let alive = server::probe_running_daemon(&record).await;
        println!(
            "Daemon: {} (port {}, pid {})",
            if alive { "running" } else { "stopped (stale record)" },
            record.port,
            record.pid
        );
    } else {
        println!("Daemon: not running");
    }
    Ok(())
}

async fn run_list(ctx: AppContext) -> Result<(), String> {
    let projects = creation::list_projects(&ctx).await?;
    if projects.is_empty() {
        println!("No Pockets registered yet. Run `pockets new <name>` or `pockets link .`.");
        return Ok(());
    }
    for item in projects {
        println!(
            "{}\t{}\t{:?}\tlinked={}",
            item.project.id,
            item.project.manifest.metadata.name,
            item.project.phase,
            item.development_linked
        );
    }
    Ok(())
}

async fn run_new(
    ctx: AppContext,
    name: String,
    parent: Option<PathBuf>,
    folder: Option<String>,
    brief: String,
) -> Result<(), String> {
    let parent_dir = match parent {
        Some(p) => p,
        None => std::env::current_dir()
            .map_err(|err| format!("Could not read current directory: {err}"))?,
    };
    let folder_slug = folder.unwrap_or_else(|| slugify(&name));
    let project =
        development_commands::create_development_project(&ctx, parent_dir.clone(), name, folder_slug.clone(), brief)
            .await?;
    println!(
        "Created Pocket '{}' (id: {}) at {}",
        project.manifest.metadata.name,
        project.id,
        parent_dir.join(folder_slug).display()
    );
    Ok(())
}

async fn run_link(ctx: AppContext, path: PathBuf, id: Option<String>) -> Result<(), String> {
    let canonical = std::fs::canonicalize(&path)
        .map_err(|err| format!("Could not resolve '{}': {err}", path.display()))?;
    let project = development_commands::link_development_folder(&ctx, id, canonical.clone()).await?;
    println!(
        "Linked Pocket '{}' (id: {}) -> {}",
        project.manifest.metadata.name,
        project.id,
        canonical.display()
    );
    Ok(())
}

async fn run_start(ctx: AppContext, id: String) -> Result<(), String> {
    let view = project_lifecycle::start_project(&ctx, id).await?;
    println!(
        "{:?} (ready={}) {}",
        view.status.state,
        view.ready,
        view.status.url.as_deref().unwrap_or("")
    );
    Ok(())
}

async fn run_stop(ctx: AppContext, id: String) -> Result<(), String> {
    let view = project_lifecycle::stop_project(&ctx, id).await?;
    println!("{:?} — {}", view.status.state, view.status.message);
    Ok(())
}

async fn run_status(ctx: AppContext, id: String) -> Result<(), String> {
    let view = project_lifecycle::project_status(&ctx, id).await?;
    println!(
        "{:?} (ready={}) {}",
        view.status.state,
        view.ready,
        view.status.url.as_deref().unwrap_or("")
    );
    Ok(())
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_end_matches('-');
    if trimmed.is_empty() {
        "my-pocket".to_owned()
    } else {
        trimmed.chars().take(48).collect()
    }
}

async fn run_service(ctx: AppContext, action: ServiceAction, port: u16) -> Result<(), String> {
    match action {
        ServiceAction::Print => {
            print!("{}", service::render_template(port));
            Ok(())
        }
        ServiceAction::Install => {
            let path = service::install(port).await?;
            let token = server::load_or_create_session_token(&ctx)?;
            println!("Installed and started background service at {}", path.display());
            println!(
                "Open once to sign in: {}",
                server::bootstrap_url(port, &token)
            );
            println!(
                "After signing in, bookmark {} — it will stay available automatically.",
                server::dashboard_url(port)
            );
            Ok(())
        }
        ServiceAction::Uninstall => {
            let path = service::uninstall().await?;
            println!("Stopped and removed background service at {}", path.display());
            Ok(())
        }
    }
}
