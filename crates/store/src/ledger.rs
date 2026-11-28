//! Ledger: shared state for the control plane (DESIGN.md §3.7).
//!
//! Two tables:
//! - `tasks`: dispatch records (existing P0 face, moved here from main.rs)
//! - `events`: agent lifecycle events reported by Stop hooks
//!
//! Every event is classified against the seat's in-flight dispatch baseline:
//! completed (HEAD advanced, tree clean) / partial (advanced, dirty) /
//! stalled (HEAD unchanged, dirty) / interrupted (stop without progress).

use anyhow::{Context, Result};
use fleetd_core::verdict::{classify, SeatEvent, Verdict};
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;

/// Home directory cross-platform: HOME first (unix/MSYS), USERPROFILE on
/// Windows. Scheduled-task environments often lack HOME — falling back to "."
/// resolved the db against System32 and broke every store face (2026-11-27).
pub fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into())
}

pub fn db_path() -> PathBuf {
    std::env::var("FLEETD_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = home_dir();
            PathBuf::from(home).join(".fleetd/ledger.db")
        })
}

pub fn open() -> Result<Connection> {
    let path = db_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create db dir {}", dir.display()))?;
    }
    let c = Connection::open(&path).with_context(|| format!("open ledger {}", path.display()))?;
    c.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            host TEXT NOT NULL,
            repo TEXT NOT NULL,
            task_file TEXT NOT NULL,
            state TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            finished_at INTEGER
         );
         CREATE TABLE IF NOT EXISTS seats (
            seat TEXT PRIMARY KEY,
            cwd TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'idle',
            current_tag TEXT,
            baseline_head TEXT,
            updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS jobs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            schedule TEXT NOT NULL,
            command TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            last_run INTEGER,
            created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS job_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id INTEGER NOT NULL,
            started INTEGER NOT NULL,
            finished INTEGER NOT NULL,
            exit_code INTEGER NOT NULL,
            output_tail TEXT NOT NULL DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS idx_job_runs_job ON job_runs(job_id, started DESC);
         CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER NOT NULL,
            seat TEXT NOT NULL,
            head TEXT,
            open_count INTEGER,
            dirty_count INTEGER,
            kind TEXT NOT NULL DEFAULT 'stop',
            verdict TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_events_seat ON events(seat, ts DESC);
         CREATE TABLE IF NOT EXISTS dispatches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            seat TEXT NOT NULL,
            tag TEXT NOT NULL,
            fired_at INTEGER NOT NULL,
            baseline_head TEXT,
            finished_at INTEGER,
            outcome TEXT,
            head_at_finish TEXT,
            open_at_finish INTEGER,
            dirty_at_finish INTEGER,
            note TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_dispatch_seat ON dispatches(seat, fired_at DESC);
         CREATE TABLE IF NOT EXISTS presence (
            seat TEXT PRIMARY KEY,
            state TEXT NOT NULL,
            transcript_path TEXT,
            last_event_ts INTEGER,
            last_output_ts INTEGER,
            pending_tool_use INTEGER,
            open_todos INTEGER,
            blocked_todos INTEGER,
            updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS memory (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            key TEXT NOT NULL,
            seat TEXT,
            tag TEXT,
            content TEXT NOT NULL,
            source TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            UNIQUE(key)
         );
         CREATE INDEX IF NOT EXISTS idx_memory_seat ON memory(seat);
         CREATE INDEX IF NOT EXISTS idx_memory_tag ON memory(tag);",
    )?;
    // Seat-control gate migration (2026-11-26 falcon incident): disabled
    // seats must be skipped by every auto-start path. ALTER with tolerance
    // for already-migrated databases.
    // executor (FLEETD-EXECUTOR-001): seat environment dimension — every
    // pre-existing seat is tmux, hence the NOT NULL DEFAULT 'tmux'.
    for col in [
        "stopped_by TEXT",
        "stopped_at INTEGER",
        "executor TEXT NOT NULL DEFAULT 'tmux'",
    ] {
        let sql = format!("ALTER TABLE seats ADD COLUMN {col};");
        let _ = c.execute_batch(&sql); // duplicate-column errors are fine
    }
    Ok(c)
}

fn default_kind() -> String {
    "stop".into()
}

/// Verdict of an event against the seat's in-flight dispatch baseline.
/// Pure classification moved to fleetd_core::verdict (2026-11-27 crate split).

/// Seat-control gate: a disabled seat must be skipped by every auto-start
/// path (idle-watch, fire, agent heartbeat, patrol resume). Set by
/// `seat stop`/`seat disable`, cleared only by `seat enable`.
pub fn is_disabled(c: &Connection, seat: &str) -> Result<bool> {
    let mut st = c.prepare("SELECT status FROM seats WHERE seat = ?1")?;
    let status: Option<String> = st.query_row([seat], |r| r.get(0)).optional()?;
    Ok(status.as_deref() == Some("disabled"))
}

/// Write the gate without touching the session/process (soft stop).
pub fn set_gate(c: &Connection, seat: &str, disabled: bool, by: &str) -> Result<()> {
    let status = if disabled { "disabled" } else { "idle" };
    let now = chrono::Utc::now().timestamp();
    c.execute(
        "UPDATE seats SET status = ?2, stopped_by = ?3, stopped_at = ?4, updated_at = ?4
         WHERE seat = ?1",
        rusqlite::params![seat, status, by, now],
    )?;
    // ensure row exists even if the seat was never seen before
    c.execute(
        "INSERT OR IGNORE INTO seats (seat, cwd, status, stopped_by, stopped_at, updated_at)
         VALUES (?1, '', ?2, ?3, ?4, ?4)",
        rusqlite::params![seat, status, by, now],
    )?;
    Ok(())
}

/// Register a seat's executor dimension (FLEETD-EXECUTOR-001): which
/// environment actually runs the agent process (tmux / container / native /
/// vm). Unknown names are rejected here so the wire enum stays closed; the
/// row is created when the seat was never seen (registration may precede the
/// first tmux session).
pub fn set_executor(c: &Connection, seat: &str, executor: &str) -> Result<()> {
    let kind = fleetd_core::executor::Executor::from_str(executor)
        .with_context(|| format!("unknown executor: {executor} (tmux/container/native/vm)"))?;
    let now = chrono::Utc::now().timestamp();
    c.execute(
        "UPDATE seats SET executor = ?2, updated_at = ?3 WHERE seat = ?1",
        rusqlite::params![seat, kind.as_str(), now],
    )?;
    c.execute(
        "INSERT OR IGNORE INTO seats (seat, cwd, status, executor, updated_at)
         VALUES (?1, '', 'idle', ?2, ?3)",
        rusqlite::params![seat, kind.as_str(), now],
    )?;
    Ok(())
}

/// A seat's executor; tmux when the seat has no row (workspace-scan seats
/// default to tmux, per the ticket). Never errors on a missing seat.
pub fn executor_of(c: &Connection, seat: &str) -> Result<String> {
    let mut st = c.prepare("SELECT executor FROM seats WHERE seat = ?1")?;
    let ex: Option<String> = st.query_row([seat], |r| r.get(0)).optional()?;
    Ok(ex.unwrap_or_else(|| {
        fleetd_core::executor::Executor::default()
            .as_str()
            .to_string()
    }))
}

/// seat → executor map for fleet-wide faces (/v1/fleet, `fleetd fleet`).
/// Rows absent from seats read as tmux at the call site.
pub fn executor_map(c: &Connection) -> Result<std::collections::HashMap<String, String>> {
    let mut st = c.prepare("SELECT seat, executor FROM seats")?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .and_then(|m| m.collect::<std::result::Result<Vec<_>, _>>())?;
    Ok(rows.into_iter().collect())
}

/// In-flight dispatch for a seat (tag + baseline), if any.
pub fn inflight(c: &Connection, seat: &str) -> Result<Option<(i64, String, Option<String>)>> {
    let mut st = c.prepare(
        "SELECT id, tag, baseline_head FROM dispatches
         WHERE seat=?1 AND finished_at IS NULL
         ORDER BY fired_at DESC LIMIT 1",
    )?;
    let row = st
        .query_row(rusqlite::params![seat], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .optional()?;
    Ok(row)
}

/// Kinds that carry a terminal state worth classifying a dispatch against.
/// notification/compact are observational — they must NOT close a dispatch.
/// pane_died (FLEETD-TMUXHOOK-001, the tmux death hook) is observational
/// too: it records the fact that a seat's pane exited, but whether that
/// seat's work actually completed stays with the presence reconcile layer,
/// which reads tmux ground truth instead of trusting the event.
fn is_terminal_kind(kind: &str) -> bool {
    matches!(
        kind,
        "stop" | "session_end" | "subagent_stop" | "teammate_idle"
    )
}

/// Ingest the event, classify against the in-flight dispatch, close it out.
/// Returns the verdict (also stored on the event row). Observational kinds
/// (notification/compact) are recorded but leave the dispatch open.
pub fn ingest(c: &Connection, ev: &SeatEvent) -> Result<Verdict> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let terminal = is_terminal_kind(&ev.kind);
    // Only classify when the event is terminal AND we have a baseline.
    let baseline: Option<String> = if terminal {
        inflight(c, &ev.seat)?.and_then(|(_, _, b)| b)
    } else {
        None
    };
    let verdict = if terminal {
        classify(baseline.as_deref(), ev.head.as_deref(), ev.dirty)
    } else {
        Verdict::Untracked // recorded with kind, never closes a dispatch
    };
    c.execute(
        "INSERT INTO events(ts,seat,head,open_count,dirty_count,kind,verdict)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![
            ts,
            ev.seat,
            ev.head,
            ev.open,
            ev.dirty,
            ev.kind,
            verdict.as_str()
        ],
    )?;
    // Only terminal events close the in-flight dispatch. Observational kinds
    // (notification/session_start/compact) are recorded but leave it open.
    if terminal && inflight(c, &ev.seat)?.is_some() {
        c.execute(
            "UPDATE dispatches SET finished_at=?1, outcome=?2, head_at_finish=?3,
             open_at_finish=?4, dirty_at_finish=?5 WHERE seat=?6 AND finished_at IS NULL",
            rusqlite::params![ts, verdict.as_str(), ev.head, ev.open, ev.dirty, ev.seat],
        )?;
    }
    Ok(verdict)
}

/// Register a dispatch so later events can be classified.
pub fn record_dispatch(
    c: &Connection,
    seat: &str,
    tag: &str,
    baseline: Option<&str>,
) -> Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    c.execute(
        "INSERT INTO dispatches(seat,tag,fired_at,baseline_head) VALUES(?1,?2,?3,?4)",
        rusqlite::params![seat, tag, ts, baseline],
    )?;
    Ok(())
}

/// Per-seat aggregate counts for `fleetd status` over the last `days`.
pub struct SeatProgress {
    pub seat: String,
    pub fired: i64,
    pub completed: i64,
    pub inflight: i64,
    pub stalled: i64,
    pub partial: i64,
}

pub fn progress(c: &Connection, days: u64) -> Result<Vec<SeatProgress>> {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(days * 86400);
    let mut st = c.prepare(
        "SELECT seat,
            COUNT(*) AS fired,
            SUM(outcome='completed') AS completed,
            SUM(outcome IS NULL) AS inflight,
            SUM(outcome='stalled') AS stalled,
            SUM(outcome='partial') AS partial
         FROM dispatches WHERE fired_at > ?1
         GROUP BY seat ORDER BY seat",
    )?;
    let rows = st
        .query_map(rusqlite::params![since as i64], |r| {
            Ok(SeatProgress {
                seat: r.get(0)?,
                fired: r.get(1)?,
                completed: r.get(2)?,
                inflight: r.get(3)?,
                stalled: r.get(4)?,
                partial: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advanced_clean_is_completed() {
        assert_eq!(
            classify(Some("aaa"), Some("bbb"), Some(0)),
            Verdict::Completed
        );
    }
    #[test]
    fn advanced_dirty_is_partial() {
        assert_eq!(
            classify(Some("aaa"), Some("bbb"), Some(3)),
            Verdict::Partial
        );
    }
    #[test]
    fn unchanged_dirty_is_stalled() {
        assert_eq!(
            classify(Some("aaa"), Some("aaa"), Some(2)),
            Verdict::Stalled
        );
    }
    #[test]
    fn unchanged_clean_is_no_progress() {
        assert_eq!(
            classify(Some("aaa"), Some("aaa"), Some(0)),
            Verdict::NoProgress
        );
    }
    #[test]
    fn no_baseline_is_untracked() {
        assert_eq!(classify(None, Some("bbb"), Some(0)), Verdict::Untracked);
        assert_eq!(classify(Some("aaa"), None, Some(0)), Verdict::Untracked);
    }
    #[test]
    fn case_sensitive_head_compare() {
        // Git short heads are lowercase hex; "AAA" must not equal "aaa".
        assert_eq!(
            classify(Some("aaa"), Some("AAA"), Some(0)),
            Verdict::Completed
        );
    }

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE dispatches (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                seat TEXT NOT NULL, tag TEXT NOT NULL, fired_at INTEGER NOT NULL,
                baseline_head TEXT, finished_at INTEGER, outcome TEXT,
                head_at_finish TEXT, open_at_finish INTEGER, dirty_at_finish INTEGER, note TEXT);
             CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL, seat TEXT NOT NULL, head TEXT,
                open_count INTEGER, dirty_count INTEGER, kind TEXT, verdict TEXT);
             CREATE TABLE presence (
                seat TEXT PRIMARY KEY, state TEXT NOT NULL, transcript_path TEXT,
                last_event_ts INTEGER, last_output_ts INTEGER, pending_tool_use INTEGER,
                open_todos INTEGER, blocked_todos INTEGER, updated_at INTEGER NOT NULL);
             CREATE TABLE memory (
                id INTEGER PRIMARY KEY AUTOINCREMENT, key TEXT NOT NULL,
                seat TEXT, tag TEXT, content TEXT NOT NULL, source TEXT,
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                UNIQUE(key));
             CREATE TABLE seats (
                seat TEXT PRIMARY KEY, cwd TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'idle', current_tag TEXT,
                baseline_head TEXT, updated_at INTEGER NOT NULL,
                stopped_by TEXT, stopped_at INTEGER,
                executor TEXT NOT NULL DEFAULT 'tmux');",
        )
        .unwrap();
        c
    }

    #[test]
    fn ingest_completes_inflight() {
        let c = mem();
        record_dispatch(&c, "kode", "kode-x-001", Some("aaa")).unwrap();
        let v = ingest(
            &c,
            &SeatEvent {
                seat: "kode".into(),
                head: Some("bbb".into()),
                open: Some(9),
                blocked: Some(0),
                dirty: Some(0),
                kind: "stop".into(),
            },
        )
        .unwrap();
        assert_eq!(v, Verdict::Completed);
        let outcome: String = c
            .query_row(
                "SELECT outcome FROM dispatches WHERE seat='kode'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(outcome, "completed");
    }

    #[test]
    fn ingest_without_dispatch_is_untracked_but_recorded() {
        let c = mem();
        let v = ingest(
            &c,
            &SeatEvent {
                seat: "falcon".into(),
                head: Some("bbb".into()),
                open: Some(1),
                blocked: Some(0),
                dirty: Some(0),
                kind: "stop".into(),
            },
        )
        .unwrap();
        assert_eq!(v, Verdict::Untracked);
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM events WHERE seat='falcon'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn observational_event_does_not_close_dispatch() {
        let c = mem();
        record_dispatch(&c, "kode", "t1", Some("aaa")).unwrap();
        // notification kind must be recorded but leave dispatch in-flight
        let v = ingest(
            &c,
            &SeatEvent {
                seat: "kode".into(),
                head: Some("bbb".into()),
                open: Some(1),
                blocked: Some(0),
                dirty: Some(0),
                kind: "observational".into(),
            },
        )
        .unwrap();
        assert_eq!(v, Verdict::Untracked);
        let (finished, outcome): (Option<i64>, Option<String>) = c
            .query_row(
                "SELECT finished_at, outcome FROM dispatches WHERE seat='kode'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            finished.is_none(),
            "observational event must NOT close dispatch"
        );
        assert!(outcome.is_none());
        // terminal stop now closes it correctly
        let v2 = ingest(
            &c,
            &SeatEvent {
                seat: "kode".into(),
                head: Some("bbb".into()),
                open: Some(1),
                blocked: Some(0),
                dirty: Some(0),
                kind: "stop".into(),
            },
        )
        .unwrap();
        assert_eq!(v2, Verdict::Completed);
        let outcome2: String = c
            .query_row(
                "SELECT outcome FROM dispatches WHERE seat='kode'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(outcome2, "completed");
    }

    #[test]
    fn pane_died_event_is_observational() {
        // The tmux death hook's kind records the death without closing the
        // in-flight dispatch: death vs completion stays with the reconcile
        // layer (FLEETD-TMUXHOOK-001).
        let c = mem();
        record_dispatch(&c, "kode", "t1", Some("aaa")).unwrap();
        let v = ingest(
            &c,
            &SeatEvent {
                seat: "kode".into(),
                head: Some("bbb".into()),
                open: Some(0),
                blocked: Some(0),
                dirty: Some(0),
                kind: "pane_died".into(),
            },
        )
        .unwrap();
        assert_eq!(v, Verdict::Untracked);
        let (finished, outcome): (Option<i64>, Option<String>) = c
            .query_row(
                "SELECT finished_at, outcome FROM dispatches WHERE seat='kode'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(finished.is_none(), "pane_died must NOT close the dispatch");
        assert!(outcome.is_none());
        let kind: String = c
            .query_row("SELECT kind FROM events WHERE seat='kode'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kind, "pane_died");
    }

    #[test]
    fn progress_counts_by_outcome() {
        let c = mem();
        record_dispatch(&c, "kode", "t1", Some("aaa")).unwrap();
        ingest(
            &c,
            &SeatEvent {
                seat: "kode".into(),
                head: Some("bbb".into()),
                open: Some(0),
                blocked: Some(0),
                dirty: Some(0),
                kind: "stop".into(),
            },
        )
        .unwrap();
        record_dispatch(&c, "kode", "t2", Some("bbb")).unwrap();
        // t2: baseline bbb, head bbb (unchanged), dirty>0 → stalled
        ingest(
            &c,
            &SeatEvent {
                seat: "kode".into(),
                head: Some("bbb".into()),
                open: Some(4),
                blocked: Some(0),
                dirty: Some(2),
                kind: "stop".into(),
            },
        )
        .unwrap();
        let rows = progress(&c, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].fired, 2);
        assert_eq!(rows[0].completed, 1);
        assert_eq!(rows[0].stalled, 1);
    }

    #[test]
    fn presence_upsert_and_filter_roundtrip() {
        let c = mem();
        presence_upsert(
            &c,
            "kode",
            "active",
            Some("/t/kode.jsonl"),
            Some(111),
            Some(112),
            false,
            3,
            1,
            200,
        )
        .unwrap();
        presence_upsert(&c, "courier", "idle", None, None, None, true, 0, 0, 201).unwrap();
        // Upsert overwrites in place (no duplicate rows).
        presence_upsert(
            &c,
            "kode",
            "stuck",
            Some("/t/kode.jsonl"),
            Some(150),
            Some(150),
            true,
            3,
            1,
            300,
        )
        .unwrap();
        let all = presence_rows(&c, "").unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seat, "courier"); // seat order
        let kode = all.iter().find(|r| r.seat == "kode").unwrap();
        assert_eq!(kode.state, "stuck");
        assert_eq!(kode.open, 3);
        assert_eq!(kode.blocked, 1);
        assert_eq!(kode.last_event_ts, Some(150));
        // State filter narrows to matching rows only.
        let idle = presence_rows(&c, "idle").unwrap();
        assert_eq!(idle.len(), 1);
        assert_eq!(idle[0].seat, "courier");
        assert_eq!(idle[0].last_commit, None);
        assert_eq!(presence_rows(&c, "gone").unwrap().len(), 0);
    }

    fn put_blocker(c: &Connection, seat: &str, key: &str, content: &str) {
        memory_put(c, key, Some(seat), Some("blocker"), content, Some("fleetd")).unwrap();
    }

    #[test]
    fn memory_put_get_roundtrip() {
        let c = mem();
        put_blocker(&c, "kode", "seat:kode:blocked", "winpkg sign-tool missing");
        let row = memory_get(&c, "seat:kode:blocked").unwrap().unwrap();
        assert_eq!(row.seat.as_deref(), Some("kode"));
        assert_eq!(row.tag.as_deref(), Some("blocker"));
        assert_eq!(row.content, "winpkg sign-tool missing");
        assert_eq!(row.source.as_deref(), Some("fleetd"));
        assert_eq!(row.created_at, row.updated_at, "first write stamps both");
        assert!(memory_get(&c, "no-such-key").unwrap().is_none());
    }

    #[test]
    fn memory_put_upsert_overwrites_and_keeps_created_at() {
        let c = mem();
        put_blocker(&c, "kode", "k1", "old content");
        let first = memory_get(&c, "k1").unwrap().unwrap();
        // Sleep-free determinism: rewrite through the API, then compare.
        memory_put(
            &c,
            "k1",
            None,
            Some("decision"),
            "new content",
            Some("hermes"),
        )
        .unwrap();
        let second = memory_get(&c, "k1").unwrap().unwrap();
        assert_eq!(second.content, "new content");
        assert_eq!(second.seat, None, "upsert replaces seat too");
        assert_eq!(second.tag.as_deref(), Some("decision"));
        assert_eq!(second.source.as_deref(), Some("hermes"));
        assert_eq!(
            second.created_at, first.created_at,
            "created_at stays first-write"
        );
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM memory WHERE key='k1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1, "UNIQUE(key): upsert must not duplicate rows");
    }

    #[test]
    fn memory_list_filters_by_seat_and_tag() {
        let c = mem();
        put_blocker(&c, "apollo", "seat:apollo:blocked", "apollo stuck");
        put_blocker(&c, "atlas", "seat:atlas:blocked", "atlas stuck");
        memory_put(
            &c,
            "global:freeze",
            None,
            Some("decision"),
            "freeze week",
            None,
        )
        .unwrap();
        let apollo = memory_list(&c, Some("apollo"), None, 50).unwrap();
        assert_eq!(apollo.len(), 1);
        assert_eq!(apollo[0].key, "seat:apollo:blocked");
        let blockers = memory_list(&c, None, Some("blocker"), 50).unwrap();
        assert_eq!(blockers.len(), 2);
        let decisions = memory_list(&c, None, Some("decision"), 50).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].key, "global:freeze");
        // Seat filter excludes the global NULL-seat note.
        assert!(memory_list(&c, Some("apollo"), Some("decision"), 50)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn memory_list_orders_by_updated_desc_and_limits() {
        let c = mem();
        put_blocker(&c, "a", "k-a", "a");
        put_blocker(&c, "b", "k-b", "b");
        put_blocker(&c, "c", "k-c", "c");
        // Stamp distinct updated_at values: same-second writes would make
        // the DESC order ambiguous.
        for (key, ts) in [("k-a", 100), ("k-c", 90), ("k-b", 80)] {
            c.execute(
                "UPDATE memory SET updated_at=?1 WHERE key=?2",
                rusqlite::params![ts, key],
            )
            .unwrap();
        }
        let rows = memory_list(&c, None, None, 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "k-a", "most recently updated first");
        assert_eq!(rows[1].key, "k-c");
    }

    #[test]
    fn memory_delete_reports_whether_row_existed() {
        let c = mem();
        put_blocker(&c, "kode", "k1", "x");
        assert!(memory_delete(&c, "k1").unwrap());
        assert!(!memory_delete(&c, "k1").unwrap(), "second delete = no row");
        assert!(memory_get(&c, "k1").unwrap().is_none());
    }

    #[test]
    fn memory_seat_counts_aggregate_per_seat() {
        let c = mem();
        put_blocker(&c, "apollo", "k1", "x");
        memory_put(&c, "k2", Some("apollo"), Some("decision"), "y", None).unwrap();
        put_blocker(&c, "atlas", "k3", "z");
        let counts = memory_seat_counts(&c).unwrap();
        assert_eq!(
            counts,
            vec![("apollo".to_string(), 2), ("atlas".to_string(), 1)]
        );
    }

    #[test]
    fn executor_defaults_to_tmux_for_unregistered_seats() {
        let c = mem();
        // No seats rows at all: every seat reads as tmux (workspace-scan default).
        assert_eq!(executor_of(&c, "apollo").unwrap(), "tmux");
        assert_eq!(executor_of(&c, "ghost").unwrap(), "tmux");
        // And the map is empty — absent rows stay tmux at the call site.
        assert!(executor_map(&c).unwrap().is_empty());
    }

    #[test]
    fn set_executor_roundtrip_creates_and_updates_rows() {
        let c = mem();
        set_executor(&c, "boxcar", "container").unwrap();
        assert_eq!(executor_of(&c, "boxcar").unwrap(), "container");
        // Re-registering with a different environment updates in place.
        set_executor(&c, "boxcar", "vm").unwrap();
        assert_eq!(executor_of(&c, "boxcar").unwrap(), "vm");
        // The map carries exactly the registered row.
        let map = executor_map(&c).unwrap();
        assert_eq!(map.get("boxcar").map(String::as_str), Some("vm"));
    }

    #[test]
    fn set_executor_rejects_unknown_environment_names() {
        let c = mem();
        assert!(set_executor(&c, "boxcar", "docker").is_err());
        assert!(set_executor(&c, "boxcar", "").is_err());
        // The rejection must not have written a row.
        assert_eq!(executor_of(&c, "boxcar").unwrap(), "tmux");
    }
}

/// One presence row (FLEETD-PRESENCE-001): current liveness of a seat as
/// reconciled from its transcript tail and pane capture.
#[derive(Debug, Clone)]
pub struct PresenceRow {
    pub seat: String,
    pub state: String,
    pub open: i64,
    pub blocked: i64,
    pub last_event_ts: Option<i64>,
    pub last_commit: Option<i64>,
}

/// Upsert one reconciled presence row. All signals are passed by the caller
/// (the scheduler owns tmux/transcript IO); this function only persists.
#[allow(clippy::too_many_arguments)]
pub fn presence_upsert(
    c: &Connection,
    seat: &str,
    state: &str,
    transcript_path: Option<&str>,
    last_event_ts: Option<i64>,
    last_output_ts: Option<i64>,
    pending_tool_use: bool,
    open_todos: i64,
    blocked_todos: i64,
    updated_at: i64,
) -> Result<()> {
    c.execute(
        "INSERT INTO presence (seat, state, transcript_path, last_event_ts,
             last_output_ts, pending_tool_use, open_todos, blocked_todos, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(seat) DO UPDATE SET
             state=excluded.state, transcript_path=excluded.transcript_path,
             last_event_ts=excluded.last_event_ts, last_output_ts=excluded.last_output_ts,
             pending_tool_use=excluded.pending_tool_use, open_todos=excluded.open_todos,
             blocked_todos=excluded.blocked_todos, updated_at=excluded.updated_at",
        rusqlite::params![
            seat,
            state,
            transcript_path,
            last_event_ts,
            last_output_ts,
            i64::from(pending_tool_use),
            open_todos,
            blocked_todos,
            updated_at
        ],
    )?;
    Ok(())
}

/// Read presence rows in seat order. `state` filters when non-empty.
/// `last_commit` comes from the most recent event for that seat (the ledger
/// already records lifecycle events there), not from transcript IO.
pub fn presence_rows(c: &Connection, state_filter: &str) -> Result<Vec<PresenceRow>> {
    let mut st = c.prepare(
        "SELECT p.seat, p.state, p.open_todos, p.blocked_todos, p.last_event_ts,
                (SELECT MAX(e.ts) FROM events e WHERE e.seat = p.seat) AS last_commit
         FROM presence p
         WHERE (?1 = '' OR p.state = ?1)
         ORDER BY p.seat",
    )?;
    let rows = st.query_map([state_filter], |r| {
        Ok(PresenceRow {
            seat: r.get(0)?,
            state: r.get(1)?,
            open: r.get(2)?,
            blocked: r.get(3)?,
            last_event_ts: r.get(4)?,
            last_commit: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// One fleet-memory note (FLEETD-MEM-001, docs/agent-support.md §4.2): a
/// structured note any writer (hermes / fleetd / a seat) attaches under a
/// convention key, e.g. `seat:kode:blocker:KODE-WINPKG-001`. This is the
/// storage + convention face only; retrieval mechanisms come later.
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub key: String,
    /// Owning seat (None = fleet-global note).
    pub seat: Option<String>,
    /// Free-form tag (blocker / decision / credgap / ...).
    pub tag: Option<String>,
    pub content: String,
    /// Writer identity (hermes / fleetd / seat:<name>).
    pub source: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Upsert one memory note. UNIQUE(key) conflict updates seat/tag/content/
/// source/updated_at; created_at keeps the first-write time.
#[allow(clippy::too_many_arguments)]
pub fn memory_put(
    c: &Connection,
    key: &str,
    seat: Option<&str>,
    tag: Option<&str>,
    content: &str,
    source: Option<&str>,
) -> Result<()> {
    let now = now_secs();
    c.execute(
        "INSERT INTO memory(key,seat,tag,content,source,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?6)
         ON CONFLICT(key) DO UPDATE SET
             seat=excluded.seat, tag=excluded.tag, content=excluded.content,
             source=excluded.source, updated_at=excluded.updated_at",
        rusqlite::params![key, seat, tag, content, source, now],
    )?;
    Ok(())
}

const MEMORY_COLS: &str = "key,seat,tag,content,source,created_at,updated_at";

fn memory_row(r: &rusqlite::Row) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        key: r.get(0)?,
        seat: r.get(1)?,
        tag: r.get(2)?,
        content: r.get(3)?,
        source: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

/// Fetch one note by exact key.
pub fn memory_get(c: &Connection, key: &str) -> Result<Option<MemoryRow>> {
    let mut st = c.prepare(&format!("SELECT {MEMORY_COLS} FROM memory WHERE key = ?1"))?;
    let row = st
        .query_row(rusqlite::params![key], memory_row)
        .optional()?;
    Ok(row)
}

/// List notes newest-update first. Seat and tag are independent optional
/// filters (seat filter matches only notes owned by that seat — global
/// notes with NULL seat are excluded from seat-scoped lists).
pub fn memory_list(
    c: &Connection,
    seat: Option<&str>,
    tag: Option<&str>,
    limit: i64,
) -> Result<Vec<MemoryRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {MEMORY_COLS} FROM memory
         WHERE (?1 IS NULL OR seat = ?1) AND (?2 IS NULL OR tag = ?2)
         ORDER BY updated_at DESC LIMIT ?3"
    ))?;
    let rows = st
        .query_map(rusqlite::params![seat, tag, limit], memory_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Delete one note by key. Returns true when a row was removed.
pub fn memory_delete(c: &Connection, key: &str) -> Result<bool> {
    let n = c.execute("DELETE FROM memory WHERE key = ?1", [key])?;
    Ok(n > 0)
}

/// Per-seat note counts (for the /v1/fleet face: each seat row carries its
/// memory count so dashboards read it without a second round-trip).
pub fn memory_seat_counts(c: &Connection) -> Result<Vec<(String, i64)>> {
    let mut st = c.prepare(
        "SELECT seat, COUNT(*) FROM memory
         WHERE seat IS NOT NULL GROUP BY seat ORDER BY seat",
    )?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Dispatches fired but not yet finished — the "is fleetd working" signal
/// that drives the tray rotation speed (2026-11-27 用户令:跑任务时 icon 缓转).
pub fn in_flight_count() -> i64 {
    let c = match open() {
        Ok(c) => c,
        Err(_) => return 0,
    };
    c.query_row(
        "SELECT COUNT(*) FROM dispatches WHERE finished_at IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}
