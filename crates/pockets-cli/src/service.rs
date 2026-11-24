//! Cross-platform user background service management (macOS launchd, Linux systemd).
//!
//! Installing the background service keeps the Pockets loopback daemon running
//! across logins and reboots so a user's saved URL (`http://pockets.localhost:18786/`)
//! or Dock/PWA icon works immediately without opening a terminal.

use std::path::PathBuf;

use pockets_core::fs_browser;

const MACOS_LABEL: &str = "dev.pockets.daemon";

pub fn render_template(port: u16) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "pockets".to_owned());
    render_template_for(&exe, port)
}

pub fn render_template_for(exe: &str, port: u16) -> String {
    #[cfg(target_os = "macos")]
    {
        let log_path = fs_browser::user_home_dir()
            .map(|h| h.join("Library/Logs/pockets.log").display().to_string())
            .unwrap_or_else(|| "/tmp/pockets.log".to_owned());
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
  </array>
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

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        format!(
            r#"[Unit]
Description=Pockets local self-hosted web app daemon
After=network.target

[Service]
ExecStart={exe} serve --port {port}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
"#
        )
    }

    #[cfg(windows)]
    {
        format!("# Run at login via Task Scheduler:\n{exe} serve --port {port}\n")
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

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Ok(home
            .join(".config")
            .join("systemd")
            .join("user")
            .join("pockets.service"))
    }

    #[cfg(windows)]
    {
        Err("Automatic background service installation is not yet supported on Windows. Run `pockets service print` for Task Scheduler instructions.".to_owned())
    }
}

pub async fn install(port: u16) -> Result<PathBuf, String> {
    let path = service_file_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    let contents = render_template(port);
    std::fs::write(&path, contents)
        .map_err(|err| format!("Could not write {}: {err}", path.display()))?;

    #[cfg(target_os = "macos")]
    {
        let path_str = path.to_string_lossy().into_owned();
        // Unload any prior version cleanly; ignore errors if not previously loaded.
        let _ = run_cmd("launchctl", &["unload", &path_str]).await;
        run_cmd("launchctl", &["load", "-w", &path_str]).await?;
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        run_cmd("systemctl", &["--user", "daemon-reload"]).await?;
        run_cmd("systemctl", &["--user", "enable", "--now", "pockets.service"]).await?;
    }

    Ok(path)
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
        let text = render_template_for("/usr/local/bin/pockets", 18786);
        assert!(text.contains("/usr/local/bin/pockets"));
        assert!(text.contains("18786"));
        assert!(text.contains("serve"));
    }
}
