//! Attach face (FLEETD-ATTACH-001): route `fleetd attach` by the session's
//! recorded form (docs/agent-support.md §4.1). Three shapes:
//! - interactive (tmux session live): print the ONE command a human uses to
//!   attach with their own real terminal — fleetd never proxies the terminal
//!   stream (2026-11-27 user test: programmatic tmux attach passthrough is
//!   garbage ANSI).
//! - headless run-id: resolve the run's JSONL feed (stream.jsonl, or the
//!   claude transcript for `--bg` runs) and render the readable event stream.
//! - nothing: clean error, exit 1 at the CLI layer.
//!
//! `--snapshot` (clean single pane frame) and `--tail` (readable transcript
//! tail) are the read-only faces. Parsing lives in `fleetd_core::attach`
//! (pure); this module owns tmux/file IO and routing.

use crate::headless::RUN_DIR;
use crate::presence;
use fleetd_core::attach as core_attach;
use std::path::PathBuf;
use store::ledger::home_dir;

/// Hard byte budget for one `--tail` / run-feed read (the tail walk stops
/// here even when the event count has not been reached).
const TAIL_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Routing verdict for one `fleetd attach <target>` argument.
#[derive(Debug, PartialEq)]
pub enum Route {
    /// Live tmux session — the human attaches with this command themselves.
    Interactive { session: String },
    /// Headless run directory — parsed readable stream.
    Headless { run_id: String },
    /// No live session and no headless run by that name.
    Nothing,
}

/// True when a tmux session with this exact name exists (never matches a
/// prefix — tmux session targeting is exact).
fn tmux_has_session(name: &str) -> bool {
    let out = std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.trim() == name),
        _ => false,
    }
}

/// Route one target name: interactive session first (a live session outranks
/// a stale run dir), then the headless runs directory, then nothing.
pub fn route(target: &str) -> Route {
    if tmux_has_session(target) {
        return Route::Interactive {
            session: target.to_string(),
        };
    }
    let dir = PathBuf::from(RUN_DIR).join(target);
    if dir.join("stream.jsonl").is_file() || dir.join("session-id").is_file() {
        return Route::Headless {
            run_id: target.to_string(),
        };
    }
    Route::Nothing
}

/// The command a human runs (in their own real terminal) to attach to a live
/// seat session. Local flagship: plain tmux attach; the ssh form is printed
/// by remote callers — fleetd itself is always co-located with its tmux.
pub fn attach_command(session: &str) -> String {
    format!("tmux attach -t {session}")
}

/// `--snapshot <seat>`: clean single-frame text of the pane's current screen
/// (capture-pane -p is already plain text — no ANSI passthrough).
pub fn snapshot(seat: &str) -> Option<String> {
    let out = std::process::Command::new("tmux")
        .args(["capture-pane", "-p", "-t", seat])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Newest transcript for a seat, via the presence layer (same newest-by-mtime
/// rule the classifier uses).
fn seat_transcript(seat: &str) -> Option<PathBuf> {
    fleetd_core::presence::latest_transcript(seat)
}

/// Find a claude transcript by session id: `<session-id>.jsonl` under any
/// project dir. The workdir slug is not recorded for headless runs, so scan
/// the projects root.
fn find_transcript_by_session_id(sid: &str) -> Option<PathBuf> {
    let root = PathBuf::from(home_dir()).join(".claude/projects");
    let want = format!("{sid}.jsonl");
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let p = entry.path();
        if p.file_name()
            .map(|n| n.to_string_lossy() == want)
            .unwrap_or(false)
        {
            return Some(p);
        }
    }
    None
}

/// Resolve the JSONL file a headless run's readable feed comes from: the
/// run's stream.jsonl when present (claude stream-json / codex exec --json),
/// else the claude transcript matching the session id (`--bg` runs print it
/// into session-id at launch).
fn run_feed(run_dir: &PathBuf) -> Option<PathBuf> {
    let stream = run_dir.join("stream.jsonl");
    if stream.is_file() {
        return Some(stream);
    }
    let sid = std::fs::read_to_string(run_dir.join("session-id"))
        .ok()?
        .trim()
        .to_string();
    find_transcript_by_session_id(&sid)
}

/// `--tail <seat> [-n N]`: the readable event stream from the seat's newest
/// transcript. None when the seat has no transcript at all.
pub fn tail_transcript(seat: &str, n: usize) -> Option<String> {
    let path = seat_transcript(seat)?;
    let events = core_attach::tail_events(&path, n, TAIL_MAX_BYTES).ok()?;
    Some(core_attach::render(&events))
}

/// `attach <run-id>`: the headless run's readable event stream (same
/// rendering as --tail, source resolved by run form).
pub fn run_stream(run_id: &str) -> Option<String> {
    let feed = run_feed(&PathBuf::from(RUN_DIR).join(run_id))?;
    let events = core_attach::tail_events(&feed, 50, TAIL_MAX_BYTES).ok()?;
    Some(core_attach::render(&events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_unknown_target_is_nothing() {
        // No such tmux session and no run dir under RUN_DIR.
        let ghost = format!("fleetd-attach-no-such-{}", std::process::id());
        assert_eq!(route(&ghost), Route::Nothing);
    }

    #[test]
    fn attach_command_is_plain_tmux() {
        assert_eq!(attach_command("beacon"), "tmux attach -t beacon");
    }

    #[test]
    fn run_feed_prefers_stream_jsonl() {
        let dir = std::env::temp_dir().join(format!("fleetd-attach-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stream.jsonl"), "{}\n").unwrap();
        assert_eq!(run_feed(&dir), Some(dir.join("stream.jsonl")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_transcript_missing_seat_is_none() {
        let ghost = format!("fleetd-attach-no-such-{}", std::process::id());
        assert_eq!(tail_transcript(&ghost, 10), None);
        assert_eq!(snapshot(&ghost), None);
    }
}
