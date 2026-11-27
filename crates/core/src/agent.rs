//! Agent abstraction: how to drive a coding agent (claude / codex / opencode / pi ...)
//! on a remote or local host.
//!
//! DESIGN: an `Agent` is a plugin that knows how to
//!   1. probe how many of its processes are running on a host (capacity),
//!   2. deliver a task into a running session (dispatch),
//!   3. decide "is it busy / is it done" from a terminal pane capture.
//!
//! The scheduler and ledger stay agent-agnostic: they only see the trait.
//! Adding a new agent = one `impl Agent`, registered in `registry()`.

use anyhow::Result;
use std::process::Command;

/// Which agent backend a seat runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Codex,
    Opencode,
    Pi,
    /// Unknown / future agent: conservative defaults, never crashes the fleet.
    Unknown,
}

impl AgentKind {
    pub fn from_str(s: &str) -> AgentKind {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => AgentKind::Claude,
            "codex" => AgentKind::Codex,
            "opencode" => AgentKind::Opencode,
            "pi" => AgentKind::Pi,
            _ => AgentKind::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Opencode => "opencode",
            AgentKind::Pi => "pi",
            AgentKind::Unknown => "unknown",
        }
    }
}

/// What a capacity probe found on one host.
#[derive(Debug, Clone, Copy)]
pub struct Load {
    pub running: usize,
    pub reachable: bool,
}

/// Outcome of delivering a task into a seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Task text reached the seat and it accepted it (esc/queued/done marker seen).
    Delivered,
    /// The seat session is gone.
    NoSession,
    /// Pasted but the seat did not acknowledge (needs a retry / human).
    NotDelivered,
}

/// Outcome of reading a pane to decide busy/idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// A turn is in flight (esc-to-interrupt footer, token stream, spinner).
    Working,
    /// Idle at a bare prompt — safe to dispatch.
    Idle,
    /// No session to inspect.
    Gone,
}

/// The plugin interface. One impl per agent kind; see `registry()`.
pub trait Agent: Send + Sync {
    fn kind(&self) -> AgentKind;

    /// Probe this agent's process count on a host.
    /// `host` is the ssh alias, or "local". Returns Err only when unreachable.
    fn probe_load(&self, host: &str, max_concurrent: usize) -> Result<Load>;

    /// Deliver a one-line task into a named tmux seat on this host.
    fn dispatch(&self, host: &str, seat: &str, task_text: &str) -> Delivery;

    /// Classify a pane capture for this agent's busy/idle markers.
    fn classify(&self, pane_text: &str) -> Busy;
}

/// Build the tmux invocation for a host: local is direct, remote goes through ssh.
pub fn tmux_run(host: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    if host == "local" {
        Command::new("tmux").args(args).output()
    } else {
        let mut cmd = Command::new("ssh");
        cmd.args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host]);
        // remote shell: tmux ...
        cmd.arg(format!("tmux {}", shell_join(args)));
        cmd.output()
    }
}

/// Naive single-quote shell join for remote tmux arg passing.
fn shell_join(args: &[&str]) -> String {
    args.iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One shared implementation of pane classification with per-agent marker sets.
/// Every coding-agent TUI shows a busy footer with a timer while a turn runs;
/// the idle footer has no digit. Spinner rows carry a live token/`↓` stream.
pub struct PaneMarkers {
    pub busy_footer: &'static [&'static str],
    pub spinner: &'static [&'static str],
    pub idle_prompt: &'static [&'static str],
}

fn classify_with(pane: &str, m: &PaneMarkers) -> Busy {
    if pane.contains("can't find window") || pane.contains("no server running") {
        return Busy::Gone;
    }
    // Busy footer: "esc to interrupt · 12s" but not the idle "· ← for agents".
    for marker in m.busy_footer {
        for line in pane.lines() {
            if line.contains(marker) && line.chars().any(|c| c.is_ascii_digit()) {
                return Busy::Working;
            }
        }
    }
    // Spinner rows with a live token stream: "12m 3s · ↓".
    for line in pane.lines() {
        if m.spinner.iter().any(|s| line.contains(s))
            && line.chars().any(|c| c.is_ascii_digit())
            && (line.contains('↓') || line.contains("tokens"))
        {
            return Busy::Working;
        }
    }
    if m.idle_prompt.iter().any(|p| pane.contains(p)) {
        return Busy::Idle;
    }
    // Unknown UI: conservatively treat as busy (never double-dispatch).
    Busy::Working
}

/// Claude Code's TUI markers (verified on this fleet).
pub fn claude_markers() -> PaneMarkers {
    PaneMarkers {
        busy_footer: &["esc to interrupt ·"],
        spinner: &[
            "✻", "✽", "✢", "●", "⏵", "Cogitating", "Musing", "Brewed", "Thinking",
            "Spinning", "Sautéed", "Churned", "Meandering", "Mustering",
        ],
        idle_prompt: &["❯"],
    }
}

/// Codex CLI markers (TUI: "esc to interrupt" footer + spinner; no ❯ on idle).
pub fn codex_markers() -> PaneMarkers {
    PaneMarkers {
        busy_footer: &["esc to interrupt"],
        spinner: &["●", "◌", "thinking", "working", "tokens"],
        idle_prompt: &["> "],
    }
}

/// OpenCode / Pi: conservative — only explicit spinner means busy, prompt means idle.
pub fn plain_markers() -> PaneMarkers {
    PaneMarkers {
        busy_footer: &["esc to interrupt", "ctrl+c to cancel"],
        spinner: &["●", "◌", "thinking", "working"],
        idle_prompt: &["❯", "> "],
    }
}

/// Claude Code agent plugin: probes `pgrep -c claude`, dispatches into tmux,
/// and classifies with the verified Claude TUI markers.
pub struct ClaudeAgent;

impl Agent for ClaudeAgent {
    fn kind(&self) -> AgentKind {
        AgentKind::Claude
    }

    fn probe_load(&self, host: &str, _max: usize) -> Result<Load> {
        let out = if host == "local" {
            Command::new("pgrep").arg("-c").arg("claude").output()?
        } else {
            Command::new("ssh")
                .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
                .arg("pgrep -c claude")
                .output()?
        };
        // pgrep exit 1 = zero matches (reachable, just empty); other = unreachable
        let running = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        let reachable = out.status.success() || out.status.code() == Some(1);
        Ok(Load { running, reachable })
    }

    fn dispatch(&self, host: &str, seat: &str, task_text: &str) -> Delivery {
        // Literal send-keys avoids shell-quoting the task text.
        let _ = tmux_run(host, &["send-keys", "-t", seat, "-l", task_text]);
        let _ = tmux_run(host, &["send-keys", "-t", seat, "C-m"]);
        std::thread::sleep(std::time::Duration::from_millis(3000));
        let pane = tmux_run(host, &["capture-pane", "-p", "-t", seat])
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        if pane.contains("can't find") || pane.contains("no server") {
            return Delivery::NoSession;
        }
        if pane.contains("esc to interrupt")
            || pane.contains("queued messages")
            || pane.contains("✻")
            || pane.contains("✢")
        {
            return Delivery::Delivered;
        }
        Delivery::NotDelivered
    }

    fn classify(&self, pane_text: &str) -> Busy {
        classify_with(pane_text, &claude_markers())
    }
}

/// Generic agent plugin driven by configurable marker sets.
pub struct GenericAgent {
    kind: AgentKind,
    markers: PaneMarkers,
    process_name: &'static str,
}

impl GenericAgent {
    pub fn new(kind: AgentKind, markers: PaneMarkers, process_name: &'static str) -> Self {
        Self { kind, markers, process_name }
    }
}

impl Agent for GenericAgent {
    fn kind(&self) -> AgentKind {
        self.kind
    }

    fn probe_load(&self, host: &str, _max: usize) -> Result<Load> {
        let out = if host == "local" {
            Command::new("pgrep").arg("-c").arg(self.process_name).output()?
        } else {
            Command::new("ssh")
                .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
                .arg(format!("pgrep -c {}", self.process_name))
                .output()?
        };
        let running = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0);
        let reachable = out.status.success() || out.status.code() == Some(1);
        Ok(Load { running, reachable })
    }

    fn dispatch(&self, host: &str, seat: &str, task_text: &str) -> Delivery {
        let _ = tmux_run(host, &["send-keys", "-t", seat, "-l", task_text]);
        let _ = tmux_run(host, &["send-keys", "-t", seat, "C-m"]);
        std::thread::sleep(std::time::Duration::from_millis(3000));
        let pane = tmux_run(host, &["capture-pane", "-p", "-t", seat])
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        if pane.contains("can't find") || pane.contains("no server") {
            return Delivery::NoSession;
        }
        match self.classify(&pane) {
            Busy::Working => Delivery::Delivered,
            _ => Delivery::NotDelivered,
        }
    }

    fn classify(&self, pane_text: &str) -> Busy {
        classify_with(pane_text, &self.markers)
    }
}

/// Plugin registry: the one place that maps agent kind → concrete impl.
/// Adding an agent = one entry here (plus its marker set above).
pub fn registry(kind: AgentKind) -> Box<dyn Agent> {
    match kind {
        AgentKind::Claude => Box::new(ClaudeAgent),
        AgentKind::Codex => Box::new(GenericAgent::new(
            AgentKind::Codex,
            codex_markers(),
            "codex",
        )),
        AgentKind::Opencode => Box::new(GenericAgent::new(
            AgentKind::Opencode,
            plain_markers(),
            "opencode",
        )),
        AgentKind::Pi => Box::new(GenericAgent::new(
            AgentKind::Pi,
            plain_markers(),
            "pi",
        )),
        AgentKind::Unknown => Box::new(GenericAgent::new(
            AgentKind::Unknown,
            plain_markers(),
            "claude", // conservative fallback: probe claude to avoid load-0 pick
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_kinds_case_insensitive() {
        assert_eq!(AgentKind::from_str("Claude"), AgentKind::Claude);
        assert_eq!(AgentKind::from_str(" CODEX "), AgentKind::Codex);
        assert_eq!(AgentKind::from_str("opencode"), AgentKind::Opencode);
        assert_eq!(AgentKind::from_str("pi"), AgentKind::Pi);
        assert_eq!(AgentKind::from_str("something-else"), AgentKind::Unknown);
    }

    #[test]
    fn claude_busy_footer_with_digit() {
        let pane = "● Bash(ls)\n  ⏵⏵ bypass permissions on · esc to interrupt · ← for agents\n❯ ";
        // digit-free footer is idle even if spinner-like char is above
        assert_eq!(classify_with(pane, &claude_markers()), Busy::Idle);
    }

    #[test]
    fn claude_busy_footer_timer() {
        let pane = "● Bash(ls)\n  ⏵⏵ bypass permissions on · esc to interrupt · 12s · ← for agents";
        assert_eq!(classify_with(pane, &claude_markers()), Busy::Working);
    }

    #[test]
    fn claude_token_stream() {
        let pane = "✢ Cogitating… (3m 12s · ↓ 12.4k tokens)\n❯";
        assert_eq!(classify_with(pane, &claude_markers()), Busy::Working);
    }

    #[test]
    fn gone_pane_is_gone() {
        let pane = "can't find window kode";
        assert_eq!(classify_with(pane, &claude_markers()), Busy::Gone);
    }

    #[test]
    fn unknown_ui_is_conservatively_busy() {
        let pane = "some unrecognized agent output";
        assert_eq!(classify_with(pane, &claude_markers()), Busy::Working);
    }

    #[test]
    fn codex_idle_prompt() {
        assert_eq!(classify_with("> ", &codex_markers()), Busy::Idle);
    }
}

pub fn agent_for(spec: &str) -> AgentKind {
    AgentKind::from_str(spec)
}
