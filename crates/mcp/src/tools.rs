// Agent toolchain lifecycle: install / inventory / version pin / config
// sync for the coding agents fleetd manages (claude, codex, opencode).
//
// Design contract (2026-11-27 user direction): fleet manages the FULL
// lifecycle so humans never hand-replace binaries — every agent's official
// layout keeps multiple versions on disk, so "upgrade" and "rollback" are
// pointer moves, not overwrites:
//   claude:   ~/.local/share/claude/versions/<ver> + ~/.local/bin/claude
//   codex:    ~/.codex/packages/standalone/releases/<triple> + current/
//   opencode: ~/.opencode/bin/opencode (installer-managed)
//
// Remote execution reuses the ssh runner convention ("local" = direct).

use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Codex,
    Opencode,
}

impl Tool {
    pub fn from_str(s: &str) -> Option<Tool> {
        match s {
            "claude" => Some(Tool::Claude),
            "codex" => Some(Tool::Codex),
            "opencode" => Some(Tool::Opencode),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Opencode => "opencode",
        }
    }

    /// The official installer pipeline (never hand-copy binaries): each
    /// command is idempotent and manages its own versions directory.
    pub fn install_cmd(&self) -> &'static str {
        match self {
            // curl-fetched shell installer (claude.ai official)
            Tool::Claude => "curl -fsSL https://claude.ai/install.sh | bash",
            // codex standalone installer (GitHub releases driven)
            Tool::Codex => "curl -fsSL https://developer.openai.com/codex/install.sh | bash -",
            // opencode official installer
            Tool::Opencode => "curl -fsSL https://opencode.ai/install | bash",
        }
    }
}

/// Run a shell pipeline on a host ("local" = direct, else ssh).
fn run_on(host: &str, cmd: &str) -> std::io::Result<std::process::Output> {
    if host == "local" {
        Command::new("bash").args(["-c", cmd]).output()
    } else {
        Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
            .arg(cmd)
            .output()
    }
}

/// Installed version of `tool` on `host`, or "missing". Reads the binary's
/// own --version so the number is always the truth on disk.
pub fn version_on(host: &str, tool: Tool) -> String {
    let name = tool.name();
    let out = run_on(
        host,
        &format!("{name} --version 2>/dev/null || echo missing"),
    );
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                "missing".into()
            } else {
                s
            }
        }
        _ => "missing".into(),
    }
}

/// Inventory one agent across the default fleet hosts.
pub fn inventory(hosts: &[&str], tool: Tool) -> Vec<(String, String)> {
    hosts
        .iter()
        .map(|h| ((*h).to_string(), version_on(h, tool)))
        .collect()
}

/// Install (or upgrade to latest) via the official installer. Blocking: the
/// installer output is captured for the caller to print.
pub fn install(host: &str, tool: Tool) -> Result<String, String> {
    let out = run_on(host, tool.install_cmd()).map_err(|e| e.to_string())?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        return Err(format!(
            "installer exit {:?}: {}",
            out.status.code(),
            text.chars()
                .rev()
                .take(400)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ));
    }
    Ok(version_on(host, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_from_str_roundtrip() {
        assert_eq!(Tool::from_str("claude"), Some(Tool::Claude));
        assert_eq!(Tool::from_str("codex"), Some(Tool::Codex));
        assert_eq!(Tool::from_str("opencode"), Some(Tool::Opencode));
        assert_eq!(Tool::from_str("pi"), None);
    }

    #[test]
    fn installers_are_official_pipelines() {
        // The whole point: fleet never hand-replaces binaries; every
        // install path is the vendor's own idempotent installer.
        for t in [Tool::Claude, Tool::Codex, Tool::Opencode] {
            let cmd = t.install_cmd();
            assert!(cmd.starts_with("curl -fsSL"), "{cmd}");
            assert!(cmd.contains("| bash"), "{cmd}");
        }
    }
}
