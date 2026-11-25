//! Pockets CLI & cross-platform local web dashboard daemon.

mod rpc;
mod server;
mod service;

use std::path::PathBuf;

use pockets_core::{
    creation, development_commands, fs_browser, github_install, project_lifecycle,
    runtime::RuntimeState, tailscale, AppContext,
};

struct Cli {
    data_dir: Option<PathBuf>,
    command: Option<Commands>,
}

#[derive(Debug, Clone)]
struct ServerFlags {
    port: u16,
    no_bootstrap: bool,
    dev_origin: Option<String>,
    public_url: Option<String>,
    /// `Some(true)` for `--tailscale`, `Some(false)` for `--no-tailscale`, `None` for auto.
    tailscale: Option<bool>,
}

enum Commands {
    Help,
    Version,
    Open(ServerFlags),
    Serve(ServerFlags),
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
    Add {
        source: String,
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
        flags: ServerFlags,
    },
    Tailscale {
        action: TailscaleAction,
        port: u16,
    },
    Url,
}

enum ServiceAction {
    Print,
    Install,
    Uninstall,
}

enum TailscaleAction {
    Status,
    On,
    Off,
}

const HELP_TEXT: &str = "\
Pockets — Local-first home for personal self-hosted web apps.

Usage:
  pockets [OPTIONS] [COMMAND]

Commands:
  open [--port PORT] [--tailscale] [--public-url URL] [--no-bootstrap]
                                          Start daemon (if needed) and open dashboard (default)
  serve [--port PORT] [--tailscale] [--public-url URL] [--dev-origin URL]
                                          Run local web dashboard daemon in foreground
  tailscale [on|off|status] [--port PORT] Manage private Tailscale HTTPS access for the dashboard
  doctor [--bootstrap]                    Check Podman, Tailscale, and local environment health
  list                                    List registered Pockets
  new <NAME> [--parent DIR] [--folder SLUG] [--brief TEXT]
                                          Create a new Pocket starter folder and register it
  link [PATH] [--id ID]                   Link an existing Pocket folder (default: .)
  add <OWNER/REPO | GITHUB_URL>           Clone & register a Pocket from GitHub
  start <ID>                              Start an installed Pocket
  stop <ID>                               Stop a running Pocket
  status <ID>                             Show status for an installed Pocket
  service [install|uninstall|print] [--port PORT] [--tailscale] [--public-url URL]
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
        "open" => Commands::Open(parse_server_flags(args)?),
        "serve" => Commands::Serve(parse_server_flags(args)?),
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
        "add" => Commands::Add {
            source: args
                .first()
                .ok_or("Usage: pockets add <OWNER/REPO | https://github.com/OWNER/REPO>")?
                .clone(),
        },
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
            let flags = parse_server_flags(rest_args)?;
            Commands::Service { action, flags }
        }
        "tailscale" => {
            let (action, rest_args) = match args.first().map(String::as_str) {
                Some("on") | Some("enable") => (TailscaleAction::On, &args[1..]),
                Some("off") | Some("disable") => (TailscaleAction::Off, &args[1..]),
                Some("status") => (TailscaleAction::Status, &args[1..]),
                None => (TailscaleAction::Status, args),
                Some(flag) if flag.starts_with('-') => (TailscaleAction::Status, args),
                Some(other) => {
                    return Err(format!(
                        "Unknown tailscale action '{other}'. Use `pockets tailscale [on|off|status]`."
                    ));
                }
            };
            let flags = parse_server_flags(rest_args)?;
            Commands::Tailscale {
                action,
                port: flags.port,
            }
        }
        "url" => Commands::Url,
        other => return Err(format!("Unknown command '{other}'. Run `pockets --help`.")),
    };

    Ok(Cli {
        data_dir,
        command: Some(command),
    })
}

fn default_server_flags() -> ServerFlags {
    let public_url = std::env::var("POCKETS_PUBLIC_URL")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let tailscale = std::env::var("POCKETS_TAILSCALE_DASHBOARD")
        .ok()
        .and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        });
    ServerFlags {
        port: server::DEFAULT_PORT,
        no_bootstrap: false,
        dev_origin: None,
        public_url,
        tailscale,
    }
}

fn parse_server_flags(args: &[String]) -> Result<ServerFlags, String> {
    let mut flags = default_server_flags();
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--port" => {
                let raw = args.get(idx + 1).ok_or("Missing value for --port")?;
                flags.port = raw
                    .parse::<u16>()
                    .map_err(|_| format!("Invalid port '{raw}'"))?;
                idx += 2;
            }
            "--no-bootstrap" => {
                flags.no_bootstrap = true;
                idx += 1;
            }
            "--dev-origin" => {
                flags.dev_origin = Some(
                    args.get(idx + 1)
                        .ok_or("Missing value for --dev-origin")?
                        .clone(),
                );
                idx += 2;
            }
            "--public-url" => {
                flags.public_url = Some(
                    args.get(idx + 1)
                        .ok_or("Missing value for --public-url")?
                        .clone(),
                );
                idx += 2;
            }
            "--tailscale" => {
                flags.tailscale = Some(true);
                idx += 1;
            }
            "--no-tailscale" => {
                flags.tailscale = Some(false);
                idx += 1;
            }
            other => return Err(format!("Unknown option: {other}")),
        }
    }
    Ok(flags)
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
        println!("pockets {}", pockets_core::APP_VERSION);
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
        None => run_open(ctx, default_server_flags()).await,
        Some(Commands::Help | Commands::Version) => Ok(()),
        Some(Commands::Open(flags)) => run_open(ctx, flags).await,
        Some(Commands::Serve(flags)) => run_serve(ctx, flags).await,
        Some(Commands::Doctor { bootstrap }) => run_doctor(ctx, bootstrap).await,
        Some(Commands::List) => run_list(ctx).await,
        Some(Commands::New {
            name,
            parent,
            folder,
            brief,
        }) => run_new(ctx, name, parent, folder, brief).await,
        Some(Commands::Link { path, id }) => run_link(ctx, path, id).await,
        Some(Commands::Add { source }) => run_add(ctx, source).await,
        Some(Commands::Start { id }) => run_start(ctx, id).await,
        Some(Commands::Stop { id }) => run_stop(ctx, id).await,
        Some(Commands::Status { id }) => run_status(ctx, id).await,
        Some(Commands::Service { action, flags }) => run_service(ctx, action, flags).await,
        Some(Commands::Tailscale { action, port }) => run_tailscale(ctx, action, port).await,
        Some(Commands::Url) => run_url(ctx).await,
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn is_headless_session() -> bool {
    let has_ssh = std::env::var_os("SSH_CONNECTION").is_some_and(|v| !v.is_empty())
        || std::env::var_os("SSH_TTY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("SSH_CLIENT").is_some_and(|v| !v.is_empty());
    if has_ssh {
        return true;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let has_display = std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
            || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
        !has_display
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        false
    }
}

fn should_enable_tailscale_dashboard(flags: &ServerFlags, headless: bool) -> bool {
    match flags.tailscale {
        Some(explicit) => explicit,
        None => headless && tailscale::find_tailscale().is_some(),
    }
}

fn ssh_target_hint() -> String {
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "user".to_owned());
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "<vm-host>".to_owned());
    format!("{user}@{host}")
}

async fn print_and_maybe_open_dashboard(
    port: u16,
    token: &str,
    remote_url: Option<&str>,
    headless: bool,
    open_browser: bool,
) {
    let local_bootstrap = server::bootstrap_url(port, token);
    let local_dash = server::dashboard_url(port);

    if let Some(remote) = remote_url {
        let remote_bootstrap = server::bootstrap_url_for(remote, token);
        let remote_bookmark = format!("{}/", remote.trim_end_matches('/'));
        println!("Pockets dashboard (remote / Tailscale):");
        println!("  Sign-in URL (open once): {remote_bootstrap}");
        println!("  Dashboard bookmark:      {remote_bookmark}");
        if !headless {
            println!("Local URL:                 {local_dash}");
            println!("Local sign-in URL:         {local_bootstrap}");
            if open_browser {
                if let Err(err) = pockets_core::open_in_browser(&local_bootstrap).await {
                    println!("Could not open default browser automatically ({err})");
                }
            }
        }
        return;
    }

    if headless {
        let target = ssh_target_hint();
        println!("Pockets is running on 127.0.0.1:{port} (headless / SSH session detected).");
        println!("To open from your laptop over SSH:");
        println!("  ssh -N -L {port}:127.0.0.1:{port} {target}");
        println!("Then open once in your browser:");
        println!("  {local_bootstrap}");
        println!(
            "Tip: For direct HTTPS access from any device (no SSH tunnel needed), \
             install Tailscale (`sudo tailscale up --operator=$USER`) and run `pockets tailscale on`."
        );
        return;
    }

    println!("Pockets dashboard running at {local_dash}");
    println!("Open in any browser: {local_bootstrap}");
    if open_browser {
        if let Err(err) = pockets_core::open_in_browser(&local_bootstrap).await {
            println!("Could not open default browser automatically ({err})");
        }
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
            if let Some(remote) = existing.remote_url.as_deref() {
                println!("{}", server::bootstrap_url_for(remote, &existing.token));
                if !is_headless_session() {
                    println!("{}", server::bootstrap_url(existing.port, &existing.token));
                }
            } else {
                println!("{}", server::bootstrap_url(existing.port, &existing.token));
            }
            return Ok(());
        }
    }
    let token = server::load_or_create_session_token(&ctx)?;
    if let Some(ts_url) = tailscale::active_port_serve_url(server::DEFAULT_PORT).await {
        println!(
            "{} (start the daemon first with `pockets`)",
            server::bootstrap_url_for(&ts_url, &token)
        );
    } else {
        println!(
            "{} (start the daemon first with `pockets`)",
            server::bootstrap_url(server::DEFAULT_PORT, &token)
        );
    }
    Ok(())
}

async fn try_open_existing_daemon(ctx: &AppContext, headless: bool) -> bool {
    if let Some(existing) = server::read_daemon_record(ctx) {
        if server::probe_running_daemon(&existing).await {
            print_and_maybe_open_dashboard(
                existing.port,
                &existing.token,
                existing.remote_url.as_deref(),
                headless,
                true,
            )
            .await;
            return true;
        }
    }
    false
}

async fn run_open(ctx: AppContext, flags: ServerFlags) -> Result<(), String> {
    let headless = is_headless_session();
    if try_open_existing_daemon(&ctx, headless).await {
        return Ok(());
    }

    maybe_bootstrap_podman(flags.no_bootstrap).await;

    // A background service (e.g. `brew services start pockets`) may have
    // finished binding while we waited for Podman to start.
    if try_open_existing_daemon(&ctx, headless).await {
        return Ok(());
    }

    let token = server::load_or_create_session_token(&ctx)?;
    let enable_tailscale = should_enable_tailscale_dashboard(&flags, headless);

    let (tx, rx) = tokio::sync::oneshot::channel::<server::DaemonReady>();
    let open_token = token.clone();
    tokio::spawn(async move {
        if let Ok(ready) = rx.await {
            print_and_maybe_open_dashboard(
                ready.port,
                &open_token,
                ready.remote_url.as_deref(),
                headless,
                true,
            )
            .await;
            println!("Press Ctrl+C to stop the dashboard daemon (installed Pockets keep running).");
        }
    });

    let options = server::DaemonOptions {
        port: flags.port,
        dev_origin: flags.dev_origin,
        public_url: flags.public_url,
        enable_tailscale,
        token,
    };
    match server::run_daemon(ctx.clone(), options, Some(tx)).await {
        Ok(()) => Ok(()),
        Err(err) => {
            // If another daemon bound the port in the final millisecond, open it
            // cleanly instead of surfacing an `Address already in use` error.
            if try_open_existing_daemon(&ctx, headless).await {
                return Ok(());
            }
            Err(err)
        }
    }
}

async fn run_serve(ctx: AppContext, flags: ServerFlags) -> Result<(), String> {
    let token = server::load_or_create_session_token(&ctx)?;
    let enable_tailscale = flags.tailscale.unwrap_or(false);

    let (tx, rx) = tokio::sync::oneshot::channel::<server::DaemonReady>();
    let print_token = token.clone();
    let print_dev_origin = flags.dev_origin.clone();
    let no_bootstrap = flags.no_bootstrap;
    tokio::spawn(async move {
        if let Ok(ready) = rx.await {
            println!("Pockets daemon listening on http://127.0.0.1:{}", ready.port);
            if let Some(dev) = print_dev_origin.as_deref() {
                println!(
                    "Dev dashboard URL: {}",
                    server::bootstrap_url_for(dev, &print_token)
                );
            }
            println!(
                "Local dashboard URL: {}",
                server::bootstrap_url(ready.port, &print_token)
            );
            if let Some(remote) = ready.remote_url.as_deref() {
                println!(
                    "Remote dashboard URL: {}",
                    server::bootstrap_url_for(remote, &print_token)
                );
            }
            // Bootstrap Podman after the loopback listener is bound so
            // `brew services start pockets && pockets` immediately discovers
            // the running daemon instead of racing to bind port 18786.
            maybe_bootstrap_podman(no_bootstrap).await;
        }
    });

    let options = server::DaemonOptions {
        port: flags.port,
        dev_origin: flags.dev_origin,
        public_url: flags.public_url,
        enable_tailscale,
        token,
    };
    server::run_daemon(ctx, options, Some(tx)).await
}

async fn run_doctor(ctx: AppContext, bootstrap: bool) -> Result<(), String> {
    println!("Pockets v{}", pockets_core::APP_VERSION);
    println!("OS: {} ({})", std::env::consts::OS, std::env::consts::ARCH);
    println!(
        "Session: {}",
        if is_headless_session() {
            "headless / SSH"
        } else {
            "desktop"
        }
    );
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

    let tailscale_report = pockets_core::detect_tailscale(&ctx).await?;
    println!(
        "Tailscale: {:?} — {} (dns={:?})",
        tailscale_report.state, tailscale_report.message, tailscale_report.dns_name
    );

    if let Some(linger) = service::check_linux_linger().await {
        if linger {
            println!("Systemd linger: enabled (daemon & containers survive SSH disconnect)");
        } else {
            let user = std::env::var("USER").unwrap_or_else(|_| "$USER".to_owned());
            println!(
                "Systemd linger: disabled — run `sudo loginctl enable-linger {user}` so Pockets stays running after SSH disconnect"
            );
        }
    }

    if let Some(record) = server::read_daemon_record(&ctx) {
        let alive = server::probe_running_daemon(&record).await;
        println!(
            "Daemon: {} (port {}, pid {})",
            if alive { "running" } else { "stopped (stale record)" },
            record.port,
            record.pid
        );
        if alive {
            if let Some(remote) = record.remote_url.as_deref() {
                println!("Remote dashboard: {remote}");
            }
        }
    } else {
        println!("Daemon: not running");
    }
    Ok(())
}

async fn run_tailscale(ctx: AppContext, action: TailscaleAction, port: u16) -> Result<(), String> {
    match action {
        TailscaleAction::Status => {
            let report = pockets_core::detect_tailscale(&ctx).await?;
            println!("Tailscale: {:?} — {}", report.state, report.message);
            if let Some(url) = tailscale::active_port_serve_url(port).await {
                println!("Dashboard route: active ({url})");
            } else {
                println!("Dashboard route: inactive (run `pockets tailscale on` to enable)");
            }
            if let Some(portal) = report.portal_url.as_deref() {
                println!("Pocket launcher: {portal}");
            }
            Ok(())
        }
        TailscaleAction::On => {
            let ts_url = tailscale::enable_port_serve(port).await?;
            let token = server::load_or_create_session_token(&ctx)?;
            if let Ok(path) = service::service_file_path() {
                if path.exists() {
                    let config = service::ServiceConfig {
                        port,
                        enable_tailscale: true,
                        public_url: default_server_flags().public_url,
                    };
                    let outcome = service::install(&config).await?;
                    println!(
                        "Updated and restarted background service at {} with Tailscale enabled.",
                        outcome.path.display()
                    );
                }
            }
            println!("Tailscale dashboard enabled:");
            println!(
                "  Sign-in URL (open once): {}",
                server::bootstrap_url_for(&ts_url, &token)
            );
            println!("  Dashboard bookmark:      {ts_url}/");
            Ok(())
        }
        TailscaleAction::Off => {
            tailscale::disable_port_serve(port).await?;
            if let Ok(path) = service::service_file_path() {
                if path.exists() {
                    let config = service::ServiceConfig {
                        port,
                        enable_tailscale: false,
                        public_url: default_server_flags().public_url,
                    };
                    let outcome = service::install(&config).await?;
                    println!(
                        "Updated and restarted background service at {} (loopback only).",
                        outcome.path.display()
                    );
                }
            }
            println!("Disabled Tailscale HTTPS access for the Pockets dashboard.");
            Ok(())
        }
    }
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

async fn run_add(ctx: AppContext, source: String) -> Result<(), String> {
    let project = github_install::add_github_pocket(&ctx, &source, None).await?;
    println!(
        "Added Pocket '{}' (id: {}). Open Pockets to review, configure variables, and click Build & start.",
        project.manifest.metadata.name, project.id
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

async fn run_service(
    ctx: AppContext,
    action: ServiceAction,
    flags: ServerFlags,
) -> Result<(), String> {
    let headless = is_headless_session();
    let enable_tailscale = should_enable_tailscale_dashboard(&flags, headless);
    let config = service::ServiceConfig {
        port: flags.port,
        enable_tailscale,
        public_url: flags.public_url.clone(),
    };
    match action {
        ServiceAction::Print => {
            print!("{}", service::render_template(&config));
            Ok(())
        }
        ServiceAction::Install => {
            let outcome = service::install(&config).await?;
            let token = server::load_or_create_session_token(&ctx)?;
            println!(
                "Installed and started background service at {}",
                outcome.path.display()
            );
            if outcome.linger_enabled == Some(false) {
                let user = std::env::var("USER").unwrap_or_else(|_| "$USER".to_owned());
                println!(
                    "Note: Run once so Pockets stays running after SSH disconnect: sudo loginctl enable-linger {user}"
                );
            }
            // Wait briefly for the freshly started daemon to publish its remote URL if enabled.
            let mut remote_url = flags.public_url.clone();
            if enable_tailscale && remote_url.is_none() {
                for _ in 0..8 {
                    if let Some(rec) = server::read_daemon_record(&ctx) {
                        if rec.remote_url.is_some() && server::probe_running_daemon(&rec).await {
                            remote_url = rec.remote_url;
                            break;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                if remote_url.is_none() {
                    remote_url = tailscale::active_port_serve_url(flags.port).await;
                }
            }
            print_and_maybe_open_dashboard(
                flags.port,
                &token,
                remote_url.as_deref(),
                headless,
                false,
            )
            .await;
            Ok(())
        }
        ServiceAction::Uninstall => {
            let _ = tailscale::disable_port_serve(flags.port).await;
            let path = service::uninstall().await?;
            println!(
                "Stopped and removed background service at {}",
                path.display()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tailscale_and_public_url_flags() {
        let cli = parse_cli(&[
            "serve".to_owned(),
            "--port".to_owned(),
            "19000".to_owned(),
            "--tailscale".to_owned(),
            "--public-url".to_owned(),
            "https://pockets.example.com".to_owned(),
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Serve(flags)) => {
                assert_eq!(flags.port, 19000);
                assert_eq!(flags.tailscale, Some(true));
                assert_eq!(
                    flags.public_url.as_deref(),
                    Some("https://pockets.example.com")
                );
            }
            _ => panic!("expected Serve command"),
        }

        let ts_on = parse_cli(&["tailscale".to_owned(), "on".to_owned()]).unwrap();
        assert!(matches!(
            ts_on.command,
            Some(Commands::Tailscale {
                action: TailscaleAction::On,
                port: server::DEFAULT_PORT
            })
        ));
    }
}
