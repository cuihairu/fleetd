// `fleetd fleet <flagship-url>` — remote fleet overview on any machine (ANSI
// table, user-approved option A over a full TUI framework). Pulls the flagship's
// /v1/fleet/summary and renders a compact seat/task table. `--follow` polls.
//
// Data contract: GET {flagship}/v1/fleet/summary -> {"seats":[{seat,status,
// current_tag,updated_at}],"recent_dispatches":[{id,seat,tag,fired_at,
// finished_at,outcome}],"open_count":N}

use std::io::Write as _;
use std::process::Command;

/// Minimal HTTP GET over curl (present on every fleet machine; avoids
/// hand-rolling HTTP client code in fleetd).
fn http_get(url: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-sf", "--max-time", "10", url])
        .output()
        .map_err(|e| format!("curl spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!("curl exit {:?} for {url}", out.status.code()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// ANSI dim grey divider.
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";

/// Parse the summary JSON minimally (serde_json Value — no typed struct here
/// so the TUI face stays resilient to additive schema changes).
fn render(json: &str, now: i64) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("summary JSON: {e}"))?;
    let mut out = String::new();
    out.push_str(&format!(
        "{BOLD}FLEET OVERVIEW{RESET}  open={}\n",
        v["open_count"].as_i64().unwrap_or(0),
    ));
    out.push_str(&format!(
        "{DIM}{}{RESET}\n",
        "-".repeat(66)
    ));
    out.push_str(&format!(
        "{BOLD}{:<14} {:<22} {:<10} {:>8}{RESET}\n",
        "SEAT", "TASK", "STATE", "AGE"
    ));

    // Merge view: seats first (their live status), then dispatch rows.
    if let Some(seats) = v["seats"].as_array() {
        for s in seats {
            let seat = s["seat"].as_str().unwrap_or("?");
            let status = s["status"].as_str().unwrap_or("?");
            let tag = s["current_tag"].as_str().unwrap_or("-");
            let (dot, color) = match status {
                "working" | "running" => ("●", GREEN),
                "disabled" => ("⊘", RED),
                _ => ("○", CYAN),
            };
            out.push_str(&format!(
                "{color}{dot}{RESET} {color}{:<13}{RESET} {:<22} {:<10} {:>8}\n",
                seat, tag, status, "-"
            ));
        }
    }
    if let Some(rows) = v["recent_dispatches"].as_array() {
        for r in rows.iter().take(12) {
            let seat = r["seat"].as_str().unwrap_or("?");
            let tag = r["tag"].as_str().unwrap_or("?");
            let fired = r["fired_at"].as_i64().unwrap_or(0);
            let finished = r["finished_at"].as_i64();
            let outcome = r["outcome"].as_str().unwrap_or("");
            let (state, color) = match (finished, outcome) {
                (Some(_), "completed") | (Some(_), "success") => ("✓ done", GREEN),
                (Some(_), "stalled") => ("✗ stalled", RED),
                (Some(_), "partial") => ("◐ partial", YELLOW),
                (Some(_), _) => ("· ended", DIM),
                (None, _) => ("● working", GREEN),
            };
            let age = fmt_age(now - fired);
            out.push_str(&format!(
                "{color}{:<15}{RESET} {:<22} {color}{:<10}{RESET} {:>8}\n",
                trunc(seat, 15),
                trunc(tag, 22),
                state,
                age
            ));
        }
    }
    Ok(out)
}

/// Truncate to n display columns (chars; CJK widths ignored — fleet ids are ASCII).
fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn fmt_age(secs: i64) -> String {
    if secs < 0 {
        return "-".into();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let m = secs / 60;
    if m < 60 {
        return format!("{m}m");
    }
    format!("{}h{}m", m / 60, m % 60)
}

/// One-shot render, or follow loop (`--follow` clears and redraws every 2s).
pub fn run(flagship: &str, follow: bool) -> Result<(), String> {
    let url = format!("{}/v1/fleet/summary", flagship.trim_end_matches('/'));
    loop {
        let rendered = match http_get(&url) {
            Ok(j) => render(&j, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64)?,
            Err(e) => format!("{RED}flagship unreachable: {e}{RESET}"),
        };
        if follow {
            // Clear screen + home cursor (ANSI; works in every fleet terminal).
            print!("\x1b[2J\x1b[H{rendered}");
            std::io::stdout().flush().ok();
            std::thread::sleep(std::time::Duration::from_secs(2));
        } else {
            println!("{rendered}");
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"seats":[{"seat":"kode","status":"working","current_tag":"K-1","updated_at":1}],
        "recent_dispatches":[{"id":1,"seat":"kode","tag":"K-1","fired_at":1795715000,
        "finished_at":null,"outcome":null}],"open_count":1}"#;

    #[test]
    fn renders_seat_and_dispatch_rows() {
        let out = render(SAMPLE, 1795715000 + 120).unwrap();
        assert!(out.contains("FLEET OVERVIEW"));
        assert!(out.contains("kode"));
        assert!(out.contains("K-1"));
        assert!(out.contains("working"));
        assert!(out.contains("2m")); // dispatch age
    }

    #[test]
    fn bad_json_is_an_error_not_a_panic() {
        assert!(render("{broken", 0).is_err());
    }
}
