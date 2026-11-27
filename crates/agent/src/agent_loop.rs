// `fleetd agent` — ship-side heartbeat loop (relay F2 client face).
//
// Ships behind NAT/unstable networks run this loop instead of relying on
// flagship→ship inbound SSH. Every cycle:
//   1. POST heartbeat {seat, head, dirty} to the flagship (outbound only)
//   2. if the response carries a pull-dispatch → write task file, launch
//      headless claude, and keep heartbeating (state lands on the flagship via
//      the next beat's head/dirty fields; the run artifacts stay on disk)
//   3. sleep and repeat; failures never kill the loop
//
// Also serves as the ALWAYS-ON channel for every ship (user ruling
// 2026-11-27: all ships run the agent so a ZT/network outage never orphans
// them) — SSH-capable ships simply keep SSH as the faster path.

use anyhow::{Context, Result};
use serde_json::json;
use std::io::Write;
use std::process::Command;
use std::time::Duration;

/// Persist agent identity so the loop survives restarts.
const AGENT_CONF: &str = ".fleetd/agent.toml";

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AgentConfig {
    #[serde(alias = "hub")]
    pub flagship: String,
    pub token: String,
    pub seat: String,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
}

fn default_interval() -> u64 {
    30
}

pub fn load_or_init(flagship: Option<&str>, token: Option<&str>, seat: Option<&str>) -> Result<AgentConfig> {
    let home = store::ledger::home_dir();
    let conf_path = std::path::Path::new(&home).join(AGENT_CONF);
    load_from(&conf_path, flagship, token, seat)
}

/// Conf-path injected variant (tests use a tempdir; keeps HOME untouched).
pub fn load_from(
    conf_path: &std::path::Path,
    flagship: Option<&str>,
    token: Option<&str>,
    seat: Option<&str>,
) -> Result<AgentConfig> {
    let flagship_field: Option<String> = flagship.map(String::from);
    let saved: Option<AgentConfig> = std::fs::read_to_string(conf_path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok());
    let cfg = AgentConfig {
        flagship: flagship_field
            .or_else(|| saved.as_ref().map(|c| c.flagship.clone()))
            .context("flagship url required (first run): fleetd agent --flagship http://host:3790")?,
        token: token
            .map(String::from)
            .or_else(|| saved.as_ref().map(|c| c.token.clone()))
            .unwrap_or_default(),
        seat: seat
            .map(String::from)
            .or_else(|| saved.as_ref().map(|c| c.seat.clone()))
            .unwrap_or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_else(|| "ship".into())
            }),
        interval_secs: 30,
    };
    // Persist on first run so later beats don't need flags.
    if saved.is_none() {
        if let Some(dir) = conf_path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(
            conf_path,
            format!(
                "flagship = \"{}\"\ntoken = \"{}\"\nseat = \"{}\"\n",
                cfg.flagship, cfg.token, cfg.seat
            ),
        )
        .ok();
    }
    Ok(cfg)
}

fn git_head(workdir: &str) -> Option<String> {
    Command::new("git")
        .args(["-C", workdir, "rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn count_dirty(workdir: &str) -> i64 {
    Command::new("git")
        .args(["-C", workdir, "status", "--porcelain"])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .count()
                .try_into()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// One heartbeat: POST and return the dispatch (if any). Never panics.
fn beat(cfg: &AgentConfig, workdir: &str) -> Option<(String, String, String)> {
    let body = json!({
        "seat": cfg.seat,
        "head": git_head(workdir),
        "dirty": count_dirty(workdir),
    });
    let out = Command::new("curl")
        .args([
            "-sf",
            "--max-time",
            "15",
            "-X",
            "POST",
            &format!("{}/v1/agent/heartbeat", cfg.flagship.trim_end_matches('/')),
            "-H",
            &format!("Authorization: Bearer {}", cfg.token),
            "-H",
            "Content-Type: application/json",
            "-d",
            &body.to_string(),
        ])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let d = v.get("dispatch")?;
    let id = d.get("id")?.as_str()?.to_string();
    let task_text = d.get("task_text")?.as_str()?.to_string();
    let repo = d.get("repo")?.as_str()?.to_string();
    Some((id, task_text, repo))
}

/// The loop. Runs forever: beat → maybe execute → sleep. Callers never see
/// an Err from individual beats (they are logged and skipped).
pub fn run_loop(cfg: AgentConfig, workdir: &str, permission_mode: &str) -> Result<()> {
    let home = store::ledger::home_dir();
    let runs_dir = std::path::Path::new(&home).join(".fleetd/agent-runs");
    std::fs::create_dir_all(&runs_dir).ok();
    println!(
        "fleetd agent: seat={} flagship={} interval={}s workdir={}",
        cfg.seat, cfg.flagship, cfg.interval_secs, workdir
    );
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    loop {
        match beat(&cfg, workdir) {
            Some((id, task_text, repo)) => {
                let _ = writeln!(lock, "[{}] dispatch {}: {} chars", now(), id, task_text.len());
                let _ = lock.flush();
                // Materialize the task and launch headless claude (fire and
                // forget with setsid; the flagship sees progress via git head on
                // later beats).
                let dir = runs_dir.join(&id);
                let _ = std::fs::create_dir_all(&dir);
                let task_file = dir.join("task.md");
                std::fs::write(&task_file, &task_text).ok();
                let launch = format!(
                    "cd {workdir} && {{ setsid nohup claude --bg --permission-mode {pm} \"$(cat {tf})\" > {dir}/session-id 2> {dir}/stderr.log & }} && echo launched",
                    workdir = workdir,
                    pm = permission_mode,
                    tf = task_file.display(),
                    dir = dir.display(),
                );
                match Command::new("bash").args(["-c", &launch]).output() {
                    Ok(_) => {
                        let _ = writeln!(lock, "[{}] launched in {}", now(), workdir);
                    }
                    Err(e) => {
                        let _ = writeln!(lock, "[{}] launch failed: {e}", now());
                    }
                }
                let _ = lock.flush();
            }
            None => {
                let _ = writeln!(lock, "[{}] beat ok (no dispatch)", now());
                let _ = lock.flush();
            }
        }
        std::thread::sleep(Duration::from_secs(cfg.interval_secs));
    }
}

fn now() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_persists_on_first_run() {
        let dir = std::env::temp_dir().join(format!("fleetd-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join(".fleetd/agent.toml");
        let cfg = load_from(&conf, Some("http://flagship:3790"), Some("tok"), Some("ship-a")).unwrap();
        assert_eq!(cfg.seat, "ship-a");
        let saved = std::fs::read_to_string(&conf).unwrap();
        assert!(saved.contains("http://flagship:3790"));
        // Second load without flags reads the persisted config.
        let again = load_from(&conf, None, None, None).unwrap();
        assert_eq!(again.flagship, "http://flagship:3790");
    }
}
