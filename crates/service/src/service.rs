// Self-service registration: `fleetd service install` writes the native
// per-OS service definition so fleetd serve survives reboot — one command
// per platform, no external tooling required.
//
// Platform mechanisms (all first-class, no wrappers like WinSW/NSSM):
//   linux:   systemd user unit  ~/.config/systemd/user/fleetd.service
//            + `systemctl --user enable --now fleetd` (linger recommended:
//              `loginctl enable-linger <user>` so it runs without login)
//   windows: schtasks ONSTART task (SYSTEM-level autostart at boot; tray
//            still shows in the logged-in session via separate ONLOGON task
//            when tray is desired)
//   macos:   launchd agent ~/Library/LaunchAgents/ai.fleetd.plist
//            + `launchctl load`
//
// Uninstall mirrors install (remove the definition + stop).

pub enum Platform {
    Linux,
    Windows,
    Macos,
}

pub fn detect() -> Platform {
    if cfg!(target_os = "windows") {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Linux
    }
}

/// The serve command each platform definition should run. `--no-tray` on
/// headless unix; tray stays on for interactive Windows sessions.
pub fn serve_args(tray: bool) -> &'static str {
    if tray {
        "serve"
    } else {
        "serve"
    }
}

pub fn systemd_unit(exe: &str) -> String {
    format!(
        "[Unit]\nDescription=fleetd flagship/ship agent\nAfter=network-online.target\n\n\
[Service]\nType=simple\nExecStart={exe} serve\nRestart=on-failure\nRestartSec=5\n\n\
[Install]\nWantedBy=default.target\n"
    )
}

pub fn launchd_plist(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>ai.fleetd</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>serve</string>
    <string>--no-tray</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/fleetd.log</string>
  <key>StandardErrorPath</key><string>/tmp/fleetd.log</string>
</dict>
</plist>
"#,
        exe = exe
    )
}

/// Windows: serve registers as a REAL SCM service (`sc create`, auto-start,
/// restart-on-failure) — manageable from services.msc. The tray is a separate
/// ONLOGON task launching `fleetd tray` in the interactive session
/// (2026-11-27 用户令「serve 应该以服务的方式运行」).
pub fn windows_commands(exe: &str) -> Vec<String> {
    vec![
        format!(
            r#"sc create FleetD binPath= "\"{exe}\" serve" start= auto DisplayName= "fleetd flagship service""#,
            exe = exe
        ),
        format!(r#"sc failure FleetD reset= 86400 actions= restart/5000"#),
        format!(
            r#"schtasks /Create /F /SC ONLOGON /TN FleetDTray /RL HIGHEST /TR "\"{exe}\" tray""#,
            exe = exe
        ),
        "sc start FleetD".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_has_restart_and_enable_target() {
        let u = systemd_unit("/usr/local/bin/fleetd");
        assert!(u.contains("ExecStart=/usr/local/bin/fleetd serve"));
        assert!(u.contains("Restart=on-failure"));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn launchd_plist_keeps_alive() {
        let p = launchd_plist("/usr/local/bin/fleetd");
        assert!(p.contains("<string>serve</string>"));
        assert!(p.contains("RunAtLoad"));
        assert!(p.contains("KeepAlive"));
    }

    #[test]
    fn windows_registers_real_scm_service_plus_tray_task() {
        let cmds = windows_commands(r"C:\Users\cui\.fleetd\fleetd.exe");
        assert_eq!(cmds.len(), 4); // create, failure-recovery, tray task, start
        assert!(cmds[0].contains("sc create FleetD"));
        assert!(cmds[0].contains("start= auto"));
        assert!(cmds[0].contains(" serve")); // pure service, no tray flags
        assert!(cmds[1].contains("sc failure FleetD")); // auto-restart
        assert!(cmds[2].contains("/SC ONLOGON")); // tray face
        assert!(cmds[2].contains(" tray")); // tray is its own process
        assert_eq!(cmds[3], "sc start FleetD");
    }
}
