//! Dispatcher: turn a Stop-hook verdict into the next dispatch decision.
//!
//! Decision is a PURE function (unit-tested): given the verdict, the seat's
//! open-task count and whether a queued task exists, what should happen?
//! Execution is a thin layer that asks the agent plugin (claude/codex/...) to
//! deliver the task. The dispatcher itself knows nothing about agent UIs.

use crate::agent::{AgentKind, Delivery};
use crate::ledger::Verdict;

/// What the dispatcher decides to do after an agent stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Completed and a queued task is waiting → deliver that task.
    DispatchQueued { tag: String },
    /// Completed but no queued task → nudge to continue the open todo ledger.
    NudgeTodo { open: i64 },
    /// Stalled/partial → send a resume note carrying the pre-death progress.
    Resume { dirty: i64, reason: String },
    /// Nothing safe to do (untracked / no open work / kill-switch).
    Noop { reason: String },
}

/// Pure decision function — no I/O, fully testable.
pub fn decide(
    verdict: Verdict,
    open: i64,
    queued_tag: Option<String>,
    dirty: i64,
    auto_enabled: bool,
    cooldown_ok: bool,
    agent_busy: bool,
) -> Decision {
    if !auto_enabled {
        return Decision::Noop { reason: "auto-dispatch disabled (noauto flag)".into() };
    }
    if !cooldown_ok {
        return Decision::Noop { reason: "cooldown: last dispatch too recent".into() };
    }
    if agent_busy {
        return Decision::Noop { reason: "agent is busy; never stack a second task".into() };
    }
    match verdict {
        Verdict::Completed => match queued_tag {
            Some(tag) if !tag.is_empty() => Decision::DispatchQueued { tag },
            _ if open > 0 => Decision::NudgeTodo { open },
            _ => Decision::Noop { reason: "completed and no open todos".into() },
        },
        Verdict::Partial | Verdict::Stalled => Decision::Resume {
            dirty,
            reason: format!("{:?}: uncommitted work in tree", verdict),
        },
        Verdict::NoProgress => Decision::Resume {
            dirty,
            reason: "stopped with no progress; report blocker".into(),
        },
        Verdict::Untracked => Decision::Noop {
            reason: "no in-flight dispatch baseline; cannot judge".into(),
        },
    }
}

/// Render the task text for a decision (one-liner, no shell quoting).
pub fn task_text(d: &Decision, seat: &str) -> Option<String> {
    match d {
        Decision::DispatchQueued { tag } => Some(format!(
            "执行 ~/.hermes/cache/scratch/tasks/{tag}.md (fleetd auto-dispatch) — 读任务书后按验收执行，验证一个功能点即 commit+push，不攒批。"
        )),
        Decision::NudgeTodo { open } => Some(format!(
            "继续 {seat} 的 todo.md 顺序清任务（open={open}）：先查上一单是否已 commit+push，未完成的先收口，再做下一项。禁重做已完成部分。"
        )),
        Decision::Resume { dirty, reason } => Some(format!(
            "续命单：{reason}（工作区 {dirty} 处未提交改动）。先 git status + git diff 核死前进度，已完成的功能点 commit+push，半成品接着做完；禁从头重做，禁前台长 sleep。"
        )),
        Decision::Noop { .. } => None,
    }
}

/// Safety gates before any execution. Pure — unit-tested.
pub fn gates_pass(noauto_global: bool, noauto_seat: bool, agent_busy: bool, cooldown_ok: bool) -> Result<(), String> {
    if noauto_global {
        return Err("global kill-switch (noauto-nudge/ALL) is set".into());
    }
    if noauto_seat {
        return Err("per-seat noauto flag is set".into());
    }
    if agent_busy {
        return Err("agent already working; never stack".into());
    }
    if !cooldown_ok {
        return Err("cooldown window not elapsed".into());
    }
    Ok(())
}

/// Execute a decision through the agent plugin. Returns what happened.
/// This is the only side-effecting entry; all gates must pass first.
pub fn execute(
    decision: &Decision,
    seat: &str,
    host: &str,
    agent_spec: &str,
) -> Result<String, String> {
    let text = match task_text(decision, seat) {
        Some(t) => t,
        None => return Ok("noop".into()),
    };
    let kind = agent_for(agent_spec);
    let agent = crate::agent::registry(kind);
    match agent.dispatch(host, seat, &text) {
        Delivery::Delivered => Ok(format!("delivered via {}: {}", kind.as_str(), shrink(&text))),
        Delivery::NoSession => Err(format!("{} session missing on {host}", seat)),
        Delivery::NotDelivered => Err(format!("{} did not acknowledge on {host}", seat)),
    }
}

fn shrink(s: &str) -> String {
    let mut out: String = s.chars().take(60).collect();
    if s.chars().count() > 60 {
        out.push('…');
    }
    out
}

/// Full dispatch cycle: gates → decision → agent delivery → ledger record.
/// This is the only function that can actually send a task to a seat.
pub fn run(
    seat: &str,
    host: &str,
    agent_spec: &str,
    verdict: Verdict,
    open: i64,
    dirty: i64,
    queued_tag: Option<String>,
    auto: bool,
) -> Result<String, String> {
    let kind = agent_for(agent_spec);
    let agent = crate::agent::registry(kind);

    // 1) safety flags
    let global_flag = std::path::Path::new("/home/cui/.hermes/noauto-nudge/ALL").exists();
    let seat_flag =
        std::path::Path::new(&format!("/home/cui/.hermes/noauto-nudge/{seat}")).exists();

    // 2) live busy check (never stack on a working seat)
    let pane = crate::agent::tmux_run(host, &["capture-pane", "-p", "-t", seat])
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let agent_busy = matches!(agent.classify(&pane), crate::agent::Busy::Working);

    // 3) cooldown: refuse if this seat was dispatched < 90s ago
    let cooldown_ok = cooldown_ok(seat, 90);

    gates_pass(global_flag, seat_flag, agent_busy, cooldown_ok)?;

    let decision = decide(verdict, open, queued_tag, dirty, auto, true, agent_busy);
    if let Decision::Noop { reason } = &decision {
        return Ok(format!("noop: {reason}"));
    }

    let outcome = execute(&decision, seat, host, agent_spec)?;

    // record the new dispatch so the next event has a fresh baseline
    if let Ok(c) = crate::ledger::open() {
        let head = crate::agent::tmux_run(
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

/// Resolve the agent for a seat from the config/environment. Unknown = conservative.
pub fn agent_for(spec: &str) -> AgentKind {
    AgentKind::from_str(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_with_queue_dispatches() {
        let d = decide(Verdict::Completed, 10, Some("t-001".into()), 0, true, true, false);
        assert_eq!(d, Decision::DispatchQueued { tag: "t-001".into() });
    }

    #[test]
    fn completed_no_queue_nudges_todo() {
        let d = decide(Verdict::Completed, 7, None, 0, true, true, false);
        assert_eq!(d, Decision::NudgeTodo { open: 7 });
    }

    #[test]
    fn completed_nothing_open_is_noop() {
        let d = decide(Verdict::Completed, 0, None, 0, true, true, false);
        assert!(matches!(d, Decision::Noop { .. }));
    }

    #[test]
    fn stalled_sends_resume_not_original_task() {
        let d = decide(Verdict::Stalled, 9, Some("t-001".into()), 3, true, true, false);
        assert!(matches!(d, Decision::Resume { dirty: 3, .. }));
    }

    #[test]
    fn partial_sends_resume() {
        let d = decide(Verdict::Partial, 9, None, 2, true, true, false);
        assert!(matches!(d, Decision::Resume { dirty: 2, .. }));
    }

    #[test]
    fn untracked_is_noop() {
        let d = decide(Verdict::Untracked, 9, None, 0, true, true, false);
        assert!(matches!(d, Decision::Noop { .. }));
    }

    #[test]
    fn disabled_never_dispatches() {
        let d = decide(Verdict::Completed, 9, Some("t".into()), 0, false, true, false);
        assert!(matches!(d, Decision::Noop { .. }));
    }

    #[test]
    fn cooldown_blocks() {
        let d = decide(Verdict::Completed, 9, Some("t".into()), 0, true, false, false);
        assert!(matches!(d, Decision::Noop { .. }));
    }

    #[test]
    fn busy_never_stacks() {
        let d = decide(Verdict::Completed, 9, Some("t".into()), 0, true, true, true);
        assert!(matches!(d, Decision::Noop { .. }));
    }

    #[test]
    fn resume_text_has_progress_hint() {
        let d = Decision::Resume { dirty: 3, reason: "stalled".into() };
        let t = task_text(&d, "kode").unwrap();
        assert!(t.contains("git status"));
        assert!(t.contains("3"));
        assert!(t.contains("禁从头重做"));
    }

    #[test]
    fn queued_text_has_verify_and_push() {
        let d = Decision::DispatchQueued { tag: "t-9".into() };
        let t = task_text(&d, "kode").unwrap();
        assert!(t.contains("t-9.md"));
        assert!(t.contains("commit+push"));
    }

    #[test]
    fn noop_has_no_text() {
        let d = Decision::Noop { reason: "x".into() };
        assert!(task_text(&d, "kode").is_none());
    }

    #[test]
    fn agent_alias_parsing() {
        assert_eq!(agent_for("claude"), AgentKind::Claude);
        assert_eq!(agent_for("codex"), AgentKind::Codex);
        assert_eq!(agent_for("whatever"), AgentKind::Unknown);
    }

    #[test]
    fn gates_block_on_global_killswitch() {
        assert!(gates_pass(true, false, false, true).is_err());
        assert!(gates_pass(false, true, false, true).is_err());
        assert!(gates_pass(false, false, true, true).is_err());
        assert!(gates_pass(false, false, false, false).is_err());
        assert!(gates_pass(false, false, false, true).is_ok());
    }

    #[test]
    fn noop_executes_nothing() {
        let d = Decision::Noop { reason: "x".into() };
        assert_eq!(execute(&d, "kode", "local", "claude").unwrap(), "noop");
    }
}
