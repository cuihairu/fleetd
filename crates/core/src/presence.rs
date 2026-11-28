//! Presence: seat-liveness classification from transcript tails (pure logic,
//! no process spawning — callers hand in file paths and pane captures).
//!
//! Ports the verified classifier semantics of the orchestrator's temporary
//! `seat_state.py` (validated 12/12 against live seats on 2026-11-28), with
//! one hard requirement the python version misses: transcripts are read
//! tail-only (last ~256 KiB / 200 lines), never loaded whole.
//!
//! States:
//! - ACTIVE   last transcript event < 90 s old (work is flowing)
//! - STUCK    last assistant turn ends in a tool_use with no later
//!            user/tool_result event, command in flight (mtime fresh)
//! - IDLE     last event >= 240 s old and the pane shows no live spinner
//!            (pane capture is the final veto, never the primary signal)
//! - GONE     no transcript at all
//! - DRAINING hysteresis band between ACTIVE and IDLE (90–240 s): recent
//!            activity, not yet provably idle

/// Last transcript event younger than this (seconds) is unconditionally ACTIVE.
pub const ACTIVE_FRESH_SECS: f64 = 90.0;
/// Transcript silent this long (seconds) classifies as IDLE (before the pane veto).
pub const IDLE_AFTER_SECS: f64 = 240.0;
/// A pending tool_use older than this (seconds) is considered abandoned
/// history rather than an in-flight command.
pub const STALE_TOOL_USE_SECS: f64 = 600.0;
/// Classifier never looks at more than the last N lines of a transcript.
pub const TAIL_LINES: usize = 200;
/// Hard byte budget for the backward tail walk: giant single lines (huge
/// tool outputs) stop the walk after this much even when 200 lines have not
/// been reached. Still never the whole file.
const TAIL_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Backward read chunk size for the tail walk.
const TAIL_CHUNK: u64 = 64 * 1024;

/// Liveness state of one seat (lowercase wire form via [`State::as_str`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Draining,
    Idle,
    Stuck,
    Gone,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Active => "active",
            State::Draining => "draining",
            State::Idle => "idle",
            State::Stuck => "stuck",
            State::Gone => "gone",
        }
    }
}

/// Evidence collected while classifying; kept for the presence table and
/// for humans debugging a surprising verdict.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    /// Epoch seconds of the last timestamped transcript event (None = tail
    /// carried no usable timestamps).
    pub last_event_ts: Option<f64>,
    /// Age of that event relative to `now` (None = same fallback rule).
    pub last_event_age: Option<f64>,
    /// File mtime age relative to `now` (None = no file).
    pub mtime_age: Option<f64>,
    /// Tool name if the tail ends in an unresolved tool_use.
    pub pending_tool: Option<String>,
    /// Short human reason for the final state.
    pub signal: String,
}

/// Read only the tail of a (possibly very large) text file: walk backwards
/// in chunks until [`TAIL_LINES`] lines (or the file start, or the byte
/// budget) are collected, then keep exactly the last [`TAIL_LINES`] lines.
/// Never loads the whole file.
pub fn tail_lines(path: &std::path::Path) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut collected: Vec<u8> = Vec::new();
    let mut pos = len;
    let mut reached_start = false;
    while collected.iter().filter(|&&b| b == b'\n').count() < TAIL_LINES + 1 {
        let start = pos.saturating_sub(TAIL_CHUNK);
        let mut chunk = vec![0u8; (pos - start) as usize];
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&collected);
        collected = chunk;
        pos = start;
        if pos == 0 {
            reached_start = true;
            break;
        }
        if collected.len() as u64 >= TAIL_MAX_BYTES {
            break;
        }
    }
    let text = String::from_utf8_lossy(&collected);
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    // A chunk boundary can split the first line; drop the partial head
    // unless we walked to the file start (then it is a whole line).
    if !reached_start && !lines.is_empty() {
        lines.remove(0);
    }
    if lines.len() > TAIL_LINES {
        lines.drain(..lines.len() - TAIL_LINES);
    }
    Ok(lines)
}

/// Parse an RFC 3339 timestamp (python `fromisoformat` compatible: `Z` or
/// explicit offset). Returns epoch seconds.
pub fn iso_to_epoch(ts: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp() as f64 + f64::from(d.timestamp_subsec_millis()) / 1000.0)
}

/// One transcript JSONL event worth of facts, extracted leniently: broken
/// JSON or missing fields simply yield `None` (the python reference skips
/// such lines the same way).
#[derive(Debug, Default, Clone)]
struct EventFacts {
    ts: Option<f64>,
    is_assistant: bool,
    is_user_like: bool, // "user" or "attachment"
    tool_use: Option<String>,
}

fn event_facts(line: &str) -> EventFacts {
    let mut f = EventFacts::default();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return f;
    };
    if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
        f.ts = iso_to_epoch(ts);
    }
    match v.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            f.is_assistant = true;
            if let Some(blocks) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        f.tool_use = Some(
                            b.get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("?")
                                .to_string(),
                        );
                        break;
                    }
                }
            }
        }
        Some("user") | Some("attachment") => f.is_user_like = true,
        _ => {}
    }
    f
}

/// Classify from an in-memory transcript tail. `mtime_age` is the age of the
/// file's mtime relative to `now` — the fallback when the tail carries no
/// usable timestamps. Pure: no clock, no filesystem.
pub fn classify_from_tail(lines: &[String], mtime_age: f64, now: f64) -> (State, Evidence) {
    let mut ev = Evidence {
        mtime_age: Some(mtime_age),
        ..Default::default()
    };

    // Backward walk: last timestamped event + last pending tool_use.
    let mut pending: Option<(f64, String)> = None;
    for line in lines.iter().rev() {
        let facts = event_facts(line);
        if ev.last_event_ts.is_none() {
            if let Some(ts) = facts.ts {
                ev.last_event_ts = Some(ts);
            }
        }
        if pending.is_none() {
            if let Some(tool) = facts.tool_use {
                pending = facts.ts.map(|ts| (ts, tool));
            }
        }
        if ev.last_event_ts.is_some() && pending.is_some() {
            break;
        }
    }
    if let Some(ts) = ev.last_event_ts {
        ev.last_event_age = Some(now - ts);
    } else {
        // No timestamps in the tail: fall back to mtime, python-style.
        ev.last_event_age = Some(mtime_age);
        ev.last_event_ts = Some(now - mtime_age);
    }

    // 1) Fresh event flow = ACTIVE regardless of anything else.
    if ev.last_event_age.unwrap_or(f64::INFINITY) < ACTIVE_FRESH_SECS {
        ev.signal = "fresh-event".into();
        return (State::Active, ev);
    }

    // 2) Pending tool_use only counts from the LAST assistant turn: any
    //    later user/attachment event proves the command already returned.
    let mut last_assistant_idx: Option<usize> = None;
    let mut last_user_idx: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        let facts = event_facts(line);
        if facts.is_assistant {
            last_assistant_idx = Some(idx);
        }
        if facts.is_user_like {
            last_user_idx = Some(idx);
        }
    }
    if let Some((ts, tool)) = pending {
        let cancelled = match (last_assistant_idx, last_user_idx) {
            (Some(a), Some(u)) => u > a,
            _ => false,
        };
        let pend_age = now - ts;
        if !cancelled && pend_age < STALE_TOOL_USE_SECS && mtime_age < IDLE_AFTER_SECS {
            ev.pending_tool = Some(tool.clone());
            ev.signal = format!("pending-tool-use:{tool}");
            return (State::Stuck, ev);
        }
    }

    // 3) Stale beyond IDLE_AFTER => IDLE candidate (pane veto happens above
    //    this function; see `veto_idle`).
    if ev.last_event_age.unwrap_or(f64::INFINITY) >= IDLE_AFTER_SECS {
        ev.signal = "stale-transcript".into();
        return (State::Idle, ev);
    }

    // Between ACTIVE_FRESH and IDLE_AFTER: hysteresis band.
    ev.signal = "hysteresis".into();
    (State::Draining, ev)
}

/// Classify one transcript file (mtime + tail). `now` in epoch seconds.
pub fn classify_transcript(path: &std::path::Path, now: f64) -> std::io::Result<(State, Evidence)> {
    // `elapsed()` is already the age (now - mtime); subtracting it from
    // `now` would yield the epoch and permanently disable the STUCK gate.
    let mtime_age = std::fs::metadata(path)?
        .modified()?
        .elapsed()
        .unwrap_or_default()
        .as_secs_f64();
    let lines = tail_lines(path)?;
    Ok(classify_from_tail(&lines, mtime_age, now))
}

/// True when the pane capture shows nothing contradicting idleness (a live
/// spinner, a running timer, or an interactive menu awaiting the user).
/// Ports `pane_idle` from the reference script — pure text in, bool out, so
/// the caller owns the tmux capture.
pub fn pane_idle(pane: &str) -> bool {
    for line in pane.lines() {
        if let Some(i) = line.find("esc to interrupt · ") {
            let rest = &line[i + "esc to interrupt · ".len()..];
            if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return false;
            }
        }
        // "<N>m <N>s · <arrow>" or "(<N>s · <arrow>" — a live turn timer.
        for arrow in ["↓", "↑", "↗", "↘", "↖", "↙"] {
            if let Some(i) = line.find(arrow) {
                let before = &line[..i];
                if let Some(j) = before.rfind("s · ") {
                    // digits directly before the "s · " marker
                    let digits_end = j;
                    let digits_start = before[..j]
                        .char_indices()
                        .rev()
                        .take_while(|(_, c)| c.is_ascii_digit())
                        .map(|(k, _)| k)
                        .next();
                    if let Some(ds) = digits_start {
                        let _ = ds;
                        // require at least one digit immediately before "s · "
                        if before[..digits_end]
                            .chars()
                            .last()
                            .is_some_and(|c| c.is_ascii_digit())
                        {
                            return false;
                        }
                    }
                }
            }
        }
        if line.contains("⎿ (timeout ") {
            let i = line.find("⎿ (timeout ").unwrap();
            let rest = &line[i + "⎿ (timeout ".len()..];
            if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return false;
            }
        }
        // Interactive menu awaiting user decision: dispatching into it would
        // be swallowed by the menu handler — the seat is busy with the user.
        if line.contains("Enter to select · Tab/Arrow keys to navigate") {
            return false;
        }
        if (line.contains("Esc to cancel")
            || line.contains("按数字选择")
            || line.contains("请选择"))
            && line.contains('❯')
        {
            return false;
        }
    }
    true
}

/// Final veto: an IDLE candidate with a live spinner counts as ACTIVE
/// (python `pane-spinner-overrides`). Anything else passes through.
pub fn veto_idle(state: State, pane: Option<&str>, mut ev: Evidence) -> (State, Evidence) {
    if state == State::Idle {
        if let Some(p) = pane {
            if !pane_idle(p) {
                ev.signal = "pane-spinner-overrides".into();
                return (State::Active, ev);
            }
        }
    }
    (state, ev)
}

/// Claude Code transcript directory for one workspace seat (the reference
/// script scans the same glob).
pub fn projects_dir(seat: &str) -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".claude/projects")
        .join(format!("-home-cui-workspaces-{seat}"))
}

/// Newest transcript for a seat (by mtime), or None when the seat never
/// produced one (GONE territory).
pub fn latest_transcript(seat: &str) -> Option<std::path::PathBuf> {
    let dir = projects_dir(seat);
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .map(|e| e.path())
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
}

/// Open and blocked todo counts for a workspace seat (same rule as the
/// `blocked` verb: `- [ ]` lines; blocked markers BLOCKED / 阻塞 / ⛔).
/// Returns (open, blocked).
pub fn todo_counts(seat: &str) -> (i64, i64) {
    let base = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("workspaces")
        .join(seat);
    let todo = ["todo.md", "TODO.md"]
        .iter()
        .map(|n| base.join(n))
        .find(|p| p.is_file());
    let Some(path) = todo else {
        return (0, 0);
    };
    let Ok(body) = std::fs::read_to_string(path) else {
        return (0, 0);
    };
    let mut open = 0i64;
    let mut blocked = 0i64;
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("- [ ]") {
            open += 1;
            if t.contains("BLOCKED") || t.contains("阻塞") || t.contains('⛔') {
                blocked += 1;
            }
        }
    }
    (open, blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_line(ts: &str, ty: &str) -> String {
        format!(r#"{{"type":"{ty}","timestamp":"{ts}"}}"#)
    }

    fn tool_line(ts: &str, tool: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"content":[{{"type":"tool_use","name":"{tool}"}}]}}}}"#
        )
    }

    const NOW: f64 = 1_800_000_000.0;

    fn ts_ago(secs: f64) -> String {
        let t = NOW - secs;
        let dt = chrono::DateTime::from_timestamp(t as i64, 0).unwrap();
        format!("{}", dt.format("%Y-%m-%dT%H:%M:%SZ"))
    }

    #[test]
    fn fresh_event_is_active() {
        let lines = vec![ev_line(&ts_ago(10.0), "user")];
        let (s, ev) = classify_from_tail(&lines, 10.0, NOW);
        assert_eq!(s, State::Active);
        assert_eq!(ev.signal, "fresh-event");
    }

    #[test]
    fn stale_event_is_idle() {
        let lines = vec![ev_line(&ts_ago(300.0), "user")];
        let (s, ev) = classify_from_tail(&lines, 300.0, NOW);
        assert_eq!(s, State::Idle);
        assert_eq!(ev.signal, "stale-transcript");
    }

    #[test]
    fn hysteresis_band_is_draining() {
        let lines = vec![ev_line(&ts_ago(120.0), "user")];
        let (s, _) = classify_from_tail(&lines, 120.0, NOW);
        assert_eq!(s, State::Draining);
    }

    #[test]
    fn pending_tool_use_is_stuck() {
        // tool_use past the ACTIVE window (90 s) but younger than the stale
        // window (600 s), mtime fresh: the command is in flight.
        let lines = vec![
            ev_line(&ts_ago(150.0), "user"),
            tool_line(&ts_ago(120.0), "Bash"),
        ];
        let (s, ev) = classify_from_tail(&lines, 5.0, NOW);
        assert_eq!(s, State::Stuck);
        assert_eq!(ev.pending_tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn user_reply_after_tool_cancels_stuck() {
        // The tool_use is historic: a later user event proves it returned.
        let lines = vec![
            tool_line(&ts_ago(300.0), "Bash"),
            ev_line(&ts_ago(290.0), "user"),
        ];
        let (s, _) = classify_from_tail(&lines, 290.0, NOW);
        assert_eq!(s, State::Idle);
    }

    #[test]
    fn ancient_tool_use_is_not_stuck() {
        // tool_use older than STALE_TOOL_USE: abandoned history, not in-flight.
        let lines = vec![tool_line(&ts_ago(700.0), "Bash")];
        let (s, _) = classify_from_tail(&lines, 700.0, NOW);
        assert_eq!(s, State::Idle);
    }

    #[test]
    fn stale_mtime_blocks_stuck() {
        // tool_use still inside the stale window, but the file has been
        // quiet beyond IDLE_AFTER: not an in-flight command — IDLE.
        let lines = vec![tool_line(&ts_ago(300.0), "Bash")];
        let (s, _) = classify_from_tail(&lines, 300.0, NOW);
        assert_eq!(s, State::Idle);
    }

    #[test]
    fn no_transcript_is_gone() {
        // classify_transcript on a missing file errors; the caller maps that
        // (and the no-file case) to GONE. Locked in via the helper below.
        let missing = std::env::temp_dir().join("fleetd-presence-gone-test.jsonl");
        let _ = std::fs::remove_file(&missing);
        assert!(classify_transcript(&missing, NOW).is_err());
    }

    #[test]
    fn empty_and_garbled_lines_use_mtime_fallback() {
        let lines = vec!["not json".to_string(), "{broken".to_string()];
        let (s, ev) = classify_from_tail(&lines, 20.0, NOW);
        assert_eq!(s, State::Active);
        assert_eq!(ev.last_event_age, Some(20.0));
    }

    #[test]
    fn lines_without_timestamp_fall_back_to_mtime() {
        let lines = vec![r#"{"type":"user"}"#.to_string()];
        let (s, ev) = classify_from_tail(&lines, 500.0, NOW);
        assert_eq!(s, State::Idle);
        assert_eq!(ev.last_event_age, Some(500.0));
    }

    #[test]
    fn tail_keeps_only_last_lines() {
        let dir = std::env::temp_dir().join(format!("fleetd-presence-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mut body = String::new();
        for i in 0..300 {
            body.push_str(&ev_line(&ts_ago(1000.0), "user"));
            body.push('\n');
        }
        // The very last line is fresh — tail-only reading must find it even
        // though the 300-line history is ancient.
        body.push_str(&ev_line(&ts_ago(3.0), "user"));
        std::fs::write(&path, body).unwrap();
        let lines = tail_lines(&path).unwrap();
        assert!(lines.len() <= TAIL_LINES);
        let (s, _) = classify_transcript(&path, NOW).unwrap();
        assert_eq!(
            s,
            State::Active,
            "fresh tail line must win over old history"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_transcript_uses_file_mtime_age() {
        // Regression: classify_transcript once computed the mtime EPOCH
        // (`now - elapsed`) instead of the age, so the STUCK gate
        // (`mtime_age < 240`) could never pass on any live file.
        let dir = std::env::temp_dir().join(format!("fleetd-presence-mtime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mut body = String::new();
        body.push_str(&tool_line(&ts_ago(300.0), "Bash"));
        body.push('\n');
        body.push_str(&ev_line(&ts_ago(290.0), "system"));
        body.push('\n');
        std::fs::write(&path, body).unwrap();
        // Backdate the mtime to 165 s ago: young file, stale events — the
        // exact shape that must classify STUCK (pending, uncancelled).
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let back = std::time::SystemTime::now() - std::time::Duration::from_secs(165);
        f.set_modified(back).unwrap();
        let (s, ev) = classify_transcript(&path, NOW).unwrap();
        assert_eq!(s, State::Stuck, "young mtime + fresh pending = stuck");
        assert_eq!(ev.pending_tool.as_deref(), Some("Bash"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spinner_lines_defeat_idle() {
        assert!(!pane_idle("esc to interrupt · 12s"));
        assert!(!pane_idle("(3s · ↓ 1.2k tokens)"));
        assert!(!pane_idle("⎿ (timeout 5s)"));
        assert!(pane_idle("╭──────────╮"));
        assert!(pane_idle("> give me a task"));
    }

    #[test]
    fn menu_lines_defeat_idle() {
        assert!(!pane_idle("Enter to select · Tab/Arrow keys to navigate"));
        assert!(!pane_idle("Esc to cancel · ❯ 1. option"));
        assert!(pane_idle("nothing to see here"));
    }

    #[test]
    fn veto_promotes_idle_to_active_on_spinner() {
        let ev = Evidence::default();
        let (s, e) = veto_idle(State::Idle, Some("esc to interrupt · 2s"), ev);
        assert_eq!(s, State::Active);
        assert_eq!(e.signal, "pane-spinner-overrides");
        let (s, _) = veto_idle(State::Idle, Some("quiet pane"), Evidence::default());
        assert_eq!(s, State::Idle);
        let (s, _) = veto_idle(State::Active, None, Evidence::default());
        assert_eq!(s, State::Active);
    }

    #[test]
    fn state_wire_names_are_lowercase() {
        assert_eq!(State::Active.as_str(), "active");
        assert_eq!(State::Draining.as_str(), "draining");
        assert_eq!(State::Idle.as_str(), "idle");
        assert_eq!(State::Stuck.as_str(), "stuck");
        assert_eq!(State::Gone.as_str(), "gone");
    }
}
