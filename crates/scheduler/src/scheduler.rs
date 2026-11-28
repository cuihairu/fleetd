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

pub use fleetd_core::schedule::{is_due, parse_schedule, ts, Schedule};

pub fn run_command(command: &str) -> Result<(i32, String)> {
    let finish = |code: i32, text: &str| -> Result<(i32, String)> {
        let tail: String = text
            .chars()
            .rev()
            .take(2000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
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
                fn seats(&self) -> Vec<(String, String)> {
                    Vec::new()
                }
                fn jobs(&self) -> Vec<(String, String, bool)> {
                    Vec::new()
                }
                fn run_job(&self, _name: &str) -> anyhow::Result<i32> {
                    Ok(0)
                }
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
        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .output()
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
            fn seats(&self) -> Vec<(String, String)> {
                Vec::new()
            }
            fn jobs(&self) -> Vec<(String, String, bool)> {
                Vec::new()
            }
            fn run_job(&self, _name: &str) -> anyhow::Result<i32> {
                Ok(0)
            }
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
        assert!(matches!(
            parse_schedule("*/5 * * * *").unwrap(),
            Schedule::Cron(_)
        ));
        assert_eq!(
            parse_schedule("every 15m").unwrap(),
            Schedule::Interval { minutes: 15 }
        );
        assert_eq!(
            parse_schedule("@every 2h").unwrap(),
            Schedule::Interval { minutes: 120 }
        );
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
            fn seats(&self) -> Vec<(String, String)> {
                Vec::new()
            }
            fn jobs(&self) -> Vec<(String, String, bool)> {
                Vec::new()
            }
            fn run_job(&self, _name: &str) -> anyhow::Result<i32> {
                Ok(0)
            }
        }
        let _ = crate::pyhost::init(Box::new(NoopApi));
        let path = std::env::temp_dir().join("fleetd-e2e-pyjob.py");
        std::fs::write(
            &path,
            "import fleetd\nprint('e2e-ok', hasattr(fleetd,'seats'))",
        )
        .unwrap();
        let (code, out) = super::run_command(path.to_str().unwrap()).unwrap();
        assert_eq!(code, 0, "py job failed: {out}");
        assert!(out.contains("e2e-ok True"), "stdout: {out}");
    }
}
