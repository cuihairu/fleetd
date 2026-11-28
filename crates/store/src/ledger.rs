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
    // busy_timeout (FLEETD-DBMETA-001): the first open after a schema bump
    // races the running serve (tray + CLI + serve all open the ledger);
    // waiting on the lock beats failing half-way through a migration.
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
    migrate(&c).with_context(|| format!("migrate ledger {}", path.display()))?;
    Ok(c)
}

// --- schema versioning: meta table + upgrade-only migrations (FLEETD-DBMETA-001) ---
//
// The database records its schema version in a `meta` key/value table (not
// PRAGMA user_version — a table carries the migration history alongside the
// version number). Upgrades run forward only: an old binary opening a newer
// database refuses to start instead of silently tripping over missing
// columns. `SCHEMA_VERSION` bumps by one with every schema change and a
// matching Migration entry is appended below — history is never rewritten.

pub const META_SCHEMA_VERSION: &str = "schema_version";
pub const META_SCHEMA_HISTORY: &str = "schema_history";

/// Latest schema version this binary knows how to write and read.
pub const SCHEMA_VERSION: i64 = 4;

/// Baseline tables (schema v1). CREATE IF NOT EXISTS everywhere: v1 runs
/// over both fresh databases and pre-versioning legacy databases, where the
/// tables already exist and every statement is a no-op.
const BASELINE_SCHEMA: &str = "
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
         CREATE INDEX IF NOT EXISTS idx_memory_tag ON memory(tag);";

/// One schema migration step. `up` runs inside a single transaction together
/// with the version stamp and the history append, so a failed step leaves
/// the database — and its version number — exactly as before (SQLite DDL is
/// transactional).
pub struct Migration {
    pub version: i64,
    pub description: &'static str,
    pub up: fn(&Connection) -> Result<()>,
}

/// Append one column to a table only when it is missing. Legacy databases
/// (pre-meta, this machine's ledger.db included) already carry columns the
/// old try-ALTER era applied: replaying blind ALTERs over them dies on
/// "duplicate column name", so every column migration is presence-guarded.
/// Fresh databases take the ALTER path; the version stamp keeps it to one
/// run either way.
fn add_column_if_missing(c: &Connection, table: &str, col_ddl: &str) -> Result<()> {
    let name = col_ddl
        .split_whitespace()
        .next()
        .context("add_column_if_missing: empty column definition")?;
    let cols: Vec<String> = c
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if cols.iter().any(|col| col == name) {
        return Ok(());
    }
    c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col_ddl};"))?;
    Ok(())
}

/// v1: the baseline table set.
fn m1_baseline(c: &Connection) -> Result<()> {
    c.execute_batch(BASELINE_SCHEMA)?;
    Ok(())
}

/// v2: seats gains the seat-control gate (disabled seats are skipped by
/// every auto-start path; 2026-11-26 falcon incident) and the executor
/// dimension (FLEETD-EXECUTOR-001) — every pre-existing seat is tmux, hence
/// the NOT NULL DEFAULT 'tmux'.
fn m2_seat_gate(c: &Connection) -> Result<()> {
    for col in [
        "stopped_by TEXT",
        "stopped_at INTEGER",
        "executor TEXT NOT NULL DEFAULT 'tmux'",
    ] {
        add_column_if_missing(c, "seats", col)?;
    }
    Ok(())
}

/// v3: dispatches gains the adversarial review gate state machine
/// (FLEETD-GATE-001, 2026-11-28): `state` active → review → done,
/// `acceptance` the task's verification tier, `evidence_path` the reviewer's
/// receipt, `review_note` the rejection reason, `reject_count` the bounce
/// tally. Legacy rows: everything already finished is done (the gate only
/// applies to completions reported after this migration).
fn m3_dispatch_gate(c: &Connection) -> Result<()> {
    for col in [
        "state TEXT NOT NULL DEFAULT 'active'",
        "acceptance TEXT",
        "evidence_path TEXT",
        "review_note TEXT",
        "reject_count INTEGER NOT NULL DEFAULT 0",
    ] {
        add_column_if_missing(c, "dispatches", col)?;
    }
    c.execute_batch(
        "UPDATE dispatches SET state='done'
         WHERE finished_at IS NOT NULL AND finished_at != '' AND state='active';",
    )?;
    Ok(())
}

/// v4: events gains `note` — rejection reasons and gate notifications ride
/// the same event chain the Stop hooks write (free-form payload).
fn m4_event_note(c: &Connection) -> Result<()> {
    add_column_if_missing(c, "events", "note TEXT")
}

/// The ordered migration list. New schema changes append entries with
/// version = previous + 1; existing entries are frozen history.
static MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "baseline tables (tasks/seats/jobs/job_runs/events/dispatches/presence/memory)",
        up: m1_baseline,
    },
    Migration {
        version: 2,
        description: "seats: seat-control gate (stopped_by/stopped_at) + executor dimension",
        up: m2_seat_gate,
    },
    Migration {
        version: 3,
        description: "dispatches: adversarial review gate (state/acceptance/evidence_path/review_note/reject_count) + legacy backfill",
        up: m3_dispatch_gate,
    },
    Migration {
        version: 4,
        description: "events: note column for gate notifications",
        up: m4_event_note,
    },
];

fn ensure_meta_table(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;
    Ok(())
}

/// True when the meta table exists (a database predating the versioning
/// framework has no meta table at all).
fn has_meta_table(c: &Connection) -> Result<bool> {
    Ok(c.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta'",
        [],
        |_| Ok(true),
    )
    .optional()?
    .unwrap_or(false))
}

/// Read one meta value.
pub fn get_meta(c: &Connection, key: &str) -> Result<Option<String>> {
    let mut st = c.prepare("SELECT value FROM meta WHERE key = ?1")?;
    Ok(st
        .query_row(rusqlite::params![key], |r| r.get(0))
        .optional()?)
}

/// Write one meta value (upsert).
pub fn set_meta(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

/// The database's current schema version. A database without the meta table
/// reads as 0 (legacy, pre-versioning); a meta row that fails to parse also
/// reads as 0 so the migration list replays from the top (column migrations
/// are presence-guarded, so replaying them over an already-migrated database
/// is harmless).
pub fn schema_version(c: &Connection) -> Result<i64> {
    if !has_meta_table(c)? {
        return Ok(0);
    }
    Ok(get_meta(c, META_SCHEMA_VERSION)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

/// One applied migration as recorded in the `schema_history` meta value.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MigrationRecord {
    pub version: i64,
    pub description: String,
    pub applied_at: i64,
}

/// The database's migration history (empty for legacy databases).
pub fn schema_history(c: &Connection) -> Result<Vec<MigrationRecord>> {
    let Some(raw) = get_meta(c, META_SCHEMA_HISTORY)? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).context("schema_history meta value is not valid JSON")
}

fn append_history(c: &Connection, version: i64, description: &str, applied_at: i64) -> Result<()> {
    let mut history = schema_history(c)?;
    history.push(MigrationRecord {
        version,
        description: description.to_string(),
        applied_at,
    });
    set_meta(c, META_SCHEMA_HISTORY, &serde_json::to_string(&history)?)?;
    Ok(())
}

/// The upgrade loop, generic over the migration list so tests can drive
/// failure/rollback scenarios with synthetic migrations. Each migration runs
/// in one transaction with its version stamp and history append; a failure
/// anywhere rolls the database back to the previous version. A database
/// newer than SCHEMA_VERSION is refused outright — downgrading is not
/// supported (old binaries must not read schemas they don't know).
fn run_migrations(c: &Connection, migrations: &[Migration]) -> Result<()> {
    ensure_meta_table(c)?;
    let current = schema_version(c)?;
    anyhow::ensure!(
        current <= SCHEMA_VERSION,
        "database schema version {current} is newer than this fleetd supports ({SCHEMA_VERSION}); \
         downgrade not supported — update fleetd instead of rolling the binary back"
    );
    for m in migrations.iter().filter(|m| m.version > current) {
        let tx = c.unchecked_transaction()?;
        (m.up)(&tx).with_context(|| {
            format!(
                "migration v{} ({}) failed; database left at v{current}",
                m.version, m.description
            )
        })?;
        set_meta(&tx, META_SCHEMA_VERSION, &m.version.to_string())?;
        append_history(&tx, m.version, m.description, now_secs())?;
        tx.commit()?;
    }
    Ok(())
}

/// Bring the database up to SCHEMA_VERSION (upgrade only, never down).
pub fn migrate(c: &Connection) -> Result<()> {
    run_migrations(c, MIGRATIONS)
}

/// Read-only schema report for `fleetd db info`. Deliberately no migrate:
/// the point of this face is reporting exactly the state open() would
/// refuse — legacy and too-new databases included.
pub struct DbSchemaInfo {
    pub db_version: i64,
    pub has_meta: bool,
    pub history: Vec<MigrationRecord>,
}

pub fn db_schema_info(c: &Connection) -> Result<DbSchemaInfo> {
    let has_meta = has_meta_table(c)?;
    let history = if has_meta {
        schema_history(c)?
    } else {
        Vec::new()
    };
    Ok(DbSchemaInfo {
        db_version: schema_version(c)?,
        has_meta,
        history,
    })
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
/// A Completed verdict no longer lands as done directly (FLEETD-GATE-001):
/// the gate function decides between direct-done (self-check tasks) and the
/// adversarial `review` state where a checker must pass it with evidence.
pub fn ingest(c: &Connection, ev: &SeatEvent) -> Result<Verdict> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let terminal = is_terminal_kind(&ev.kind);
    // Only classify when the event is terminal AND we have a baseline.
    let target: Option<(i64, String, Option<String>)> = if terminal {
        inflight(c, &ev.seat)?
    } else {
        None
    };
    let baseline: Option<String> = target.as_ref().and_then(|(_, _, b)| b.clone());
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
    // Closed by id (not by seat-wide WHERE): a re-opened review_failed row
    // must be the exact row this verdict was classified against.
    if let Some((id, tag, _)) = target {
        close_dispatch(c, id, &tag, verdict, ev, ts)?;
    }
    Ok(verdict)
}

/// Apply the gate to a terminal verdict against one dispatch row.
/// Non-Completed verdicts keep the legacy close semantics (finished with
/// their outcome; state='done' means merely "closed" — progress stats still
/// split by outcome, so a stalled close never counts as completed work).
fn close_dispatch(
    c: &Connection,
    id: i64,
    tag: &str,
    verdict: Verdict,
    ev: &SeatEvent,
    ts: i64,
) -> Result<()> {
    let decision = if verdict == Verdict::Completed {
        let spec = task_spec_for_tag(tag);
        fleetd_core::gate::gate(verdict, &spec)
    } else {
        fleetd_core::gate::GateDecision::Reject
    };
    match decision {
        fleetd_core::gate::GateDecision::DirectDone => {
            c.execute(
                "UPDATE dispatches SET finished_at=?1, outcome=?2, head_at_finish=?3,
                 open_at_finish=?4, dirty_at_finish=?5, state='done',
                 acceptance=?6 WHERE id=?7",
                rusqlite::params![
                    ts,
                    verdict.as_str(),
                    ev.head,
                    ev.open,
                    ev.dirty,
                    fleetd_core::gate::Acceptance::SelfCheck.as_str(),
                    id
                ],
            )?;
        }
        fleetd_core::gate::GateDecision::EnterReview => {
            let spec = task_spec_for_tag(tag);
            c.execute(
                "UPDATE dispatches SET finished_at=?1, outcome=?2, head_at_finish=?3,
                 open_at_finish=?4, dirty_at_finish=?5, state='review',
                 acceptance=?6 WHERE id=?7",
                rusqlite::params![
                    ts,
                    verdict.as_str(),
                    ev.head,
                    ev.open,
                    ev.dirty,
                    spec.acceptance.as_str(),
                    id
                ],
            )?;
            // Gate notification into the event chain: the orchestrator board
            // (and review list) picks review_pending up from here.
            let note = match spec.acceptance {
                fleetd_core::gate::Acceptance::UserVerify => Some(
                    "user-verify required: dispatch awaits the user's personal check".to_string(),
                ),
                _ => Some(format!("dispatch {id} awaits adversarial review")),
            };
            record_event(
                c,
                &ev.seat,
                "review_pending",
                ev.head.as_deref(),
                note.as_deref(),
            )?;
        }
        fleetd_core::gate::GateDecision::Reject => {
            c.execute(
                "UPDATE dispatches SET finished_at=?1, outcome=?2, head_at_finish=?3,
                 open_at_finish=?4, dirty_at_finish=?5, state='done' WHERE id=?6",
                rusqlite::params![ts, verdict.as_str(), ev.head, ev.open, ev.dirty, id],
            )?;
        }
    }
    Ok(())
}

/// Resolve the TaskSpec for a dispatch tag. The tag may carry an inline
/// `acceptance:` declaration (natural-language dispatches paste the whole
/// task text as the tag), name a task file on disk (seat fire / relay queue
/// pass full paths), or be a bare task-file name under the hermes scratch
/// dir. Anything unreadable defaults to adversarial — the safe gate.
fn task_spec_for_tag(tag: &str) -> fleetd_core::gate::TaskSpec {
    use fleetd_core::gate::{parse_acceptance, TaskSpec};
    if let Some(a) = parse_acceptance(tag) {
        return TaskSpec { acceptance: a };
    }
    let candidates = [
        std::path::PathBuf::from(tag.trim()),
        PathBuf::from(home_dir())
            .join(".hermes/cache/scratch/tasks")
            .join(format!("{}.md", tag.trim())),
    ];
    for path in &candidates {
        if path.is_file() {
            if let Ok(text) = std::fs::read_to_string(path) {
                return TaskSpec::from_task_text(&text);
            }
        }
    }
    TaskSpec::default()
}

/// Register a dispatch so later events can be classified. The acceptance
/// tier is resolved from the task text up front so review faces can show
/// what mode of check the dispatch owes.
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
    // Baseline capture (FLEETD-GATE-001): verdicts classify the seat's HEAD
    // against this baseline — without one every stop event reads untracked
    // and the gate never sees a completion. Callers rarely know the head, so
    // capture it from the seat workspace (the fleet-wide seat=workspace-dir
    // convention). Best effort: remote seats without a local checkout stay
    // None (untracked, the pre-gate behavior).
    let captured = baseline
        .map(|b| b.to_string())
        .or_else(|| capture_seat_head(seat));
    c.execute(
        "INSERT INTO dispatches(seat,tag,fired_at,baseline_head,acceptance)
         VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![
            seat,
            tag,
            ts,
            captured,
            task_spec_for_tag(tag).acceptance.as_str()
        ],
    )?;
    Ok(())
}

/// Current short HEAD of a seat's workspace repo; None when the dir is
/// missing or not a repository.
fn capture_seat_head(seat: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &format!("/home/cui/workspaces/{seat}"),
            "rev-parse",
            "--short",
            "HEAD",
        ])
        .output()
        .ok()?;
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !head.is_empty()).then_some(head)
}

/// Per-seat aggregate counts for `fleetd status` over the last `days`.
/// `completed` counts only gate-passed completions (state='done' with the
/// completed outcome); `review`/`review_failed` surface the gate backlog so
/// the panel shows work waiting on a checker, not silently "done".
pub struct SeatProgress {
    pub seat: String,
    pub fired: i64,
    pub completed: i64,
    pub inflight: i64,
    pub stalled: i64,
    pub partial: i64,
    pub review: i64,
    pub review_failed: i64,
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
            COALESCE(SUM(outcome='completed' AND state='done'), 0) AS completed,
            COALESCE(SUM(outcome IS NULL), 0) AS inflight,
            COALESCE(SUM(outcome='stalled'), 0) AS stalled,
            COALESCE(SUM(outcome='partial'), 0) AS partial,
            COALESCE(SUM(state='review'), 0) AS review,
            COALESCE(SUM(state='review_failed'), 0) AS review_failed
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
                review: r.get(6)?,
                review_failed: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// --- adversarial review gate face (FLEETD-GATE-001, 2026-11-28) ---

/// One dispatch awaiting (or bounced from) adversarial review.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReviewRow {
    pub id: i64,
    pub seat: String,
    pub tag: String,
    /// 'review' (awaiting a checker) or 'review_failed' (bounced, re-opened,
    /// awaiting the seat's next verdict).
    pub state: String,
    pub acceptance: Option<String>,
    pub baseline_head: Option<String>,
    pub head_at_finish: Option<String>,
    pub fired_at: i64,
    pub finished_at: Option<i64>,
    pub reject_count: i64,
    pub review_note: Option<String>,
    pub evidence_path: Option<String>,
}

fn review_row(r: &rusqlite::Row) -> rusqlite::Result<ReviewRow> {
    Ok(ReviewRow {
        id: r.get(0)?,
        seat: r.get(1)?,
        tag: r.get(2)?,
        state: r.get(3)?,
        acceptance: r.get(4)?,
        baseline_head: r.get(5)?,
        head_at_finish: r.get(6)?,
        fired_at: r.get(7)?,
        finished_at: r.get(8)?,
        reject_count: r.get(9)?,
        review_note: r.get(10)?,
        evidence_path: r.get(11)?,
    })
}

const REVIEW_COLS: &str =
    "id,seat,tag,state,acceptance,baseline_head,head_at_finish,fired_at,finished_at,\
     reject_count,review_note,evidence_path";

/// The review backlog: every dispatch in `review` or `review_failed`, newest
/// fire first. This is the checker's work queue — entries leave only through
/// review_pass / review_fail.
pub fn review_list(c: &Connection) -> Result<Vec<ReviewRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {REVIEW_COLS} FROM dispatches
         WHERE state IN ('review','review_failed')
         ORDER BY fired_at DESC"
    ))?;
    let rows = st
        .query_map([], review_row)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn dispatch_state(c: &Connection, id: i64) -> Result<Option<String>> {
    let mut st = c.prepare("SELECT state FROM dispatches WHERE id = ?1")?;
    Ok(st
        .query_row(rusqlite::params![id], |r| r.get(0))
        .optional()?)
}

// --- dashboard read faces (FLEETD-DASHBOARD-001) ---
// Read-only list queries backing the web board's dispatches block and event
// tail. JSON-only GET exposure; never a write path.

/// One dispatch in the board list (any state).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DispatchRow {
    pub id: i64,
    pub seat: String,
    pub tag: String,
    /// fired / inflight / partial / done / review / review_failed / stalled…
    pub state: String,
    pub fired_at: i64,
    pub finished_at: Option<i64>,
    pub baseline_head: Option<String>,
    pub head_at_finish: Option<String>,
    pub reject_count: i64,
    pub evidence_path: Option<String>,
}

/// Recent dispatches, newest fire first, capped at `limit` — the board's
/// dispatches block (in-flight/review highlighting is the page's concern).
pub fn dispatch_list(c: &Connection, limit: u32) -> Result<Vec<DispatchRow>> {
    let mut st = c.prepare(
        "SELECT id,seat,tag,state,fired_at,finished_at,baseline_head,head_at_finish,\
         reject_count,evidence_path
         FROM dispatches ORDER BY fired_at DESC, id DESC LIMIT ?1",
    )?;
    let rows = st
        .query_map(rusqlite::params![limit], |r| {
            Ok(DispatchRow {
                id: r.get(0)?,
                seat: r.get(1)?,
                tag: r.get(2)?,
                state: r.get(3)?,
                fired_at: r.get(4)?,
                finished_at: r.get(5)?,
                baseline_head: r.get(6)?,
                head_at_finish: r.get(7)?,
                reject_count: r.get(8)?,
                evidence_path: r.get(9)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One ledger event in the board's tail.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventRow {
    pub id: i64,
    pub ts: i64,
    pub seat: String,
    pub kind: String,
    pub verdict: Option<String>,
    pub head: Option<String>,
    pub open_count: Option<i64>,
    pub dirty_count: Option<i64>,
}

/// Recent events, newest first, capped at `limit` — the board's event tail.
pub fn events_list(c: &Connection, limit: u32) -> Result<Vec<EventRow>> {
    let mut st = c.prepare(
        "SELECT id,ts,seat,kind,verdict,head,open_count,dirty_count
         FROM events ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = st
        .query_map(rusqlite::params![limit], |r| {
            Ok(EventRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                seat: r.get(2)?,
                kind: r.get(3)?,
                verdict: r.get(4)?,
                head: r.get(5)?,
                open_count: r.get(6)?,
                dirty_count: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Pass a review: the dispatch lands as done and the evidence path is
/// recorded for audit. Evidence is mandatory — "no evidence, no done" is
/// the whole hook this gate exists to enforce.
pub fn review_pass(c: &Connection, id: i64, evidence: &str) -> Result<()> {
    let state = dispatch_state(c, id)?.context(format!("no such dispatch {id}"))?;
    anyhow::ensure!(
        state == "review",
        "dispatch {id} is not awaiting review (state={state})"
    );
    anyhow::ensure!(
        !evidence.trim().is_empty(),
        "evidence required: review pass without evidence is refused"
    );
    c.execute(
        "UPDATE dispatches SET state='done', evidence_path=?2 WHERE id=?1",
        rusqlite::params![id, evidence.trim()],
    )?;
    Ok(())
}

/// Fail a review: bounce the dispatch back to the seat — state becomes
/// review_failed and the row re-opens (finished_at cleared) so the seat's
/// next Stop-hook verdict re-classifies against it. The reason goes on the
/// row (review_note) and into the event chain (review_rejected). Returns
/// true when this bounce crossed the escalation threshold (>= 2 rejections
/// escalates to the orchestrator).
pub fn review_fail(c: &Connection, id: i64, reason: &str) -> Result<bool> {
    let state = dispatch_state(c, id)?.context(format!("no such dispatch {id}"))?;
    anyhow::ensure!(
        state == "review",
        "dispatch {id} is not awaiting review (state={state})"
    );
    anyhow::ensure!(
        !reason.trim().is_empty(),
        "reason required: a rejection must say what failed"
    );
    let (seat, rejects): (String, i64) = c.query_row(
        "SELECT seat, reject_count FROM dispatches WHERE id = ?1",
        rusqlite::params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    c.execute(
        "UPDATE dispatches SET state='review_failed', review_note=?2,
         reject_count=reject_count+1, finished_at=NULL, outcome=NULL WHERE id=?1",
        rusqlite::params![id, reason.trim()],
    )?;
    record_event(
        c,
        &seat,
        "review_rejected",
        None,
        Some(&format!("dispatch {id} bounced: {}", reason.trim())),
    )?;
    let escalated = rejects + 1 >= 2;
    if escalated {
        record_event(
            c,
            &seat,
            "review_escalated",
            None,
            Some(&format!(
                "dispatch {id} rejected {} times; orchestrator must intervene",
                rejects + 1
            )),
        )?;
    }
    Ok(escalated)
}

/// Per-seat count of dispatches awaiting review — the board field every
/// dashboard reads from /v1/fleet to see the gate backlog at a glance.
pub fn review_counts_by_seat(c: &Connection) -> Result<Vec<(String, i64)>> {
    let mut st = c.prepare(
        "SELECT seat, COUNT(*) FROM dispatches
         WHERE state='review' GROUP BY seat ORDER BY seat",
    )?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Review-stale watchdog: reviews older than 24h get exactly one
/// review_stale event each (deduped by the dispatch reference in the note).
/// Deliberately NO auto-pass — a stale review keeps waiting; only the
/// orchestrator decides (宁可积压不可虚过).
pub fn review_stale_alerts(c: &Connection) -> Result<Vec<i64>> {
    const REVIEW_STALE_SECS: i64 = 24 * 3600;
    let cutoff = now_secs() - REVIEW_STALE_SECS;
    let mut st = c.prepare(
        "SELECT id, seat FROM dispatches
         WHERE state='review' AND finished_at IS NOT NULL AND finished_at < ?1
         ORDER BY id",
    )?;
    let stale: Vec<(i64, String)> = st
        .query_map(rusqlite::params![cutoff], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut alerted = Vec::new();
    for (id, seat) in stale {
        let pattern = format!("dispatch={id} %");
        let seen: bool = c
            .query_row(
                "SELECT 1 FROM events WHERE kind='review_stale' AND note LIKE ?1 LIMIT 1",
                rusqlite::params![pattern],
                |_r| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !seen {
            record_event(
                c,
                &seat,
                "review_stale",
                None,
                Some(&format!(
                    "dispatch={id} in review >24h; orchestrator check overdue"
                )),
            )?;
            alerted.push(id);
        }
    }
    Ok(alerted)
}

/// Append a control-plane event. Gate notifications ride the same events
/// chain the Stop hooks write, so every existing reader sees them.
pub fn record_event(
    c: &Connection,
    seat: &str,
    kind: &str,
    head: Option<&str>,
    note: Option<&str>,
) -> Result<()> {
    let ts = now_secs();
    c.execute(
        "INSERT INTO events(ts,seat,head,kind,note) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![ts, seat, head, kind, note],
    )?;
    Ok(())
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
                head_at_finish TEXT, open_at_finish INTEGER, dirty_at_finish INTEGER, note TEXT,
                state TEXT NOT NULL DEFAULT 'active', acceptance TEXT,
                evidence_path TEXT, review_note TEXT,
                reject_count INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL, seat TEXT NOT NULL, head TEXT,
                open_count INTEGER, dirty_count INTEGER, kind TEXT, verdict TEXT,
                note TEXT);
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
        let (outcome, state): (String, String) = c
            .query_row(
                "SELECT outcome, state FROM dispatches WHERE seat='kode'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(outcome, "completed");
        // The gate (FLEETD-GATE-001): a bare task defaults to adversarial,
        // so the completed dispatch waits in review, not done.
        assert_eq!(state, "review");
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
        // t1 self-check: its completion passes the gate and counts as done.
        record_dispatch(&c, "kode", "t1\nacceptance: self-check", Some("aaa")).unwrap();
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
    fn dashboard_lists_newest_first_and_capped() {
        let c = mem();
        for (seat, tag, state, fired) in [
            ("a", "old", "done", 1000),
            ("b", "newest", "review", 3000),
            ("c", "mid", "active", 2000),
        ] {
            c.execute(
                "INSERT INTO dispatches(seat,tag,fired_at,state) VALUES(?1,?2,?3,?4)",
                rusqlite::params![seat, tag, fired, state],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO events(ts,seat,kind,verdict) VALUES(100,'a','stop','clean')",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO events(ts,seat,kind) VALUES(200,'b','pane_died')",
            [],
        )
        .unwrap();
        let d = dispatch_list(&c, 2).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].tag, "newest", "newest fire first");
        assert_eq!(d[0].state, "review");
        assert_eq!(d[1].tag, "mid");
        let e = events_list(&c, 1).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].seat, "b", "newest id first");
        assert_eq!(e[0].verdict, None);
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

    // --- adversarial review gate (FLEETD-GATE-001, 2026-11-28) ---

    /// Drive one dispatch to the review state (adversarial default).
    fn dispatch_in_review(c: &Connection, seat: &str, tag: &str) -> i64 {
        record_dispatch(c, seat, tag, Some("aaa")).unwrap();
        ingest(
            c,
            &SeatEvent {
                seat: seat.into(),
                head: Some("bbb".into()),
                open: Some(0),
                blocked: Some(0),
                dirty: Some(0),
                kind: "stop".into(),
            },
        )
        .unwrap();
        c.query_row(
            "SELECT id FROM dispatches WHERE seat=?1 ORDER BY id DESC LIMIT 1",
            rusqlite::params![seat],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn dispatch_col(c: &Connection, id: i64, col: &str) -> String {
        c.query_row(
            &format!("SELECT {col} FROM dispatches WHERE id=?1"),
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn event_count(c: &Connection, kind: &str) -> i64 {
        c.query_row(
            "SELECT COUNT(*) FROM events WHERE kind=?1",
            rusqlite::params![kind],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn completed_adversarial_enters_review_with_pending_event() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "kode-x-001");
        assert_eq!(dispatch_col(&c, id, "state"), "review");
        assert_eq!(dispatch_col(&c, id, "outcome"), "completed");
        assert_eq!(dispatch_col(&c, id, "acceptance"), "adversarial");
        assert_eq!(event_count(&c, "review_pending"), 1);
    }

    #[test]
    fn completed_self_check_lands_done_without_review() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "t-docs\nacceptance: self-check");
        assert_eq!(dispatch_col(&c, id, "state"), "done");
        assert_eq!(dispatch_col(&c, id, "acceptance"), "self-check");
        assert_eq!(event_count(&c, "review_pending"), 0);
    }

    #[test]
    fn completed_user_verify_enters_review_and_flags_it() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "t-beacon\nacceptance: user-verify");
        assert_eq!(dispatch_col(&c, id, "state"), "review");
        assert_eq!(dispatch_col(&c, id, "acceptance"), "user-verify");
        let note: String = c
            .query_row(
                "SELECT note FROM events WHERE kind='review_pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(note.contains("user-verify"), "user-verify marker: {note}");
    }

    #[test]
    fn review_pass_without_evidence_is_refused() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "kode-x-001");
        assert!(review_pass(&c, id, "").is_err());
        assert!(review_pass(&c, id, "   ").is_err());
        assert_eq!(dispatch_col(&c, id, "state"), "review");
    }

    #[test]
    fn review_pass_records_evidence_and_done() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "kode-x-001");
        review_pass(&c, id, "/tmp/fleetd-e2e/proof.txt").unwrap();
        assert_eq!(dispatch_col(&c, id, "state"), "done");
        assert_eq!(
            dispatch_col(&c, id, "evidence_path"),
            "/tmp/fleetd-e2e/proof.txt"
        );
        // Done is terminal: a second pass must refuse.
        assert!(review_pass(&c, id, "/tmp/again.txt").is_err());
    }

    #[test]
    fn review_fail_bounces_reopens_and_records_reason() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "kode-x-001");
        let escalated = review_fail(&c, id, "tests do not compile").unwrap();
        assert!(!escalated, "first rejection does not escalate");
        assert_eq!(dispatch_col(&c, id, "state"), "review_failed");
        assert_eq!(dispatch_col(&c, id, "review_note"), "tests do not compile");
        assert_eq!(dispatch_col(&c, id, "CAST(reject_count AS TEXT)"), "1");
        // Re-opened: the seat's next Stop hook must be able to re-classify.
        let finished: Option<i64> = c
            .query_row(
                "SELECT finished_at FROM dispatches WHERE id=?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(finished.is_none(), "bounced dispatch re-opens");
        let note: String = c
            .query_row(
                "SELECT note FROM events WHERE kind='review_rejected'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(note.contains("tests do not compile"));
    }

    #[test]
    fn review_fail_twice_escalates_to_orchestrator() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "kode-x-001");
        assert!(!review_fail(&c, id, "wrong repo").unwrap());
        // Seat re-submits: baseline aaa, head still advanced → completed again.
        ingest(
            &c,
            &SeatEvent {
                seat: "kode".into(),
                head: Some("ccc".into()),
                open: Some(0),
                blocked: Some(0),
                dirty: Some(0),
                kind: "stop".into(),
            },
        )
        .unwrap();
        assert_eq!(dispatch_col(&c, id, "state"), "review");
        assert!(
            review_fail(&c, id, "still wrong repo").unwrap(),
            "second rejection escalates"
        );
        assert_eq!(dispatch_col(&c, id, "CAST(reject_count AS TEXT)"), "2");
        assert_eq!(event_count(&c, "review_escalated"), 1);
    }

    #[test]
    fn review_faces_reject_non_review_states() {
        let c = mem();
        // Active dispatch never entered review.
        record_dispatch(&c, "kode", "kode-x-001", Some("aaa")).unwrap();
        let id: i64 = c
            .query_row("SELECT id FROM dispatches WHERE seat='kode'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(review_pass(&c, id, "/tmp/x").is_err());
        assert!(review_fail(&c, id, "nope").is_err());
        // Unknown id.
        assert!(review_pass(&c, 9999, "/tmp/x").is_err());
        assert!(review_fail(&c, 9999, "nope").is_err());
    }

    #[test]
    fn bounced_dispatch_reclassifies_on_next_verdict() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "kode-x-001");
        review_fail(&c, id, "incomplete").unwrap();
        // Next stop is partial (dirty tree): legacy semantics close it, no review.
        ingest(
            &c,
            &SeatEvent {
                seat: "kode".into(),
                head: Some("bbb".into()),
                open: Some(2),
                blocked: Some(0),
                dirty: Some(2),
                kind: "stop".into(),
            },
        )
        .unwrap();
        assert_eq!(dispatch_col(&c, id, "state"), "done");
        assert_eq!(dispatch_col(&c, id, "outcome"), "partial");
        // A later completed stop re-enters review through the gate.
        let id2 = dispatch_in_review(&c, "falcon", "falcon-t");
        review_fail(&c, id2, "again").unwrap();
        ingest(
            &c,
            &SeatEvent {
                seat: "falcon".into(),
                head: Some("ddd".into()),
                open: Some(0),
                blocked: Some(0),
                dirty: Some(0),
                kind: "stop".into(),
            },
        )
        .unwrap();
        assert_eq!(dispatch_col(&c, id2, "state"), "review");
    }

    #[test]
    fn review_list_and_counts_surface_the_backlog() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "kode-x-001");
        dispatch_in_review(&c, "falcon", "falcon-docs\nacceptance: self-check"); // lands done
        let counts = review_counts_by_seat(&c).unwrap();
        assert_eq!(counts, vec![("kode".to_string(), 1)]);
        let rows = review_list(&c).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].seat, "kode");
        assert_eq!(rows[0].state, "review");
        assert_eq!(rows[0].baseline_head.as_deref(), Some("aaa"));
        assert_eq!(rows[0].head_at_finish.as_deref(), Some("bbb"));
        assert_eq!(rows[0].reject_count, 0);
    }

    #[test]
    fn review_stale_alerts_fire_once_per_dispatch() {
        let c = mem();
        let id = dispatch_in_review(&c, "kode", "kode-x-001");
        // Fresh review: nothing stale.
        assert!(review_stale_alerts(&c).unwrap().is_empty());
        // Age the review past 24h.
        let old = now_secs() - 25 * 3600;
        c.execute(
            "UPDATE dispatches SET finished_at=?1 WHERE id=?2",
            rusqlite::params![old, id],
        )
        .unwrap();
        assert_eq!(review_stale_alerts(&c).unwrap(), vec![id]);
        assert_eq!(event_count(&c, "review_stale"), 1);
        // Dedupe: the second sweep must not re-alert.
        assert!(review_stale_alerts(&c).unwrap().is_empty());
        assert_eq!(event_count(&c, "review_stale"), 1);
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

// --- schema versioning tests (FLEETD-DBMETA-001) ---

#[cfg(test)]
mod migration_tests {
    use super::*;

    fn table_exists(c: &Connection, name: &str) -> bool {
        c.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            rusqlite::params![name],
            |_| Ok(true),
        )
        .optional()
        .unwrap()
        .unwrap_or(false)
    }

    fn has_col(c: &Connection, table: &str, col: &str) -> bool {
        c.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<String>, _>>()
            .unwrap()
            .iter()
            .any(|x| x == col)
    }

    fn stamped_version(c: &Connection) -> i64 {
        get_meta(c, META_SCHEMA_VERSION)
            .unwrap()
            .expect("schema_version meta row must exist after migrate")
            .parse()
            .unwrap()
    }

    #[test]
    fn fresh_db_reaches_latest_version() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        assert_eq!(stamped_version(&c), SCHEMA_VERSION);
        for table in [
            "meta",
            "tasks",
            "seats",
            "jobs",
            "job_runs",
            "events",
            "dispatches",
            "presence",
            "memory",
        ] {
            assert!(table_exists(&c, table), "table {table} must exist");
        }
        // Latest-version schema carries every dimension the faces read.
        assert!(has_col(&c, "seats", "executor"));
        assert!(has_col(&c, "dispatches", "state"));
        assert!(has_col(&c, "dispatches", "reject_count"));
        assert!(has_col(&c, "events", "note"));
    }

    #[test]
    fn legacy_metaless_db_migrates_without_duplicate_column_errors() {
        // Pre-versioning database shape: all current tables/columns (the old
        // try-ALTER era already applied them) but no meta table. Blind ALTERs
        // would die on "duplicate column name" — the presence guards are what
        // make replaying v2..v4 safe here.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE seats (seat TEXT PRIMARY KEY, cwd TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'idle', current_tag TEXT,
                baseline_head TEXT, updated_at INTEGER NOT NULL,
                stopped_by TEXT, stopped_at INTEGER,
                executor TEXT NOT NULL DEFAULT 'tmux');
             CREATE TABLE dispatches (id INTEGER PRIMARY KEY AUTOINCREMENT,
                seat TEXT NOT NULL, tag TEXT NOT NULL, fired_at INTEGER NOT NULL,
                baseline_head TEXT, finished_at INTEGER, outcome TEXT,
                head_at_finish TEXT, open_at_finish INTEGER, dirty_at_finish INTEGER,
                note TEXT, state TEXT NOT NULL DEFAULT 'active', acceptance TEXT,
                evidence_path TEXT, review_note TEXT,
                reject_count INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL, seat TEXT NOT NULL, head TEXT,
                open_count INTEGER, dirty_count INTEGER, kind TEXT, verdict TEXT,
                note TEXT);
             INSERT INTO dispatches (seat, tag, fired_at, finished_at, state)
                VALUES ('kode', 'old-task', 1, 2, 'active');",
        )
        .unwrap();
        migrate(&c).unwrap();
        assert_eq!(stamped_version(&c), SCHEMA_VERSION);
        // No column got duplicated and the legacy row survived untouched.
        let (n, state): (i64, String) = c
            .query_row("SELECT COUNT(*), MAX(state) FROM dispatches", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(n, 1);
        // Backfill marks the finished legacy row done (old try-ALTER behavior).
        assert_eq!(state, "done");
    }

    #[test]
    fn legacy_baseline_db_gains_missing_columns_and_backfills() {
        // Oldest legacy shape: baseline tables only, no gate columns at all.
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE dispatches (id INTEGER PRIMARY KEY AUTOINCREMENT,
                seat TEXT NOT NULL, tag TEXT NOT NULL, fired_at INTEGER NOT NULL,
                baseline_head TEXT, finished_at INTEGER, outcome TEXT,
                head_at_finish TEXT, open_at_finish INTEGER, dirty_at_finish INTEGER,
                note TEXT);
             CREATE TABLE seats (seat TEXT PRIMARY KEY, cwd TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'idle', current_tag TEXT,
                baseline_head TEXT, updated_at INTEGER NOT NULL);
             CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL, seat TEXT NOT NULL, head TEXT,
                open_count INTEGER, dirty_count INTEGER, kind TEXT, verdict TEXT);
             INSERT INTO dispatches (seat, tag, fired_at, finished_at)
                VALUES ('a', 'done-old', 1, 2);
             INSERT INTO dispatches (seat, tag, fired_at) VALUES ('b', 'open-old', 3);",
        )
        .unwrap();
        migrate(&c).unwrap();
        assert_eq!(stamped_version(&c), SCHEMA_VERSION);
        for col in ["stopped_by", "stopped_at", "executor"] {
            assert!(has_col(&c, "seats", col), "seats.{col} must be added");
        }
        for col in [
            "state",
            "acceptance",
            "evidence_path",
            "review_note",
            "reject_count",
        ] {
            assert!(
                has_col(&c, "dispatches", col),
                "dispatches.{col} must be added"
            );
        }
        assert!(has_col(&c, "events", "note"));
        // Backfill: the finished legacy row reads done; the in-flight one
        // keeps the active default.
        let states: Vec<String> = c
            .prepare("SELECT state FROM dispatches ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(states, vec!["done".to_string(), "active".to_string()]);
    }

    #[test]
    fn failed_migration_rolls_back_version_and_ddl() {
        // Synthetic migrations drive the failure path: v2 fails midway after
        // creating a table; the transaction must leave version AND DDL at v1.
        let c = Connection::open_in_memory().unwrap();
        let boom: fn(&Connection) -> Result<()> = |c| {
            c.execute_batch("CREATE TABLE should_rollback (id INTEGER);")?;
            anyhow::bail!("deliberate failure mid-migration")
        };
        let list = &[
            Migration {
                version: 1,
                description: "ok step",
                up: |c: &Connection| Ok(c.execute_batch("CREATE TABLE stayed (id INTEGER);")?),
            },
            Migration {
                version: 2,
                description: "boom step",
                up: boom,
            },
        ];
        let err = run_migrations(&c, list).unwrap_err();
        assert!(err.to_string().contains("boom step"));
        assert_eq!(stamped_version(&c), 1, "failed step must not stamp");
        assert!(table_exists(&c, "stayed"), "v1 committed");
        assert!(
            !table_exists(&c, "should_rollback"),
            "failed step's DDL must roll back"
        );
        assert_eq!(schema_history(&c).unwrap().len(), 1);
        // Recovery: fixing the step and re-running completes the upgrade.
        let fixed = &[
            Migration {
                version: 1,
                description: "ok step",
                up: |c: &Connection| Ok(c.execute_batch("CREATE TABLE stayed (id INTEGER);")?),
            },
            Migration {
                version: 2,
                description: "fixed step",
                up: |c: &Connection| Ok(c.execute_batch("CREATE TABLE fine (id INTEGER);")?),
            },
        ];
        run_migrations(&c, fixed).unwrap();
        assert_eq!(stamped_version(&c), 2);
        assert!(table_exists(&c, "fine"));
        assert_eq!(schema_history(&c).unwrap().len(), 2);
    }

    #[test]
    fn downgrade_is_refused() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        // A binary older than the database's writer must refuse, not improvise.
        set_meta(&c, META_SCHEMA_VERSION, &(SCHEMA_VERSION + 1).to_string()).unwrap();
        let err = migrate(&c).unwrap_err();
        assert!(err.to_string().contains("downgrade not supported"));
        // The refusal must not have touched history or the version stamp.
        assert_eq!(stamped_version(&c), SCHEMA_VERSION + 1);
        assert_eq!(schema_history(&c).unwrap().len(), MIGRATIONS.len() as usize);
    }

    #[test]
    fn migrate_is_idempotent() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        let v1 = stamped_version(&c);
        let h1 = schema_history(&c).unwrap();
        migrate(&c).unwrap();
        assert_eq!(stamped_version(&c), v1);
        assert_eq!(schema_history(&c).unwrap(), h1);
    }

    #[test]
    fn history_records_every_migration() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        let history = schema_history(&c).unwrap();
        assert_eq!(history.len(), MIGRATIONS.len());
        for (record, m) in history.iter().zip(MIGRATIONS.iter()) {
            assert_eq!(record.version, m.version);
            assert_eq!(record.description, m.description);
            assert!(record.applied_at > 0);
        }
        // Versions ascend without gaps — the framework's core invariant.
        let versions: Vec<i64> = history.iter().map(|r| r.version).collect();
        let expected: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert_eq!(versions, expected);
    }
}
