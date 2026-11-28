//! Presence reconciliation (FLEETD-PRESENCE-001): periodically classify every
//! tmux seat from its transcript tail + pane capture and upsert the verdict
//! into the ledger `presence` table. Classification itself is pure logic in
//! `fleetd_core::presence`; this module owns the machine IO (tmux, files)
//! and the write.
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

/// Full classification pipeline for one seat: newest transcript -> tail
/// classification -> pane veto for IDLE candidates. The pane is only read
/// when the transcript says IDLE (the veto is never the primary signal).
pub fn classify_seat(seat: &str, now: f64) -> (State, Evidence, Option<std::path::PathBuf>) {
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

/// Long-running presence watch: one round every 60 seconds (serve thread).
pub fn watch_forever() -> ! {
    loop {
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
}

