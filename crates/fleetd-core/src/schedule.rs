//! Schedule parsing/matching primitives (pure logic, no IO).
//! 2026-11-27 下沉自 server::scheduler——fleetd-mcp 的 job 工具也需要
//! 解析/解释 schedule,纯面放 core 避免拖入 pyhost 依赖。

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
