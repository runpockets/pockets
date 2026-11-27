//! Interactive CLI flow for `pockets uninstall`.

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

use pockets_core::app_update::InstallMethod;
use pockets_core::uninstall::{
    self, PocketsDataCleanupReport, PocketsUninstallOutcome, PodmanInstallKind,
    UninstallInspection,
};
use pockets_core::{tailscale, AppContext};

use crate::{server, service};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UninstallFlags {
    pub yes: bool,
    pub remove_data: Option<bool>,
    pub uninstall_podman: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    pub remove_data: bool,
    pub uninstall_podman: bool,
    pub remove_podman_machines: bool,
}

pub fn parse_uninstall_flags(args: &[String]) -> Result<UninstallFlags, String> {
    let mut flags = UninstallFlags::default();
    for arg in args {
        match arg.as_str() {
            "-y" | "--yes" => flags.yes = true,
            "--remove-data" => {
                if flags.remove_data == Some(false) {
                    return Err("Cannot pass both --remove-data and --keep-data.".to_owned());
                }
                flags.remove_data = Some(true);
            }
            "--keep-data" => {
                if flags.remove_data == Some(true) {
                    return Err("Cannot pass both --remove-data and --keep-data.".to_owned());
                }
                flags.remove_data = Some(false);
            }
            "--uninstall-podman" => {
                if flags.uninstall_podman == Some(false) {
                    return Err(
                        "Cannot pass both --uninstall-podman and --keep-podman.".to_owned()
                    );
                }
                flags.uninstall_podman = Some(true);
            }
            "--keep-podman" => {
                if flags.uninstall_podman == Some(true) {
                    return Err(
                        "Cannot pass both --uninstall-podman and --keep-podman.".to_owned()
                    );
                }
                flags.uninstall_podman = Some(false);
            }
            other => {
                return Err(format!("Unknown option for uninstall: {other}"));
            }
        }
    }
    Ok(flags)
}

pub async fn run_uninstall(ctx: AppContext, flags: UninstallFlags) -> Result<(), String> {
    let inspection = uninstall::inspect(&ctx).await;

    let stdin = io::stdin();
    let stdout = io::stdout();
    let is_tty = stdin.is_terminal();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    let Some(plan) = resolve_plan(&inspection, &flags, is_tty, &mut reader, &mut writer)? else {
        return Ok(());
    };
    drop(reader);
    drop(writer);

    println!();

    // 1. Collect running daemon info, stop background services & running daemon.
    let mut daemon_ports = vec![server::DEFAULT_PORT];
    let running_daemon = server::read_daemon_record(&ctx);
    if let Some(rec) = &running_daemon {
        if !daemon_ports.contains(&rec.port) {
            daemon_ports.push(rec.port);
        }
    }

    if let InstallMethod::Homebrew { prefix, .. } = &inspection.install_method {
        let _ = uninstall::stop_homebrew_service(prefix).await;
    }
    if let Err(err) = service::uninstall().await {
        eprintln!("! Warning: could not remove background service file: {err}");
    }
    if let Some(rec) = &running_daemon {
        if rec.pid != std::process::id() && server::probe_running_daemon(rec).await {
            stop_running_daemon(rec.pid).await;
        }
    }
    println!("✓ Stopped Pockets background service");

    // 2. Remove Pockets-owned Tailscale Serve routes (before data_dir removal).
    match tailscale::disable_all_pockets_routes(&ctx.store(), &daemon_ports).await {
        Ok(0) => {}
        Ok(n) => println!(
            "✓ Removed {n} Tailscale Serve {}",
            plural(n, "route", "routes")
        ),
        Err(err) => eprintln!("! Warning: could not clean up Tailscale Serve routes: {err}"),
    }

    // 3. Remove Pockets containers, volumes, networks, secrets, images, and local data dir.
    if plan.remove_data {
        let report = uninstall::remove_pockets_data(
            &inspection.data_dir,
            inspection.podman_install.podman_bin(),
            inspection.podman_inventory.as_ref(),
            plan.remove_podman_machines,
        )
        .await;
        print_data_cleanup_report(&report);
    }

    // 4. Stop and remove Podman machine VM(s) BEFORE uninstalling the `podman` CLI.
    if plan.remove_podman_machines {
        if let (Some(bin), Some(inv)) = (
            inspection.podman_install.podman_bin(),
            inspection.podman_inventory.as_ref(),
        ) {
            match uninstall::stop_and_remove_podman_machines(bin, &inv.machines).await {
                Ok(n) if n > 0 => println!(
                    "✓ Stopped and removed {} Podman machine {}",
                    n,
                    plural(n, "VM", "VMs")
                ),
                Ok(_) => {}
                Err(err) => eprintln!("! Warning: {err}"),
            }
        }
    }

    // 5. Uninstall Pockets itself BEFORE `brew uninstall podman` so Homebrew
    // does not refuse to uninstall a dependency still required by `pockets`.
    match uninstall::uninstall_pockets_package(&inspection.install_method).await? {
        PocketsUninstallOutcome::UninstalledHomebrew { .. } => {
            println!("✓ Uninstalled Pockets (brew uninstall pockets)");
        }
        PocketsUninstallOutcome::RemovedBinary { exe_path } => {
            println!("✓ Removed Pockets binary ({})", exe_path.display());
        }
        PocketsUninstallOutcome::SkippedDevelopment => {
            println!("• Development build detected — skipped deleting Cargo target binary");
        }
    }

    // 6. Uninstall Podman if requested.
    if plan.uninstall_podman {
        match uninstall::uninstall_podman_package(&inspection.podman_install).await {
            Ok(cmd) => println!("✓ Uninstalled Podman ({cmd})"),
            Err(err) => {
                eprintln!("! Warning: could not uninstall Podman automatically: {err}");
            }
        }
    }

    println!("\nPockets has been uninstalled.");
    Ok(())
}

/// Inspects the environment and interactively prompts the user (or applies
/// explicit CLI flags) to build an [`UninstallPlan`].
pub fn resolve_plan(
    inspection: &UninstallInspection,
    flags: &UninstallFlags,
    is_tty: bool,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<Option<UninstallPlan>, String> {
    if !flags.yes && !is_tty {
        return Err(
            "`pockets uninstall` requires an interactive terminal. For non-interactive use, pass `--yes` (with optional `--remove-data` / `--keep-data` and `--uninstall-podman` / `--keep-podman`)."
                .to_owned(),
        );
    }

    let _ = writeln!(writer, "This will uninstall Pockets from your machine:");
    let _ = writeln!(writer, "  • Stop & remove Pockets background service");
    let _ = writeln!(
        writer,
        "  • Remove Tailscale Serve routes created by Pockets (if any)"
    );
    match &inspection.install_method {
        InstallMethod::Homebrew { prefix, .. } => {
            let _ = writeln!(
                writer,
                "  • Uninstall Pockets (Homebrew: {})",
                prefix.display()
            );
        }
        InstallMethod::Binary { exe_path } => {
            let _ = writeln!(
                writer,
                "  • Remove Pockets binary ({})",
                exe_path.display()
            );
        }
        InstallMethod::Development => {
            let _ = writeln!(
                writer,
                "  • Development build detected (Cargo target binary will not be deleted)"
            );
        }
    }
    let _ = writeln!(writer);
    let _ = writeln!(writer, "Your Pocket source code folders will NOT be touched.");

    if !flags.yes {
        let _ = writeln!(writer);
        if !prompt_yes_no(reader, writer, "Continue uninstalling Pockets? [y/N]: ")? {
            let _ = writeln!(writer, "Cancelled.");
            return Ok(None);
        }
    }

    // Question 1: Pockets data, containers, volumes, images.
    let has_data_to_remove = inspection.data_dir_exists
        || inspection
            .podman_inventory
            .as_ref()
            .is_some_and(|inv| inv.has_pockets_resources() || !inv.engine_reachable);

    let remove_data = match flags.remove_data {
        Some(choice) => choice,
        None if !has_data_to_remove => false,
        None if !is_tty => false,
        None => {
            let _ = writeln!(writer);
            let _ = writeln!(
                writer,
                "Remove Pockets containers, images, data volumes, and local data directory?"
            );
            if inspection.registered_pockets_count > 0 {
                let n = inspection.registered_pockets_count;
                let _ = writeln!(
                    writer,
                    "  • {n} registered {}",
                    plural(n, "Pocket", "Pockets")
                );
            }
            if let Some(inv) = &inspection.podman_inventory {
                if inv.engine_reachable && inv.has_pockets_resources() {
                    let mut parts = Vec::new();
                    if !inv.pockets_containers.is_empty() {
                        let n = inv.pockets_containers.len();
                        parts.push(format!("{n} {}", plural(n, "container", "containers")));
                    }
                    if !inv.pockets_volumes.is_empty() {
                        let n = inv.pockets_volumes.len();
                        parts.push(format!("{n} data {}", plural(n, "volume", "volumes")));
                    }
                    if !inv.pockets_images.is_empty() {
                        let n = inv.pockets_images.len();
                        parts.push(format!("{n} {}", plural(n, "image", "images")));
                    }
                    if !parts.is_empty() {
                        let _ = writeln!(writer, "  • Pockets Podman resources: {}", parts.join(", "));
                    }
                } else if !inv.engine_reachable {
                    let _ = writeln!(
                        writer,
                        "  • Podman engine is currently stopped (cannot inspect Pockets containers/volumes)"
                    );
                }
            }
            if inspection.data_dir_exists {
                let _ = writeln!(
                    writer,
                    "  • Data directory: {} (metadata, secrets vault)",
                    inspection.data_dir.display()
                );
            }
            let _ = writeln!(writer, "  (Your Pocket source folders are never deleted.)");
            prompt_yes_no(reader, writer, "Remove Pockets data? [y/N]: ")?
        }
    };

    // Question 2: Podman (and Podman machine VMs on macOS).
    let (uninstall_podman, remove_podman_machines) = match &inspection.podman_install {
        PodmanInstallKind::NotInstalled => {
            if flags.uninstall_podman == Some(true) {
                let _ = writeln!(writer, "\nPodman is not installed; skipping Podman removal.");
            }
            (false, false)
        }
        PodmanInstallKind::External { reason, .. } => {
            let _ = writeln!(writer);
            let _ = writeln!(writer, "{reason}");
            let _ = writeln!(writer, "Skipping Podman removal.");
            (false, false)
        }
        PodmanInstallKind::Homebrew {
            prefix,
            podman_desktop_present,
            ..
        } => {
            let machines = inspection
                .podman_inventory
                .as_ref()
                .map(|inv| inv.machines.as_slice())
                .unwrap_or(&[]);

            let choice = match flags.uninstall_podman {
                Some(explicit) => explicit,
                None if !is_tty => false,
                None => {
                    let _ = writeln!(writer);
                    let _ = writeln!(
                        writer,
                        "Podman is installed via Homebrew ({}/opt/podman).",
                        prefix.display()
                    );
                    if *podman_desktop_present {
                        let _ = writeln!(
                            writer,
                            "Note: Podman Desktop.app is also installed in /Applications and will not be removed."
                        );
                    }
                    write_podman_workload_status(
                        writer,
                        inspection,
                        remove_data,
                        !machines.is_empty(),
                    );
                    let prompt = podman_prompt_text(inspection, !machines.is_empty());
                    prompt_yes_no(reader, writer, &prompt)?
                }
            };
            (choice, choice && !machines.is_empty())
        }
        PodmanInstallKind::LinuxPackageManager { manager, .. } => {
            let choice = match flags.uninstall_podman {
                Some(explicit) => explicit,
                None if !is_tty => false,
                None => {
                    let _ = writeln!(writer);
                    let _ = writeln!(
                        writer,
                        "Podman is installed via {} (`{}`).",
                        manager.name(),
                        manager.display_remove_command()
                    );
                    write_podman_workload_status(writer, inspection, remove_data, false);
                    let prompt = podman_prompt_text(inspection, false);
                    prompt_yes_no(reader, writer, &prompt)?
                }
            };
            (choice, false)
        }
    };

    Ok(Some(UninstallPlan {
        remove_data,
        uninstall_podman,
        remove_podman_machines,
    }))
}

fn write_podman_workload_status(
    writer: &mut impl Write,
    inspection: &UninstallInspection,
    remove_data: bool,
    has_machines: bool,
) {
    if let Some(inv) = &inspection.podman_inventory {
        if inv.engine_reachable {
            if inv.has_other_workloads() {
                let mut parts = Vec::new();
                if !inv.other_containers.is_empty() {
                    let n = inv.other_containers.len();
                    parts.push(format!("{n} non-Pockets {}", plural(n, "container", "containers")));
                }
                if !inv.other_volumes.is_empty() {
                    let n = inv.other_volumes.len();
                    parts.push(format!("{n} non-Pockets {}", plural(n, "volume", "volumes")));
                }
                let _ = writeln!(
                    writer,
                    "Warning: Podman has other containers or volumes not managed by Pockets:"
                );
                let _ = writeln!(writer, "  • {}", parts.join(", "));
            } else {
                let _ = writeln!(
                    writer,
                    "No other (non-Pockets) containers or volumes were found."
                );
            }
        } else {
            let _ = writeln!(
                writer,
                "Note: The Podman engine is currently stopped, so Pockets could not inspect whether non-Pockets containers or volumes exist."
            );
        }

        if has_machines {
            let names: Vec<&str> = inv.machines.iter().map(|m| m.name.as_str()).collect();
            let _ = writeln!(writer, "Podman machine VM(s): {}", names.join(", "));
            if !remove_data
                && (!inv.pockets_volumes.is_empty()
                    || !inv.engine_reachable
                    || inspection.registered_pockets_count > 0)
            {
                let _ = writeln!(
                    writer,
                    "Warning: You chose to keep Pockets data, but removing the Podman machine VM will also delete Pockets' container data volumes."
                );
            }
        }
    }
}

fn podman_prompt_text(inspection: &UninstallInspection, has_machines: bool) -> String {
    let has_other = inspection
        .podman_inventory
        .as_ref()
        .is_some_and(|inv| inv.engine_reachable && inv.has_other_workloads());
    match (has_machines, has_other) {
        (true, true) => {
            "Also uninstall Podman and remove its machine VM(s)? (Warning: this deletes those non-Pockets containers/volumes too.) [y/N]: ".to_owned()
        }
        (true, false) => "Also uninstall Podman and remove its machine VM(s)? [y/N]: ".to_owned(),
        (false, _) => "Also uninstall Podman? [y/N]: ".to_owned(),
    }
}

fn prompt_yes_no(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    prompt: &str,
) -> Result<bool, String> {
    writer
        .write_all(prompt.as_bytes())
        .and_then(|_| writer.flush())
        .map_err(|err| format!("Could not write prompt: {err}"))?;
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .map_err(|err| format!("Could not read input: {err}"))?;
    if bytes == 0 {
        return Ok(false);
    }
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn print_data_cleanup_report(report: &PocketsDataCleanupReport) {
    let mut parts = Vec::new();
    if report.containers_removed > 0 {
        parts.push(format!(
            "{} {}",
            report.containers_removed,
            plural(report.containers_removed, "container", "containers")
        ));
    }
    if report.volumes_removed > 0 {
        parts.push(format!(
            "{} {}",
            report.volumes_removed,
            plural(report.volumes_removed, "volume", "volumes")
        ));
    }
    if report.networks_removed > 0 {
        parts.push(format!(
            "{} {}",
            report.networks_removed,
            plural(report.networks_removed, "network", "networks")
        ));
    }
    if report.secrets_removed > 0 {
        parts.push(format!(
            "{} {}",
            report.secrets_removed,
            plural(report.secrets_removed, "secret", "secrets")
        ));
    }
    if report.images_removed > 0 {
        parts.push(format!(
            "{} {}",
            report.images_removed,
            plural(report.images_removed, "image", "images")
        ));
    }
    if report.data_dir_removed {
        parts.push("local data directory".to_owned());
    }

    if !parts.is_empty() {
        println!("✓ Removed Pockets {}", parts.join(", "));
    }
    for warning in &report.warnings {
        eprintln!("! Warning: {warning}");
    }
}

async fn stop_running_daemon(pid: u32) {
    let _ = tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use pockets_core::uninstall::{LinuxPackageManager, PodmanInventory, PodmanMachineInfo};

    use super::*;

    fn sample_homebrew_inspection() -> UninstallInspection {
        UninstallInspection {
            install_method: InstallMethod::Homebrew {
                prefix: PathBuf::from("/opt/homebrew"),
                has_brew_podman: true,
            },
            data_dir: PathBuf::from("/Users/test/Library/Application Support/dev.pockets.desktop"),
            data_dir_exists: true,
            registered_pockets_count: 2,
            podman_install: PodmanInstallKind::Homebrew {
                brew: PathBuf::from("/opt/homebrew/bin/brew"),
                prefix: PathBuf::from("/opt/homebrew"),
                podman_bin: PathBuf::from("/opt/homebrew/bin/podman"),
                podman_desktop_present: false,
            },
            podman_inventory: Some(PodmanInventory {
                engine_reachable: true,
                pockets_containers: vec!["pockets-project-1".to_owned()],
                other_containers: vec![],
                pockets_volumes: vec!["pockets-project-1-data".to_owned()],
                other_volumes: vec![],
                pockets_networks: vec![],
                pockets_secrets: vec![],
                pockets_images: vec!["localhost/pockets-managed-1:build".to_owned()],
                machines: vec![PodmanMachineInfo {
                    name: "podman-machine-default".to_owned(),
                    running: true,
                    is_default: true,
                }],
            }),
        }
    }

    #[test]
    fn parses_uninstall_flags_and_rejects_conflicts() {
        let parsed = parse_uninstall_flags(&[
            "--yes".to_owned(),
            "--remove-data".to_owned(),
            "--uninstall-podman".to_owned(),
        ])
        .unwrap();
        assert_eq!(
            parsed,
            UninstallFlags {
                yes: true,
                remove_data: Some(true),
                uninstall_podman: Some(true),
            }
        );

        assert!(parse_uninstall_flags(&[
            "--remove-data".to_owned(),
            "--keep-data".to_owned()
        ])
        .is_err());
        assert!(parse_uninstall_flags(&[
            "--uninstall-podman".to_owned(),
            "--keep-podman".to_owned()
        ])
        .is_err());
        assert!(parse_uninstall_flags(&["--bogus".to_owned()]).is_err());
    }

    #[test]
    fn non_tty_without_yes_refuses_cleanly() {
        let inspection = sample_homebrew_inspection();
        let mut input = Cursor::new(b"" as &[u8]);
        let mut output = Vec::new();
        let err = resolve_plan(
            &inspection,
            &UninstallFlags::default(),
            false,
            &mut input,
            &mut output,
        )
        .unwrap_err();
        assert!(err.contains("--yes"));
    }

    #[test]
    fn non_tty_with_yes_uses_safe_defaults() {
        let inspection = sample_homebrew_inspection();
        let mut input = Cursor::new(b"" as &[u8]);
        let mut output = Vec::new();
        let plan = resolve_plan(
            &inspection,
            &UninstallFlags {
                yes: true,
                ..Default::default()
            },
            false,
            &mut input,
            &mut output,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            plan,
            UninstallPlan {
                remove_data: false,
                uninstall_podman: false,
                remove_podman_machines: false,
            }
        );
    }

    #[test]
    fn interactive_cancel_stops_immediately() {
        let inspection = sample_homebrew_inspection();
        let mut input = Cursor::new(b"n\n");
        let mut output = Vec::new();
        let plan = resolve_plan(
            &inspection,
            &UninstallFlags::default(),
            true,
            &mut input,
            &mut output,
        )
        .unwrap();
        assert!(plan.is_none());
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Your Pocket source code folders will NOT be touched."));
        assert!(text.contains("Cancelled."));
    }

    #[test]
    fn interactive_homebrew_prompts_for_data_and_podman_vm() {
        let inspection = sample_homebrew_inspection();
        let mut input = Cursor::new(b"y\ny\ny\n");
        let mut output = Vec::new();
        let plan = resolve_plan(
            &inspection,
            &UninstallFlags::default(),
            true,
            &mut input,
            &mut output,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            plan,
            UninstallPlan {
                remove_data: true,
                uninstall_podman: true,
                remove_podman_machines: true,
            }
        );
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("No other (non-Pockets) containers or volumes were found."));
        assert!(text.contains("Also uninstall Podman and remove its machine VM(s)? [y/N]: "));
    }

    #[test]
    fn warns_about_other_workloads_and_kept_volumes() {
        let mut inspection = sample_homebrew_inspection();
        inspection
            .podman_inventory
            .as_mut()
            .unwrap()
            .other_containers
            .push("my-postgres".to_owned());
        let mut input = Cursor::new(b"y\nn\nn\n");
        let mut output = Vec::new();
        let plan = resolve_plan(
            &inspection,
            &UninstallFlags::default(),
            true,
            &mut input,
            &mut output,
        )
        .unwrap()
        .unwrap();
        assert!(!plan.remove_data);
        assert!(!plan.uninstall_podman);
        assert!(!plan.remove_podman_machines);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("1 non-Pockets container"));
        assert!(text.contains("You chose to keep Pockets data, but removing the Podman machine VM"));
    }

    #[test]
    fn external_podman_skips_prompt() {
        let mut inspection = sample_homebrew_inspection();
        inspection.podman_install = PodmanInstallKind::External {
            podman_bin: PathBuf::from("/opt/podman/bin/podman"),
            reason: "Podman appears to be managed outside Homebrew.".to_owned(),
        };
        let mut input = Cursor::new(b"y\ny\n");
        let mut output = Vec::new();
        let plan = resolve_plan(
            &inspection,
            &UninstallFlags::default(),
            true,
            &mut input,
            &mut output,
        )
        .unwrap()
        .unwrap();
        assert!(plan.remove_data);
        assert!(!plan.uninstall_podman);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Skipping Podman removal."));
    }

    #[test]
    fn linux_package_manager_prompts_without_vm() {
        let inspection = UninstallInspection {
            install_method: InstallMethod::Binary {
                exe_path: PathBuf::from("/home/test/.local/bin/pockets"),
            },
            data_dir: PathBuf::from("/home/test/.local/share/pockets"),
            data_dir_exists: true,
            registered_pockets_count: 1,
            podman_install: PodmanInstallKind::LinuxPackageManager {
                manager: LinuxPackageManager::Apt,
                podman_bin: PathBuf::from("/usr/bin/podman"),
            },
            podman_inventory: Some(PodmanInventory {
                engine_reachable: true,
                ..Default::default()
            }),
        };
        let mut input = Cursor::new(b"y\nn\ny\n");
        let mut output = Vec::new();
        let plan = resolve_plan(
            &inspection,
            &UninstallFlags::default(),
            true,
            &mut input,
            &mut output,
        )
        .unwrap()
        .unwrap();
        assert!(!plan.remove_data);
        assert!(plan.uninstall_podman);
        assert!(!plan.remove_podman_machines);
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("sudo apt-get remove -y podman"));
        assert!(text.contains("Also uninstall Podman? [y/N]: "));
    }
}
