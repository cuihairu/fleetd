//! Infra watch (FLEETD-SERVE-001): probe the registered machine list once per
//! round and record state changes into the events table.
//!
//! The blind-spot analysis (2026-11-28) showed the fleet had no facility
//! plane: serve never ran, imac's FleetD service sat STOPPED unnoticed. This
//! module is the detector half — the `infra-check` CLI runs one round and is
//! invoked every 5 minutes by the `fleetd-infra-watch` scheduled job.
//!
//! State-change dedup: a state file maps host -> "up" | "down". An event is
//! recorded only on a transition (first-seen down => infra_down, down => up
//! => infra_up), so a host that stays broken does not spam the events chain.

use anyhow::{Context, Result};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// How long a single TCP connect may take before the host counts as down.
const TCP_TIMEOUT: Duration = Duration::from_secs(3);

/// One registered machine and how to probe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfraHost {
    pub name: &'static str,
    pub kind: ProbeKind,
}

/// The probe face for a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeKind {
    /// This box: flagship TCP port self-check + tmux server process.
    LocalPort,
    /// Linux ship over ssh: a live `fleetd` process must exist.
    SshProcess,
    /// Windows ship over ssh: the FleetD service must report RUNNING.
    WindowsService,
}

/// The registered machine list (FLEETD-SERVE-001 spec): this host,
/// work-runner, and imac. Extended as ships join the fleet.
pub const REGISTERED_HOSTS: &[InfraHost] = &[
    InfraHost {
        name: "local",
        kind: ProbeKind::LocalPort,
    },
    InfraHost {
        name: "work-runner",
        kind: ProbeKind::SshProcess,
    },
    InfraHost {
        name: "imac",
        kind: ProbeKind::WindowsService,
    },
];

/// Default state-file path: ~/.fleetd/infra-watch.json.
pub fn state_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home)
        .join(".fleetd")
        .join("infra-watch.json")
}

/// Extract the service state word from `sc query <svc>` output. The STATE
/// line looks like `        STATE              : 4  RUNNING`; anything that
/// is not RUNNING counts as down (STOPPED / PAUSED / missing).
pub fn parse_sc_query(output: &str) -> Option<&str> {
    for line in output.lines() {
        if line.trim_start().starts_with("STATE") {
            let word = line.rsplit(':').next()?.trim();
            // Strip the numeric code ("4  RUNNING" -> "RUNNING").
            let state =
                word.trim_start_matches(|c: char| c.is_ascii_digit() || c.is_ascii_whitespace());
            return Some(state);
        }
    }
    None
}

/// Decide whether this round needs an event. `prev` is the stored state word
/// (None = first observation of this host).
pub fn transition(prev: Option<&str>, up: bool) -> Option<&'static str> {
    match (prev, up) {
        // A host first seen down is news too — the orchestrator must know.
        (None, false) => Some("infra_down"),
        // Staying down stays quiet; only the fall and the recovery speak.
        (Some("up"), false) => Some("infra_down"),
        (Some("down"), true) => Some("infra_up"),
        _ => None,
    }
}

/// Run one probe against one host. Bounded: TCP 3 s, ssh connect timeout 8 s.
pub fn probe_host(host: &InfraHost) -> bool {
    match host.kind {
        ProbeKind::LocalPort => {
            // Flagship HTTP face on loopback 3790 (FLEETD-SERVE-001).
            let addr = SocketAddr::from(([127, 0, 0, 1], 3790));
            TcpStream::connect_timeout(&addr, TCP_TIMEOUT).is_ok()
                // Facility plane part 2: the tmux server process alive. The
                // server's comm is "tmux: server" (not "tmux"), so match it
                // exactly — a bare `pgrep -x tmux` never fires on Linux.
                && std::process::Command::new("pgrep")
                    .args(["-x", "tmux: server"])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
        }
        ProbeKind::SshProcess => std::process::Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                host.name,
                "pgrep fleetd",
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false),
        ProbeKind::WindowsService => {
            let out = std::process::Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=8",
                    host.name,
                    "sc query FleetD",
                ])
                .output();
            match out {
                Ok(o) => {
                    let text = String::from_utf8_lossy(&o.stdout);
                    parse_sc_query(&text) == Some("RUNNING")
                }
                Err(_) => false,
            }
        }
    }
}

/// Read the persisted per-host state ("up"/"down"), tolerating a missing or
/// corrupt file (first run / partial write).
fn load_state(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap_or_default()).unwrap_or_default()
}

/// One watch round: probe every registered host, record transitions into the
/// events table, and persist the new state. Returns the (host, event) pairs
/// written, for the CLI to print.
pub fn check_round(path: &std::path::Path) -> Result<Vec<(String, String)>> {
    let mut state = load_state(path);
    let mut written = Vec::new();
    for host in REGISTERED_HOSTS {
        let up = probe_host(host);
        let event = transition(state.get(host.name).map(String::as_str), up);
        if let Some(kind) = event {
            let note = if up { "up" } else { "down" };
            let c = store::ledger::open()?;
            store::ledger::record_event(&c, host.name, kind, None, Some(note))
                .with_context(|| format!("record {kind} for {}", host.name))?;
            eprintln!("infra-watch: {} {} ({kind})", host.name, note);
            written.push((host.name.to_string(), kind.to_string()));
        }
        state.insert(host.name.to_string(), if up { "up" } else { "down" }.into());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| "create state dir")?;
    }
    std::fs::write(
        path,
        serde_json::to_string(&state).context("serialize state")?,
    )
    .with_context(|| "write state file")?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sc_query_parser_reads_the_state_word() {
        let running =
            "\r\n        STATE              : 4  RUNNING\r\n        WIN32_EXIT_CODE    : 0";
        assert_eq!(parse_sc_query(running), Some("RUNNING"));
        let stopped = "        STATE              : 1  STOPPED";
        assert_eq!(parse_sc_query(stopped), Some("STOPPED"));
        assert_eq!(parse_sc_query("no state line here"), None);
    }

    #[test]
    fn transitions_only_fire_on_change() {
        // First observation down is an event; first observation up is not.
        assert_eq!(transition(None, false), Some("infra_down"));
        assert_eq!(transition(None, true), None);
        // Up -> down fires; staying down does not spam.
        assert_eq!(transition(Some("up"), false), Some("infra_down"));
        assert_eq!(transition(Some("down"), false), None);
        // Recovery fires exactly once.
        assert_eq!(transition(Some("down"), true), Some("infra_up"));
        assert_eq!(transition(Some("up"), true), None);
    }

    #[test]
    fn registered_hosts_cover_the_spec_list() {
        let names: Vec<_> = REGISTERED_HOSTS.iter().map(|h| h.name).collect();
        assert_eq!(names, vec!["local", "work-runner", "imac"]);
    }
}
