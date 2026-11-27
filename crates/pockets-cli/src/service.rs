//! Cross-platform user background service management (macOS launchd, Linux systemd).
//!
//! Installing the background service keeps the Pockets loopback daemon running
//! across logins and reboots so a user's saved URL (`http://pockets.localhost:18786/`)
//! or Dock/PWA icon works immediately without opening a terminal.

use std::path::PathBuf;

use pockets_core::fs_browser;

const MACOS_LABEL: &str = "dev.pockets.daemon";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    pub port: u16,
    pub enable_tailscale: bool,
    pub public_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub path: PathBuf,
    /// On Linux, `Some(true)` if `systemd` user lingering is active,
    /// `Some(false)` if it could not be enabled without sudo, `None` on other OSes.
    pub linger_enabled: Option<bool>,
}

pub fn render_template(config: &ServiceConfig) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "pockets".to_owned());
    render_template_for(&exe, config)
}

pub fn render_template_for(exe: &str, config: &ServiceConfig) -> String {
    let port = config.port;
    let mut extra_args: Vec<String> = Vec::new();
    if config.enable_tailscale {
        extra_args.push("--tailscale".to_owned());
    }
    if let Some(url) = config.public_url.as_deref() {
        extra_args.push("--public-url".to_owned());
        extra_args.push(url.to_owned());
    }

    #[cfg(target_os = "macos")]
    {
        let log_path = fs_browser::user_home_dir()
            .map(|h| h.join("Library/Logs/pockets.log").display().to_string())
            .unwrap_or_else(|| "/tmp/pockets.log".to_owned());
        let extra_xml: String = extra_args
            .iter()
            .map(|arg| format!("    <string>{arg}</string>\n"))
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{MACOS_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>serve</string>
    <string>--port</string>
    <string>{port}</string>
{extra_xml}  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/opt/homebrew/bin:/usr/local/bin:/opt/podman/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log_path}</string>
  <key>StandardErrorPath</key><string>{log_path}</string>
</dict>
</plist>
"#
        )
    }

    #[cfg(not(target_os = "macos"))]
    {
        let suffix = if extra_args.is_empty() {
            String::new()
        } else {
            format!(" {}", extra_args.join(" "))
        };
        format!(
            r#"[Unit]
Description=Pockets local self-hosted web app daemon
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={exe} serve --port {port}{suffix}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
"#
        )
    }
}

pub fn service_file_path() -> Result<PathBuf, String> {
    let home = fs_browser::user_home_dir()
        .ok_or_else(|| "Could not determine user home directory.".to_owned())?;

    #[cfg(target_os = "macos")]
    {
        Ok(home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{MACOS_LABEL}.plist")))
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(home
            .join(".config")
            .join("systemd")
            .join("user")
            .join("pockets.service"))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub async fn check_linux_linger() -> Option<bool> {
    let user = current_unix_user()?;
    let output = tokio::process::Command::new("loginctl")
        .args(["show-user", &user, "-p", "Linger", "--value"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let val = String::from_utf8_lossy(&output.stdout).trim().to_ascii_lowercase();
    match val.as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
pub async fn check_linux_linger() -> Option<bool> {
    None
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn ensure_linux_linger() -> Option<bool> {
    if check_linux_linger().await == Some(true) {
        return Some(true);
    }
    let user = current_unix_user()?;
    let _ = run_cmd("loginctl", &["enable-linger", &user]).await;
    check_linux_linger().await
}

#[cfg(all(unix, not(target_os = "macos")))]
fn current_unix_user() -> Option<String> {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

pub async fn install(config: &ServiceConfig) -> Result<InstallOutcome, String> {
    let path = service_file_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    let contents = render_template(config);
    std::fs::write(&path, contents)
        .map_err(|err| format!("Could not write {}: {err}", path.display()))?;

    #[cfg(target_os = "macos")]
    {
        let path_str = path.to_string_lossy().into_owned();
        // Unload any prior version cleanly; ignore errors if not previously loaded.
        let _ = run_cmd("launchctl", &["unload", &path_str]).await;
        run_cmd("launchctl", &["load", "-w", &path_str]).await?;
        Ok(InstallOutcome {
            path,
            linger_enabled: None,
        })
    }

    #[cfg(not(target_os = "macos"))]
    {
        run_cmd("systemctl", &["--user", "daemon-reload"]).await?;
        run_cmd("systemctl", &["--user", "enable", "pockets.service"]).await?;
        run_cmd("systemctl", &["--user", "restart", "pockets.service"]).await?;
        let linger_enabled = ensure_linux_linger().await;
        Ok(InstallOutcome {
            path,
            linger_enabled,
        })
    }
}

pub async fn uninstall() -> Result<PathBuf, String> {
    let path = service_file_path()?;

    #[cfg(target_os = "macos")]
    {
        if path.exists() {
            let path_str = path.to_string_lossy().into_owned();
            let _ = run_cmd("launchctl", &["unload", "-w", &path_str]).await;
            std::fs::remove_file(&path)
                .map_err(|err| format!("Could not remove {}: {err}", path.display()))?;
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = run_cmd("systemctl", &["--user", "disable", "--now", "pockets.service"]).await;
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|err| format!("Could not remove {}: {err}", path.display()))?;
        }
        let _ = run_cmd("systemctl", &["--user", "daemon-reload"]).await;
    }

    Ok(path)
}

async fn run_cmd(bin: &str, args: &[&str]) -> Result<(), String> {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|err| format!("Failed to run `{bin}`: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "`{bin} {}` failed: {}",
            args.join(" "),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_embeds_executable_and_port() {
        let text = render_template_for(
            "/usr/local/bin/pockets",
            &ServiceConfig {
                port: 18786,
                enable_tailscale: false,
                public_url: None,
            },
        );
        assert!(text.contains("/usr/local/bin/pockets"));
        assert!(text.contains("18786"));
        assert!(text.contains("serve"));
        assert!(!text.contains("--tailscale"));
    }

    #[test]
    fn template_embeds_tailscale_and_public_url_flags_when_configured() {
        let text = render_template_for(
            "/usr/local/bin/pockets",
            &ServiceConfig {
                port: 18786,
                enable_tailscale: true,
                public_url: Some("https://pockets.example.com".to_owned()),
            },
        );
        assert!(text.contains("--tailscale"));
        assert!(text.contains("--public-url"));
        assert!(text.contains("https://pockets.example.com"));
    }
}
