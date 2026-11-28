//! Presence reconciliation (FLEETD-PRESENCE-001): periodically classify every
//! tmux seat from its transcript tail + pane capture and upsert the verdict
//! into the ledger `presence` table. Classification itself is pure logic in
//! `fleetd_core::presence`; this module owns the machine IO (tmux, files)
//! and the write.
//!
//! Death-awareness layer (FLEETD-TMUXHOOK-001): serve startup (and each
//! watch round) ensures the global tmux `remain-on-exit` option plus the
//! `pane-died` / `session-closed` hooks that post the fast `pane_died`
//! event to /v1/events; reconcile then reads #{pane_dead} to classify the
//! seat as `dead` (corpse present, exit code queryable) rather than `gone`.
//!
//! Out of scope here (later tasks): stalled-dispatch backoff, the noauto
//! flag, attach/memory surfaces.

use anyhow::Result;
use fleetd_core::presence::{self, Evidence, State};
use store::ledger;

/// Local tmux session names (one per seat) — `tmux ls`, best effort.
pub fn tmux_seats() -> Vec<String> {
    let out = std::process::Command::new("tmux")
        .args(["ls", "-F", "#{session_name}"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Capture the last 12 pane lines for a seat (None when the pane is gone).
fn capture_pane(seat: &str) -> Option<String> {
    let out = std::process::Command::new("tmux")
        .args(["capture-pane", "-p", "-S", "-12", "-t", seat])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// tmux ground truth for one seat's pane: `Some(exit_code)` when the pane's
/// process is dead (remain-on-exit keeps the corpse so the exit code stays
/// readable), None when alive or the session is gone.
fn pane_death(seat: &str) -> Option<i32> {
    let out = std::process::Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            seat,
            "#{pane_dead}:#{pane_dead_status}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    presence::pane_death_from_format(&String::from_utf8_lossy(&out.stdout))
}

/// Full classification pipeline for one seat: pane death (tmux ground truth)
/// -> newest transcript -> tail classification -> pane veto for IDLE
/// candidates. The pane is only read when the transcript says IDLE (the veto
/// is never the primary signal) — except the death flag, which is checked
/// unconditionally: a dead pane's claude is gone even when its transcript
/// tail still looks fresh.
pub fn classify_seat(seat: &str, now: f64) -> (State, Evidence, Option<std::path::PathBuf>) {
    if let Some(code) = pane_death(seat) {
        return (
            State::Dead,
            Evidence {
                signal: format!("pane-dead-exit:{code}"),
                ..Default::default()
            },
            None,
        );
    }
    let Some(transcript) = presence::latest_transcript(seat) else {
        return (
            State::Gone,
            Evidence {
                signal: "no-transcript".into(),
                ..Default::default()
            },
            None,
        );
    };
    let (state, ev) = match presence::classify_transcript(&transcript, now) {
        Ok(r) => r,
        Err(e) => {
            return (
                State::Gone,
                Evidence {
                    signal: format!("transcript-unreadable:{e}"),
                    ..Default::default()
                },
                Some(transcript),
            )
        }
    };
    let (state, ev) = if state == State::Idle {
        let pane = capture_pane(seat);
        presence::veto_idle(state, pane.as_deref(), ev)
    } else {
        (state, ev)
    };
    (state, ev, Some(transcript))
}

/// One reconciliation round: every tmux seat gets classified and upserted.
/// Returns the number of rows written. Never fails the caller's loop on a
/// single bad seat — errors degrade to that seat's `gone`.
pub fn reconcile_round() -> Result<usize> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default();
    let c = ledger::open()?;
    let mut written = 0;
    for seat in tmux_seats() {
        let (state, ev, transcript) = classify_seat(&seat, now);
        let (open, blocked) = presence::todo_counts(&seat);
        ledger::presence_upsert(
            &c,
            &seat,
            state.as_str(),
            transcript.as_ref().and_then(|p| p.to_str()),
            ev.last_event_ts.map(|t| t as i64),
            ev.mtime_age.map(|a| (now - a) as i64),
            ev.pending_tool.is_some(),
            open,
            blocked,
            now as i64,
        )?;
        written += 1;
    }
    Ok(written)
}

/// The shell command a death hook runs (via tmux `run-shell`): POST the
/// fast `pane_died` event for the dying seat to the flagship. The tmux
/// server expands #{session_name} at fire time (the dead pane's session =
/// the seat name). Transport: the local flagship unix socket when it
/// exists, else the TCP loopback face (`serve --http`). Quoting: the JSON
/// payload is double-quoted inside the run-shell's single quotes — tmux
/// strips the outer quotes and hands the rest to /bin/sh; inner quotes
/// must therefore be double (#{...} expands regardless of shell quoting).
fn death_hook_command(port: u16) -> String {
    let sock = crate::events::flagship_socket_path().display().to_string();
    let tpl = r##"run-shell 'SOCK="{sock}"; if [ -S "$SOCK" ]; then curl -sf --max-time 3 --unix-socket "$SOCK" -X POST http://localhost/v1/events -d "{\"seat\":\"#{session_name}\",\"kind\":\"pane_died\",\"open\":0,\"blocked\":0,\"dirty\":0}"; else curl -sf --max-time 3 -X POST http://127.0.0.1:{port}/v1/events -d "{\"seat\":\"#{session_name}\",\"kind\":\"pane_died\",\"open\":0,\"blocked\":0,\"dirty\":0}"; fi'"##;
    tpl.replace("{sock}", &sock)
        .replace("{port}", &port.to_string())
}

/// Serve-startup ensure (FLEETD-TMUXHOOK-001), idempotent and re-entrant:
/// - global `remain-on-exit on` so a crashed seat pane keeps its corpse
///   (#{pane_dead}=1 + #{pane_dead_status} exit code stay readable);
/// - global `pane-died` and `session-closed` hooks posting the fast
///   `pane_died` event. set-hook/set-option overwrite by name, so serve
///   restarts and per-round re-ensures have no side effects. Fleetd
///   self-manages these — nothing is written to the user's tmux.conf.
/// Returns false (and does nothing) when no tmux server is running:
/// issuing set-option would otherwise spawn an empty tmux server.
pub fn ensure_tmux_death_awareness(port: u16) -> bool {
    let live = std::process::Command::new("tmux").arg("ls").output();
    let server_up = match live {
        Ok(o) => {
            o.status.success() || !String::from_utf8_lossy(&o.stderr).contains("no server running")
        }
        Err(_) => false,
    };
    if !server_up {
        return false;
    }
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-g", "remain-on-exit", "on"])
        .output();
    let hook = death_hook_command(port);
    for event in ["pane-died", "session-closed"] {
        let _ = std::process::Command::new("tmux")
            .args(["set-hook", "-g", event, &hook])
            .output();
    }
    true
}

/// Long-running presence watch: one round every 60 seconds (serve thread).
/// Each round re-ensures the death-awareness hooks first — a tmux server
/// restart drops hooks and options, and this heals them within one cycle.
pub fn watch_forever(port: u16) -> ! {
    loop {
        let _ = ensure_tmux_death_awareness(port);
        match reconcile_round() {
            Ok(n) if n > 0 => eprintln!("presence: reconciled {n} seat(s)"),
            Ok(_) => {}
            Err(e) => eprintln!("presence error: {e:#}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_seat_without_transcript_is_gone() {
        let seat = format!("fleetd-no-such-seat-{}", std::process::id());
        let (state, ev, transcript) = classify_seat(&seat, 1_800_000_000.0);
        assert_eq!(state, State::Gone);
        assert_eq!(ev.signal, "no-transcript");
        assert!(transcript.is_none());
    }

    #[test]
    fn todo_counts_missing_seat_is_zero() {
        let (open, blocked) = presence::todo_counts("fleetd-no-such-seat");
        assert_eq!((open, blocked), (0, 0));
    }

    #[test]
    fn pane_death_missing_session_is_none() {
        // No such tmux session: display-message fails -> not dead (the
        // transcript path decides, as before the death layer existed).
        let seat = format!("fleetd-no-such-seat-{}", std::process::id());
        assert_eq!(pane_death(&seat), None);
    }

    #[test]
    fn death_hook_carries_seat_placeholder_and_kind() {
        let cmd = death_hook_command(3790);
        // The seat comes from tmux expansion at fire time, never baked in.
        assert!(cmd.contains("#{session_name}"));
        // JSON quotes are escaped for the sh layer (\" inside the run-shell).
        assert!(cmd.contains(r#"\"kind\":\"pane_died\""#));
        // Both faces: unix socket first, TCP loopback fallback.
        assert!(cmd.contains("--unix-socket"));
        assert!(cmd.contains("127.0.0.1:3790/v1/events"));
        assert!(cmd.starts_with("run-shell '"));
        // Exactly the two outer single quotes — the payload must not break
        // the run-shell quoting.
        assert_eq!(cmd.matches('\'').count(), 2);
    }
}
