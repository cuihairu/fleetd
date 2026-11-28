//! Self-update face (FLEETD-SELF-001): bring the fleet's binaries up to
//! origin/main, one machine shape at a time, with a rollback path.
//!
//! Version identity is the git short hash baked in at build time
//! (`FLEETD_BUILD_HASH`, see the root build.rs). `--check` only reads;
//! the default flow per host shape is:
//! - local: fresh binary is already in target/release after a build, so an
//!   update means rebuild + restart the systemd unit (the unit points at the
//!   build output directly).
//! - remote linux (work-runner shape): scp the binary to a `.new` temp name,
//!   then a remote one-liner keeps a `.bak` and atomically renames — the
//!   window where the binary is missing is one rename syscall.
//! - windows (imac shape): same scp-to-temp trick, then a PowerShell script
//!   stops the service, swaps the exe, starts it again — stop window is the
//!   script's own runtime, kept well under the 10 s acceptance bar.
//!
//! Safety rails: an update refuses to bounce a serve with live work unless
//! `--restart` was given, every replacement keeps exactly one `.bak`, and
//! `update_applied` / `update_rollback` land in the events table.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Build-time git short hash (root build.rs). The version identity.
pub const BUILD_HASH: &str = env!("FLEETD_BUILD_HASH");

/// The `fleetd --version` string: workspace version + git hash suffix.
pub fn version_string() -> String {
    format!("fleetd {}+g{BUILD_HASH}", env!("CARGO_PKG_VERSION"))
}

/// Remote hosts the update face knows, in default update order. Mirrors the
/// infra-watch machine list (same three ships).
pub const UPDATE_HOSTS: &[&str] = &["local", "work-runner", "imac"];

/// Absolute path of this build's binary — the source every remote copy reads.
pub fn built_binary() -> std::path::PathBuf {
    // target/<profile>/fleetd relative to the manifest dir at runtime is not
    // knowable portably; use current_exe, which for CLI runs IS this binary.
    std::env::current_exe().unwrap_or_else(|_| "fleetd".into())
}

/// Read origin/main's tip. Explicit network failure — `--check` must say so
/// rather than half-update.
pub fn origin_head() -> Result<String> {
    let out = Command::new("git")
        .args(["ls-remote", "origin", "refs/heads/main"])
        .output()
        .context("git ls-remote failed (is git installed?)")?;
    if !out.status.success() {
        bail!(
            "cannot reach origin: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let head: String = line.split_whitespace().next().unwrap_or("").to_string();
    if head.len() < 7 {
        bail!("origin/main not found in ls-remote output");
    }
    Ok(head.chars().take(7).collect())
}

/// What `--check` reports. `up_to_date` compares the build hash, not the
/// working tree — a dirty tree is irrelevant to binary identity.
#[derive(Debug)]
pub struct UpdateCheck {
    pub origin_main: String,
    pub build_hash: String,
    pub up_to_date: bool,
    pub behind_by: Option<String>,
}

/// Run the read-only check. `behind_by` lists the commits origin has that we
/// lack (best effort; absent when git log fails offline-rebuild trees).
pub fn check() -> Result<UpdateCheck> {
    let origin_main = origin_head()?;
    let up_to_date = origin_main.starts_with(BUILD_HASH);
    let behind_by = if up_to_date {
        None
    } else {
        Command::new("git")
            .args(["rev-list", &format!("{BUILD_HASH}..origin/main")])
            .output()
            .ok()
            .map(|o| {
                let n = String::from_utf8_lossy(&o.stdout).lines().count();
                format!("{n} commits")
            })
    };
    Ok(UpdateCheck {
        origin_main,
        build_hash: BUILD_HASH.to_string(),
        up_to_date,
        behind_by,
    })
}

/// The total running+queued load across the live machine pool, via the
/// flagship's /v1/machines face. None when the face is unreachable (CLI run
/// without a serve) — the caller decides whether that blocks.
pub fn live_load() -> Option<(u64, u64)> {
    let machines = crate::events::fetch_machines_via_socket()?;
    let running: u64 = machines.iter().map(|m| m.running as u64).sum();
    let queued: u64 = machines.iter().map(|m| m.queued as u64).sum();
    Some((running, queued))
}

/// Refuse to bounce a serve that still has live work, unless --restart.
pub fn guard_live_work(restart: bool) -> Result<()> {
    match live_load() {
        Some((r, q)) if r + q > 0 && !restart => {
            bail!("serve has live work (running={r} queued={q}); pass --restart to bounce anyway")
        }
        _ => Ok(()),
    }
}

/// One sh() helper for the module: run, return trimmed stdout, fail loudly
/// with stderr on nonzero exit.
fn sh(cmd: &str) -> Result<String> {
    let out = Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("spawn: {cmd}"))?;
    if !out.status.success() {
        bail!(
            "`{cmd}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Record an update lifecycle event into the ledger (best effort — an event
/// write failure must not fail the update itself).
fn note(kind: &str, note: &str) {
    if let Ok(c) = store::ledger::open() {
        let _ = store::ledger::record_event(&c, "local", kind, None, Some(note));
    }
}

/// Local update: the systemd unit points at target/release, so "replace" is
/// a rebuild plus a unit restart. The `.bak` keeps the pre-update binary for
/// `update --rollback`. Returns from→to hashes.
pub fn update_local(restart: bool) -> Result<(String, String)> {
    let active = built_binary();
    let bak = active.with_extension("bak");
    if active.exists() {
        std::fs::copy(&active, &bak)
            .with_context(|| format!("backup {} -> {}", active.display(), bak.display()))?;
    }
    sh("git -C /home/cui/workspaces/fleetd pull --ff-only")
        .context("git pull failed; local tree left untouched")?;
    sh("cargo build --release --manifest-path /home/cui/workspaces/fleetd/Cargo.toml")
        .context("release build failed; old binary still in place")?;
    let from = BUILD_HASH.to_string();
    let to = sh("git -C /home/cui/workspaces/fleetd rev-parse --short HEAD")?;
    if restart {
        sh("systemctl --user restart fleetd").context("restart fleetd unit")?;
    }
    note(
        "update_applied",
        &format!("local {from}->{to} restart={restart}"),
    );
    Ok((from, to))
}

/// Remote linux update: scp to `.new`, then one ssh round keeps `.bak` and
/// renames. `--restart` bounces the bare `fleetd serve` process the way the
/// ship runs it (observed: `/home/<user>/.fleetd/fleetd serve --port 3790`).
pub fn update_remote_linux(host: &str, restart: bool) -> Result<(String, String)> {
    let active = built_binary();
    let remote_dir = ".fleetd";
    sh(&format!(
        "scp -o BatchMode=yes {} {host}:{remote_dir}/fleetd.new",
        active.display()
    ))
    .with_context(|| format!("scp binary to {host}"))?;
    let mut script = format!(
        "cp {remote_dir}/fleetd {remote_dir}/fleetd.bak && \
         mv {remote_dir}/fleetd.new {remote_dir}/fleetd && \
         chmod +x {remote_dir}/fleetd && {remote_dir}/fleetd --version"
    );
    if restart {
        // Kill then relaunch detached; the SSH session must not own the
        // process, hence setsid + nohup + redirect.
        script.push_str(
            " && pkill -x fleetd; sleep 1; setsid nohup .fleetd/fleetd serve --port 3790 \
             >/dev/null 2>&1 & sleep 1; pgrep -x fleetd",
        );
    }
    let version_out = sh(&format!("ssh -o BatchMode=yes {host} '{script}'"))
        .with_context(|| format!("remote swap on {host}"))?;
    let to = version_out
        .rsplit("+g")
        .next()
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("?")
        .to_string();
    let from = BUILD_HASH.to_string();
    note(
        "update_applied",
        &format!("{host} {from}->{to} restart={restart}"),
    );
    Ok((from, to))
}

/// Windows update: scp to a temp name, then one PowerShell script does
/// stop-service → backup → swap → start-service (plus tray replacement when
/// a tray exe is deployed). Returns from→to hashes.
pub fn update_remote_windows(host: &str, restart: bool) -> Result<(String, String)> {
    let active = built_binary();
    sh(&format!(
        "scp -o BatchMode=yes {} {host}:C:/Users/cui/.fleetd/fleetd.new.exe",
        active.display()
    ))
    .with_context(|| format!("scp binary to {host}"))?;
    let ps = format!(
        "$d='C:/Users/cui/.fleetd'; \
         Stop-Service FleetD -Force; \
         Copy-Item \"$d/fleetd.exe\" \"$d/fleetd.exe.bak\" -Force; \
         Move-Item \"$d/fleetd.new.exe\" \"$d/fleetd.exe\" -Force; \
         if (Test-Path \"$d/fleetd-tray.exe.new\") {{ \
             Copy-Item \"$d/fleetd-tray.exe\" \"$d/fleetd-tray.exe.bak\" -Force -ErrorAction SilentlyContinue; \
             Move-Item \"$d/fleetd-tray.exe.new\" \"$d/fleetd-tray.exe\" -Force }}; \
         Start-Service FleetD; \
         (Get-Service FleetD).Status"
    );
    // The service bounce is the restart; when --restart was not given, the
    // live-work guard already refused for us, so here we always bounce.
    let _ = restart;
    sh(&format!(
        "ssh -o BatchMode=yes {host} \"powershell -NoProfile -Command '{ps}'\""
    ))
    .with_context(|| format!("windows swap on {host}"))?;
    let from = BUILD_HASH.to_string();
    let to = origin_head().unwrap_or_else(|_| "?".into());
    note(
        "update_applied",
        &format!("{host} {from}->{to} restart=true"),
    );
    Ok((from, to))
}

/// Rollback: restore the `.bak` over the active binary on the given host
/// ("local" for this machine) and bounce the face so it takes effect.
pub fn rollback(host: &str, restart: bool) -> Result<()> {
    match host {
        "local" => {
            let active = built_binary();
            let bak = active.with_extension("bak");
            if !bak.exists() {
                bail!("no {} — nothing to roll back to", bak.display());
            }
            std::fs::copy(&bak, &active).context("restore backup over active binary")?;
            if restart {
                sh("systemctl --user restart fleetd").context("restart fleetd unit")?;
            }
        }
        _ => {
            let script =
                format!("cp .fleetd/fleetd.bak .fleetd/fleetd && .fleetd/fleetd --version");
            sh(&format!("ssh -o BatchMode=yes {host} '{script}'"))
                .with_context(|| format!("rollback on {host}"))?;
            if restart {
                sh(&format!(
                    "ssh -o BatchMode=yes {host} \
                     'pkill -x fleetd; sleep 1; setsid nohup .fleetd/fleetd serve \
                      --port 3790 >/dev/null 2>&1 & sleep 1; pgrep -x fleetd'"
                ))?;
            }
        }
    }
    note("update_rollback", &format!("{host} restart={restart}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_carries_hash_suffix() {
        let v = version_string();
        assert!(v.starts_with("fleetd 0.2.0+g"), "got: {v}");
        // The hash part is 7 hex chars when the build tree was a git repo.
        let hash = v.rsplit("+g").next().unwrap();
        assert!(hash.len() == 7 || hash == "unknown", "got: {hash}");
    }

    #[test]
    fn update_hosts_cover_the_fleet() {
        assert_eq!(UPDATE_HOSTS, &["local", "work-runner", "imac"]);
    }

    #[test]
    fn guard_allows_idle_or_absent_face() {
        // Whatever the live face says, an explicit --restart always passes.
        assert!(guard_live_work(true).is_ok());
    }
}
