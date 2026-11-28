//! Job CLI + scheduler integration helpers for main.rs.
//! Kept separate so main.rs stays a thin router.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Add or update a job. `fleetd job add <name> <schedule> <command>`.
pub fn job_add(c: &Connection, name: &str, schedule: &str, command: &str) -> Result<()> {
    // validate schedule before writing
    let parsed = fleetd_core::schedule::parse_schedule(schedule)?;
    let ts_now = unix_now();
    c.execute(
        "INSERT INTO jobs(name,schedule,command,enabled,created_at) VALUES(?1,?2,?3,1,?4)
         ON CONFLICT(name) DO UPDATE SET schedule=excluded.schedule, command=excluded.command",
        rusqlite::params![name, parsed_display(&parsed), command, ts_now],
    )?;
    println!("job saved: {name} ({})", parsed_display(&parsed));
    Ok(())
}

fn parsed_display(s: &fleetd_core::schedule::Schedule) -> String {
    match s {
        fleetd_core::schedule::Schedule::Cron(x) => x.clone(),
        fleetd_core::schedule::Schedule::Interval { minutes } => format!("every {minutes}m"),
    }
}

pub fn job_list(c: &Connection) -> Result<()> {
    println!(
        "{:<28} {:<16} {:<8} {:<7} {}",
        "name", "schedule", "enabled", "runs", "command"
    );
    let mut st = c.prepare(
        "SELECT j.name, j.schedule, j.enabled,
                (SELECT COUNT(*) FROM job_runs r WHERE r.job_id=j.id) AS runs,
                j.command
         FROM jobs j ORDER BY j.name",
    )?;
    let rows: Vec<(String, String, i64, String)> = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (name, schedule, enabled, command) in rows {
        println!(
            "{name:<28} {schedule:<16} {:<8} {command}",
            if enabled == 1 { "yes" } else { "no" },
        );
    }
    Ok(())
}

pub fn job_enable(c: &Connection, name: &str, enabled: bool) -> Result<()> {
    let n = c.execute(
        "UPDATE jobs SET enabled=?1 WHERE name=?2",
        rusqlite::params![if enabled { 1 } else { 0 }, name],
    )?;
    if n == 0 {
        anyhow::bail!("no such job: {name}");
    }
    println!("job {name}: enabled={enabled}");
    Ok(())
}

pub fn job_remove(c: &Connection, name: &str) -> Result<()> {
    let n = c.execute("DELETE FROM jobs WHERE name=?1", rusqlite::params![name])?;
    if n == 0 {
        anyhow::bail!("no such job: {name}");
    }
    println!("job removed: {name}");
    Ok(())
}

/// Run one job by name immediately, record the run.
pub fn job_run(c: &Connection, name: &str) -> Result<()> {
    let (id, command): (i64, String) = c
        .query_row(
            "SELECT id, command FROM jobs WHERE name=?1",
            rusqlite::params![name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .context("no such job")?;
    let started = unix_now();
    let (code, tail) = crate::scheduler::run_command(&command)?;
    let finished = unix_now();
    c.execute(
        "INSERT INTO job_runs(job_id,started,finished,exit_code,output_tail) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![id, started, finished, code, tail],
    )?;
    c.execute(
        "UPDATE jobs SET last_run=?1 WHERE id=?2",
        rusqlite::params![finished, id],
    )?;
    println!("job {name}: exit={code} ({}s)", finished - started);
    Ok(())
}

/// Show run statistics. `fleetd job stats [name]`.
pub fn job_stats(c: &Connection, name: Option<&str>) -> Result<()> {
    let sql = match name {
        Some(_) => {
            "SELECT j.name, COUNT(r.id), SUM(r.exit_code=0),
                           ROUND(AVG(r.finished-r.started),1), MAX(r.started)
                    FROM jobs j JOIN job_runs r ON r.job_id=j.id
                    WHERE j.name=?1 GROUP BY j.name"
        }
        None => {
            "SELECT j.name, COUNT(r.id), SUM(r.exit_code=0),
                        ROUND(AVG(r.finished-r.started),1), MAX(r.started)
                 FROM jobs j JOIN job_runs r ON r.job_id=j.id
                 GROUP BY j.name ORDER BY j.name"
        }
    };
    println!(
        "{:<28} {:>6} {:>6} {:>9} {:>18}",
        "name", "runs", "ok", "avg_sec", "last_run"
    );
    let mut st = c.prepare(sql)?;
    let rows: Vec<(String, i64, i64, f64, i64)> = if let Some(n) = name {
        st.query_map(rusqlite::params![n], row_map)?
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        st.query_map([], row_map)?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (n, runs, ok, avg, last) in rows {
        let t = fleetd_core::schedule::ts(last)
            .format("%m-%d %H:%M")
            .to_string();
        println!("{n:<28} {runs:>6} {ok:>6} {avg:>9.1} {t:>18}");
    }
    Ok(())
}

fn row_map(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, i64, i64, f64, i64)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

/// One scheduler tick: find due enabled jobs, run them, record stats.
/// Returns count of jobs run. Used by the daemon background thread.
pub fn tick(c: &Connection) -> Result<usize> {
    let now_ts = unix_now();
    let now_dt = fleetd_core::schedule::ts(now_ts);
    let jobs: Vec<(i64, String, String, Option<i64>)> = {
        let mut st = c.prepare("SELECT id, name, schedule, last_run FROM jobs WHERE enabled=1")?;
        let mapped = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        mapped
    };
    let mut ran = 0;
    for (id, name, schedule, last) in jobs {
        let spec = match fleetd_core::schedule::parse_schedule(&schedule) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("bad schedule for {name}: {e}");
                continue;
            }
        };
        let last_dt = last.map(fleetd_core::schedule::ts);
        if !fleetd_core::schedule::is_due(&spec, &now_dt, last_dt.as_ref()) {
            continue;
        }
        let command: String = match c.query_row(
            "SELECT command FROM jobs WHERE id=?1",
            rusqlite::params![id],
            |r| r.get(0),
        ) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("read command for {name}: {e}");
                continue;
            }
        };
        let started = unix_now();
        let (code, tail) = match crate::scheduler::run_command(&command) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("run {name}: {e:#}");
                continue;
            }
        };
        let finished = unix_now();
        let _ = c.execute(
            "INSERT INTO job_runs(job_id,started,finished,exit_code,output_tail)
             VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![id, started, finished, code, tail],
        );
        let _ = c.execute(
            "UPDATE jobs SET last_run=?1 WHERE id=?2",
            rusqlite::params![finished, id],
        );
        eprintln!("job {name}: exit={code}");
        ran += 1;
    }
    Ok(ran)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
