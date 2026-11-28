//! Seat executor dimension (FLEETD-EXECUTOR-001, design note D6) plus the
//! libtmux integration pilot (design note D5).
//!
//! Every seat record carries an `executor`: which environment actually runs
//! the agent process. `tmux` is the default (every seat today); container /
//! native / vm are registered facts — the dispatch router knows how to route
//! them, the delivery paths land with the container pilot ticket.
//!
//! libtmux (pin =0.1.2, D5 survey): new code may use this typed API for tmux
//! queries; the existing `agent::tmux_run` call sites stay untouched (the two
//! faces coexist, migration is per-call-site over time). Control-mode is NOT
//! compiled in: the local daemon (3.6a) predates the 3.7 requirement, so the
//! `control-mode` feature stays off — the basic subprocess object model is
//! what ships.

use anyhow::{Context, Result};

/// Which environment a seat's agent process runs in. Wire names are the
/// lowercase forms (`tmux` / `container` / `native` / `vm`); `tmux` is the
/// default for every seat that never registered an explicit environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Executor {
    /// tmux session pane (today's every seat).
    #[default]
    Tmux,
    /// docker / podman container.
    Container,
    /// bare process, no tmux wrapper.
    Native,
    /// virtual machine.
    Vm,
}

impl Executor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Executor::Tmux => "tmux",
            Executor::Container => "container",
            Executor::Native => "native",
            Executor::Vm => "vm",
        }
    }

    pub fn from_str(s: &str) -> Option<Executor> {
        match s {
            "tmux" => Some(Executor::Tmux),
            "container" => Some(Executor::Container),
            "native" => Some(Executor::Native),
            "vm" => Some(Executor::Vm),
            _ => None,
        }
    }
}

/// Typed tmux session names via the libtmux object model (the pilot's real
/// call face). `socket_name` maps to tmux `-L` (None = the default socket).
/// Errors propagate — an unreachable tmux server must never read as "no
/// sessions" to a reconciler.
///
/// Unix-only: the libtmux crate itself refuses to compile on non-unix
/// targets (`compile_error!`), so the dependency is gated in Cargo.toml and
/// this face follows.
#[cfg(unix)]
pub fn tmux_session_names(socket_name: Option<&str>) -> Result<Vec<String>> {
    let mut builder = libtmux::Server::builder();
    if let Some(name) = socket_name {
        builder = builder.socket_name(name);
    }
    let server = builder.build().context("build libtmux server handle")?;
    let runtime = libtmux::blocking::Runtime::new().context("start libtmux blocking runtime")?;
    let sessions = runtime
        .run(server.sessions())
        .context("libtmux list sessions")?;
    Ok(sessions
        .iter()
        .map(|s| s.name().to_string_lossy().into_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_wire_names_roundtrip() {
        for name in ["tmux", "container", "native", "vm"] {
            let e = Executor::from_str(name).unwrap_or_else(|| panic!("{name} parses"));
            assert_eq!(e.as_str(), name);
        }
        assert_eq!(Executor::default(), Executor::Tmux);
        // common near-misses are rejected, not silently aliased
        assert_eq!(Executor::from_str("docker"), None);
        assert_eq!(Executor::from_str(""), None);
        assert_eq!(Executor::from_str("TMUX"), None);
    }

    /// Real libtmux call (acceptance 3): spin an isolated tmux server on its
    /// own `-L` socket, list sessions through the typed API, and align the
    /// result with plain `tmux ls` output. Never touches the live seat
    /// server — zero ledger/hook side effects.
    #[test]
    #[cfg(unix)] // libtmux dep is unix-gated in Cargo.toml (windows ci)
    fn libtmux_session_list_aligns_with_tmux_ls() {
        let sock = "fleetd-libtmux-probe";
        let run = |args: &[&str]| {
            std::process::Command::new("tmux")
                .args(["-L", sock])
                .args(args)
                .output()
                .expect("spawn tmux")
        };
        let _ = run(&["kill-server"]); // leftover from a crashed prior run
        let created = run(&[
            "new-session",
            "-d",
            "-s",
            "probe-sess",
            "-x",
            "80",
            "-y",
            "24",
        ]);
        assert!(
            created.status.success(),
            "tmux new-session failed: {created:?}"
        );

        let typed = tmux_session_names(Some(sock)).expect("libtmux typed session list");
        assert!(
            typed.iter().any(|n| n == "probe-sess"),
            "typed list {typed:?} misses the session we just created"
        );

        // ground truth: parse the same server's `tmux ls` lines (name before
        // the first colon) and compare as sets
        let ls = run(&["ls", "-F", "#{session_name}"]);
        let raw = String::from_utf8_lossy(&ls.stdout);
        let mut cli: Vec<String> = raw.lines().map(|l| l.trim().to_string()).collect();
        cli.sort();
        let mut typed_sorted = typed.clone();
        typed_sorted.sort();
        assert_eq!(typed_sorted, cli, "typed API and tmux ls disagree");

        let _ = run(&["kill-server"]);
    }

    /// Same alignment on the DEFAULT socket (the live seat server, when one
    /// is running): the typed names must equal `tmux ls -F` names exactly.
    /// Skips when no default server exists (CI / no seats) — the isolated
    /// probe above is the always-on evidence.
    #[test]
    #[cfg(unix)] // libtmux dep is unix-gated in Cargo.toml (windows ci)
    fn libtmux_default_socket_matches_live_tmux_ls() {
        let ls = std::process::Command::new("tmux")
            .args(["ls", "-F", "#{session_name}"])
            .output()
            .expect("spawn tmux");
        if !ls.status.success() {
            eprintln!("no default tmux server running; skipping live alignment");
            return;
        }
        let raw = String::from_utf8_lossy(&ls.stdout);
        let mut cli: Vec<String> = raw.lines().map(|l| l.trim().to_string()).collect();
        cli.sort();
        let mut typed = tmux_session_names(None).expect("libtmux default-socket list");
        typed.sort();
        assert_eq!(typed, cli, "typed API and live tmux ls disagree");
    }
}
