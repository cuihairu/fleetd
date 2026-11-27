//! dispatch_io: the IO half of dispatching (user-approved layering).
//! fleetd_core::dispatcher keeps the pure decision functions; this module owns the
//! side effects — agent delivery, ledger writes, cooldown queries.

use fleetd_core::agent::{self, agent_for};
use fleetd_core::dispatcher::{decide, execute, gates_pass, task_text, Decision};
use fleetd_core::verdict::Verdict;

/// Full dispatch cycle: gates → decision → agent delivery → ledger record.
/// This is the only function that can actually send a task to a seat.
pub fn run(
    seat: &str,
    host: &str,
    agent_spec: &str,
    verdict: Verdict,
    open: i64,
    blocked: i64,
    dirty: i64,
    queued_tag: Option<String>,
    auto: bool,
) -> Result<String, String> {
    let kind = agent_for(agent_spec);
    let agent = fleetd_core::agent::registry(kind);

    // 1) safety flags
    let global_flag = std::path::Path::new("/home/cui/.hermes/noauto-nudge/ALL").exists();
    let seat_flag =
        std::path::Path::new(&format!("/home/cui/.hermes/noauto-nudge/{seat}")).exists();

    // 2) live busy check (never stack on a working seat)
    let pane = fleetd_core::agent::tmux_run(host, &["capture-pane", "-p", "-t", seat])
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let agent_busy = matches!(agent.classify(&pane), fleetd_core::agent::Busy::Working);

    // 3) cooldown: refuse if this seat was dispatched < 90s ago
    let cooldown_ok = cooldown_ok(seat, 90);

    gates_pass(global_flag, seat_flag, agent_busy, cooldown_ok)?;

    let decision = decide(verdict, open, blocked, queued_tag, dirty, auto, true, agent_busy);
    if let Decision::Noop { reason } = &decision {
        return Ok(format!("noop: {reason}"));
    }

    let outcome = execute(&decision, seat, host, agent_spec)?;

    // record the new dispatch so the next event has a fresh baseline
    if let Ok(c) = crate::ledger::open() {
        let head = fleetd_core::agent::tmux_run(
            host,
            &["display-message", "-p", "-t", seat, "#{session_name}"],
        )
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
        let _ = head; // seat liveness only; baseline comes from git
        let tag = match &decision {
            Decision::DispatchQueued { tag } => tag.clone(),
            Decision::NudgeTodo { .. } => "auto-todo".to_string(),
            Decision::Resume { .. } => "auto-resume".to_string(),
            Decision::Noop { .. } => "auto-noop".to_string(),
        };
        let _ = crate::ledger::record_dispatch(&c, seat, &tag, None);
    }
    Ok(outcome)
}

/// Cooldown check: true when no dispatch for this seat in the last `secs`.
fn cooldown_ok(seat: &str, secs: i64) -> bool {
    let Ok(c) = crate::ledger::open() else { return false };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let last: Option<i64> = c
        .query_row(
            "SELECT MAX(fired_at) FROM dispatches WHERE seat=?1",
            rusqlite::params![seat],
            |r| r.get(0),
        )
        .unwrap_or(None);
    match last {
        Some(ts) => now - ts >= secs,
        None => true,
    }
}
