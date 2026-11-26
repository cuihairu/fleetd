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
         CREATE INDEX IF NOT EXISTS idx_dispatch_seat ON dispatches(seat, fired_at DESC);",
    )?;
    // Seat-control gate migration (2026-11-26 falcon incident): disabled
    // seats must be skipped by every auto-start path. ALTER with tolerance
    // for already-migrated databases.
    for col in ["stopped_by TEXT", "stopped_at INTEGER"] {
        let sql = format!("ALTER TABLE seats ADD COLUMN {col};");
        let _ = c.execute_batch(&sql); // duplicate-column errors are fine
    }
    Ok(c)
}

/// One agent lifecycle event as reported by a Stop hook (or CLI fallback).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SeatEvent {
    pub seat: String,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub open: Option<i64>,
    #[serde(default)]
    pub dirty: Option<i64>,
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_kind() -> String {
    "stop".into()
}

/// Verdict of an event against the seat's in-flight dispatch baseline.
#[derive(Debug, PartialEq, Eq, Clone, Copy, serde::Serialize)]
pub enum Verdict {
    /// HEAD advanced past baseline and the tree is clean.
    Completed,
    /// HEAD advanced but uncommitted changes remain.
    Partial,
    /// HEAD unchanged but the tree is dirty — agent stalled mid-task.
    Stalled,
    /// HEAD unchanged and the tree is clean — nothing was produced.
    NoProgress,
    /// No in-flight dispatch to judge against.
    Untracked,
}

/// Pure classification: baseline vs observed HEAD/dirty. Unit-tested.
pub fn classify(baseline: Option<&str>, head: Option<&str>, dirty: Option<i64>) -> Verdict {
    let dirty = dirty.unwrap_or(0);
    match (baseline, head) {
        (None, _) => Verdict::Untracked,
        // HEAD unknown: cannot judge progress — honestly untracked.
        (Some(_), None) => Verdict::Untracked,
        (Some(b), Some(h)) if h != b => {
            if dirty == 0 {
                Verdict::Completed
            } else {
                Verdict::Partial
            }
        }
        (Some(_), Some(_)) => {
            if dirty > 0 {
                Verdict::Stalled
            } else {
                Verdict::NoProgress
            }
        }
    }
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Completed => "completed",
            Verdict::Partial => "partial",
            Verdict::Stalled => "stalled",
            Verdict::NoProgress => "no_progress",
            Verdict::Untracked => "untracked",
        }
    }
}

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
fn is_terminal_kind(kind: &str) -> bool {
    matches!(kind, "stop" | "session_end" | "subagent_stop" | "teammate_idle")
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
                open_count INTEGER, dirty_count INTEGER, kind TEXT, verdict TEXT);",
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
        let v = ingest(&c, &SeatEvent {
            seat: "kode".into(), head: Some("bbb".into()), open: Some(1),
            dirty: Some(0), kind: "observational".into(),
        }).unwrap();
        assert_eq!(v, Verdict::Untracked);
        let (finished, outcome): (Option<i64>, Option<String>) = c.query_row(
            "SELECT finished_at, outcome FROM dispatches WHERE seat='kode'", [],
            |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert!(finished.is_none(), "observational event must NOT close dispatch");
        assert!(outcome.is_none());
        // terminal stop now closes it correctly
        let v2 = ingest(&c, &SeatEvent {
            seat: "kode".into(), head: Some("bbb".into()), open: Some(1),
            dirty: Some(0), kind: "stop".into(),
        }).unwrap();
        assert_eq!(v2, Verdict::Completed);
        let outcome2: String = c.query_row(
            "SELECT outcome FROM dispatches WHERE seat='kode'", [], |r| r.get(0)).unwrap();
        assert_eq!(outcome2, "completed");
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
}
