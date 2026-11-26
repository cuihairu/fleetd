//! Self-managed user-space scheduler (crontab/Hermes-cron/systemd-timer replacement).
//!
//! Design goals (user order 2026-11-26):
//! - no special permission requests: everything runs in the user's daemon
//! - unified ledger + statistics (runs, exit codes, durations) in SQLite
//! - shared with the event/dispatch plane via the same fleetd binary
//!
//! Two schedule kinds:
//! - cron: standard 5-field expression (min hour day month weekday)
//! - interval: every N minutes

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Local, Timelike};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    Cron(String),
    Interval { minutes: i64 },
}

/// Parse a schedule string: "*/5 * * * *" or "every 5m" / "@every 15m".
pub fn parse_schedule(s: &str) -> Result<Schedule> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("every ").or_else(|| lower.strip_prefix("@every ")) {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let unit: String = rest[digits.len()..].chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let n: i64 = digits.parse().context("interval must include a number")?;
        if n <= 0 { bail!("interval must be > 0"); }
        let minutes = match unit.as_str() {
            "s" | "sec" | "secs" => (n + 59) / 60,
            "m" | "min" | "mins" | "" => n,
            "h" | "hour" | "hours" => n * 60,
            "d" | "day" | "days" => n * 1440,
            _ => bail!("unknown interval unit: {unit}"),
        };
        return Ok(Schedule::Interval { minutes });
    }
    // cron: exactly 5 fields
    let fields: Vec<&str> = t.split_whitespace().collect();
    if fields.len() != 5 {
        bail!("expected 5 cron fields or 'every Nm'; got: {s}");
    }
    for (i, f) in fields.iter().enumerate() {
        validate_field(f, i)?;
    }
    Ok(Schedule::Cron(t.to_string()))
}

fn validate_field(field: &str, index: usize) -> Result<()> {
    let (lo, hi) = match index {
        0 => (0, 59),
        1 => (0, 23),
        2 => (1, 31),
        3 => (1, 12),
        4 => (0, 7),
        _ => (0, 0),
    };
    for part in field.split(',') {
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => (b, Some(s.parse::<u32>().context("bad step")?)),
            None => (part, None),
        };
        if let Some(s) = step { if s == 0 { bail!("step must be > 0"); } }
        if base == "*" { continue; }
        let (a, b) = match base.split_once('-') {
            Some((x, y)) => (x.parse::<u32>()?, y.parse::<u32>()?),
            None => (base.parse::<u32>()?, base.parse::<u32>()?),
        };
        if a < lo || b > hi || a > b {
            bail!("field {index} value {base} out of range {lo}-{hi}");
        }
    }
    Ok(())
}

fn field_matches(field: &str, value: u32) -> bool {
    for part in field.split(',') {
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => (b, s.parse::<u32>().unwrap_or(1)),
            None => (part, 1),
        };
        let (lo, hi) = if base == "*" {
            (0u32, if value == 0 { 59 } else { 59 })
        } else if let Some((x, y)) = base.split_once('-') {
            (x.parse().unwrap_or(0), y.parse().unwrap_or(59))
        } else {
            let n: u32 = base.parse().unwrap_or(u32::MAX);
            (n, n)
        };
        if value >= lo && value <= hi && (value - lo) % step == 0 {
            return true;
        }
    }
    false
}

/// Is this schedule due at time `now`, given the last run time?
pub fn is_due(schedule: &Schedule, now: &DateTime<Local>, last_run: Option<&DateTime<Local>>) -> bool {
    match schedule {
        Schedule::Interval { minutes } => match last_run {
            None => true,
            Some(last) => now.signed_duration_since(*last).num_minutes() >= *minutes,
        },
        Schedule::Cron(expr) => {
            let f: Vec<&str> = expr.split_whitespace().collect();
            if f.len() != 5 { return false; }
            // avoid running twice in the same minute
            if let Some(last) = last_run {
                if last.format("%Y-%m-%d %H:%M").to_string()
                    == now.format("%Y-%m-%d %H:%M").to_string()
                {
                    return false;
                }
            }
            let minute = now.minute();
            let hour = now.hour();
            let day = now.day();
            let month = now.month();
            let weekday = now.weekday().num_days_from_sunday();
            field_matches(f[0], minute)
                && field_matches(f[1], hour)
                && field_matches(f[2], day)
                && field_matches(f[3], month)
                && (field_matches(f[4], weekday) || field_matches(f[4], if weekday == 0 { 7 } else { weekday }))
        }
    }
}

/// One job definition (persisted in the `jobs` table).
#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub name: String,
    pub schedule: String,
    pub command: String,
    pub enabled: bool,
    pub last_run: Option<i64>,
}

/// Run record for statistics.
#[derive(Debug, Clone)]
pub struct JobRun {
    pub id: i64,
    pub job_id: i64,
    pub started: i64,
    pub finished: i64,
    pub exit_code: i32,
    pub output_tail: String,
}

/// Parse a timestamp from SQLite.
pub fn ts(v: i64) -> DateTime<Local> {
    DateTime::from_timestamp(v, 0)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap())
        .with_timezone(&Local)
}

/// Run one shell command, capture tail, return (exit_code, output_tail).
/// Commands ending in `.py` run through Python: the embedded interpreter on
/// the Linux hub (full CPython via PyO3), the system Python on Windows seats
/// (no libpython there — same `fleetd` hook module, different source).
pub fn run_command(command: &str) -> Result<(i32, String)> {
    let finish = |code: i32, text: &str| -> Result<(i32, String)> {
        let tail: String = text.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
        Ok((code, tail))
    };

    if command.trim_end().ends_with(".py") {
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        {
            // CLI `job run` bypasses serve(), so the interpreter may not be
            // booted in this process yet; init is idempotent (OnceLock).
            struct NoopApi;
            impl crate::pyhost::FleetdApi for NoopApi {
                fn log(&self, _m: &str) {}
                fn alert(&self, _m: &str) {}
                fn seats(&self) -> Vec<(String, String)> { Vec::new() }
                fn jobs(&self) -> Vec<(String, String, bool)> { Vec::new() }
                fn run_job(&self, _name: &str) -> anyhow::Result<i32> { Ok(0) }
            }
            let init_result = crate::pyhost::init(Box::new(NoopApi));
            if let Err(e) = &init_result {
                eprintln!("pyhost init failed: {e:#}");
            }
            let source = std::fs::read_to_string(command)
                .with_context(|| format!("read python job {command}"))?;
            match crate::pyhost::run_source(&source) {
                Ok(stdout) => return finish(0, &stdout),
                Err(e) => {
                    // Embedded path failed (init or exec): fall back to the
                    // system python subprocess as last resort.
                    #[cfg(target_os = "windows")]
                    {
                        use std::process::Command;
                        if let Ok(output) = Command::new("python").arg(command).output() {
                            let mut text = String::new();
                            text.push_str(&String::from_utf8_lossy(&output.stdout));
                            text.push_str(&String::from_utf8_lossy(&output.stderr));
                            let code = output.status.code().unwrap_or(-1);
                            return finish(code, &text);
                        }
                    }
                    return finish(1, &format!("{e:#}"));
                }
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = command;
            anyhow::bail!("python jobs not supported on this platform")
        }
    } else {
        use std::process::Command;
        let output = Command::new("bash").arg("-c").arg(command).output()
            .with_context(|| "spawn job command")?;
        let mut text = String::new();
        text.push_str(&String::from_utf8_lossy(&output.stdout));
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let code = output.status.code().unwrap_or(-1);
        finish(code, &text)
    }
}

/// Long-running scheduler loop. Checks due jobs every 30 seconds.
/// Uses ledger functions passed in as closures to stay decoupled.
pub fn serve<F>(mut check_and_run: F) -> !
where
    F: FnMut() -> Result<usize>,
{
    // Boot the embedded interpreter so `.py` jobs have `import fleetd`.
    // (The real LedgerApi adapter lands with the MCP exposure step.)
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        struct NoopApi;
        impl crate::pyhost::FleetdApi for NoopApi {
            fn log(&self, _m: &str) {}
            fn alert(&self, _m: &str) {}
            fn seats(&self) -> Vec<(String, String)> { Vec::new() }
            fn jobs(&self) -> Vec<(String, String, bool)> { Vec::new() }
            fn run_job(&self, _name: &str) -> anyhow::Result<i32> { Ok(0) }
        }
        let _ = crate::pyhost::init(Box::new(NoopApi));
    }
    loop {
        match check_and_run() {
            Ok(n) if n > 0 => eprintln!("scheduler: ran {n} job(s)"),
            Ok(_) => {}
            Err(e) => eprintln!("scheduler error: {e:#}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn parses_cron_and_interval() {
        assert!(matches!(parse_schedule("*/5 * * * *").unwrap(), Schedule::Cron(_)));
        assert_eq!(parse_schedule("every 15m").unwrap(), Schedule::Interval { minutes: 15 });
        assert_eq!(parse_schedule("@every 2h").unwrap(), Schedule::Interval { minutes: 120 });
        assert!(parse_schedule("bogus").is_err());
        assert!(parse_schedule("* * * *").is_err());
    }

    #[test]
    fn cron_star_every_minute() {
        let now = Local.with_ymd_and_hms(2026, 11, 26, 10, 35, 0).unwrap();
        let s = parse_schedule("* * * * *").unwrap();
        assert!(is_due(&s, &now, None));
        // same minute → not twice
        assert!(!is_due(&s, &now, Some(&now)));
    }

    #[test]
    fn cron_step_every_5_minutes() {
        let s = parse_schedule("*/5 * * * *").unwrap();
        for (m, due) in [(0, true), (3, false), (5, true), (12, false), (55, true)] {
            let now = Local.with_ymd_and_hms(2026, 11, 26, 10, m, 0).unwrap();
            assert_eq!(is_due(&s, &now, None), due, "minute {m}");
        }
    }

    #[test]
    fn cron_fixed_daily() {
        let s = parse_schedule("30 3 * * *").unwrap();
        let yes = Local.with_ymd_and_hms(2026, 11, 26, 3, 30, 0).unwrap();
        let no = Local.with_ymd_and_hms(2026, 11, 26, 4, 30, 0).unwrap();
        assert!(is_due(&s, &yes, None));
        assert!(!is_due(&s, &no, None));
    }

    #[test]
    fn interval_respects_last_run() {
        let s = parse_schedule("every 5m").unwrap();
        let now = Local.with_ymd_and_hms(2026, 11, 26, 10, 35, 0).unwrap();
        let last = Local.with_ymd_and_hms(2026, 11, 26, 10, 31, 0).unwrap();
        assert!(!is_due(&s, &now, Some(&last))); // 4 min
        let older = Local.with_ymd_and_hms(2026, 11, 26, 10, 30, 0).unwrap();
        assert!(is_due(&s, &now, Some(&older))); // 5 min
    }

    #[test]
    fn debug_step5_at_minute5() {
        let s = parse_schedule("*/5 * * * *").unwrap();
        let now = Local.with_ymd_and_hms(2026, 11, 26, 16, 5, 0).unwrap();
        assert!(is_due(&s, &now, None), "minute 5 must be due for */5");
        let now2 = Local.with_ymd_and_hms(2026, 11, 26, 16, 6, 0).unwrap();
        assert!(!is_due(&s, &now2, None), "minute 6 not due");
    }

    #[test]
    fn field_validation() {
        assert!(parse_schedule("60 * * * *").is_err());
        assert!(parse_schedule("* 24 * * *").is_err());
        assert!(parse_schedule("*/0 * * * *").is_err());
        assert!(parse_schedule("1-5 * * * *").is_ok());
    }
}

#[cfg(test)]
mod pyjob_tests {
    #[test]
    fn py_job_runs_embedded() {
        // The scheduler loop calls pyhost::init; tests must too (OnceLock is
        // per-process and the loop isn't running under cargo test).
        struct NoopApi;
        impl crate::pyhost::FleetdApi for NoopApi {
            fn log(&self, _m: &str) {}
            fn alert(&self, _m: &str) {}
            fn seats(&self) -> Vec<(String, String)> { Vec::new() }
            fn jobs(&self) -> Vec<(String, String, bool)> { Vec::new() }
            fn run_job(&self, _name: &str) -> anyhow::Result<i32> { Ok(0) }
        }
        let _ = crate::pyhost::init(Box::new(NoopApi));
        let path = std::env::temp_dir().join("fleetd-e2e-pyjob.py");
        std::fs::write(&path, "import fleetd\nprint('e2e-ok', hasattr(fleetd,'seats'))").unwrap();
        let (code, out) = super::run_command(path.to_str().unwrap()).unwrap();
        assert_eq!(code, 0, "py job failed: {out}");
        assert!(out.contains("e2e-ok True"), "stdout: {out}");
    }
}
