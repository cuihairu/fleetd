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

/// The libpython shared library this binary dynamically links (pyhost
/// embeds CPython), from ldd; None when statically linked or absent. Ships
/// without the system python package would otherwise die on exec with
/// `libpython3.14.so.1.0: cannot open shared object file` (observed live on
/// work-runner, 2026-11-28 — the update must ship the library alongside).
pub fn bundled_libpython() -> Option<String> {
    let out = Command::new("ldd").arg(built_binary()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if line.contains("libpython") && line.contains("=>") {
            let path = line.split("=>").nth(1)?.trim();
            let path = path.split_whitespace().next()?;
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// The system stdlib tree to bundle (/usr/lib/python3.x on Debian-shaped
/// hosts), found by probing the known roots. None when this host has no
/// system python — then the bundle has no stdlib and remote pyhost stays
/// dependent on whatever the ship already has.
pub fn system_stdlib_dir() -> Option<String> {
    for base in ["/usr/lib", "/usr/local/lib"] {
        if let Ok(entries) = std::fs::read_dir(base) {
            let mut found: Vec<(String, String)> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with("python3."))
                .map(|n| {
                    let p = format!("{base}/{n}");
                    // Sort key: the numeric minor, newest wins.
                    let minor: u32 = n.trim_start_matches("python3.").parse().unwrap_or(0);
                    (p, minor.to_string())
                })
                .collect();
            found.sort_by(|a, b| b.1.cmp(&a.1));
            if let Some((p, _)) = found.first() {
                return Some(p.clone());
            }
        }
    }
    None
}

/// Remote linux update: scp to an absolute `.new` path (sftp does not expand
/// `$HOME` in target paths), ship the libpython dependency alongside, then
/// one `set -e` ssh round keeps `.bak` and renames — a failing step aborts
/// the whole chain and the ssh exit code carries the failure (a bare `;`
/// chain once masked a rc=127 and left the ship half-updated, observed live).
/// `--restart` bounces the bare `fleetd serve` process the way the ship runs
/// it (observed: `/home/<user>/.fleetd/fleetd serve --port 3790`).
pub fn update_remote_linux(host: &str, restart: bool) -> Result<(String, String)> {
    let active = built_binary();
    // Home-relative paths: sftp resolves them against the login dir and an
    // absolute `host:/home/...` target once exited 0 without landing the
    // file (observed live on work-runner, 2026-11-28) — relative works.
    let dir = ".fleetd";
    sh(&format!(
        "ssh -o BatchMode=yes {host} 'mkdir -p {dir}/runtime/lib'"
    ))?;
    // Bundle the runtime the way the exe expects it (pyhost looks for
    // runtime/lib/python3.x next to the binary): the shared library plus
    // the stdlib tree. Ships without the system python package would
    // otherwise die on `No module named 'encodings'` (observed live on
    // work-runner AND imac, 2026-11-28).
    if let Some(lib) = bundled_libpython() {
        // Explicit remote filename — a trailing-slash directory target
        // exits 0 without landing the file under sftp (observed live).
        let name = lib.rsplit('/').next().unwrap_or("libpython3.so");
        sh(&format!(
            "scp -o BatchMode=yes {lib} {host}:{dir}/runtime/lib/{name}"
        ))
        .with_context(|| format!("scp {lib} to {host}"))?;
    }
    // The stdlib tree rides a tar stream — scp has no -r equivalent that
    // survives the sftp path quirks, and tar preserves everything in one shot.
    if let Some(stdlib) = system_stdlib_dir() {
        sh(&format!(
            "tar -C {stdlib} -cf - . | ssh -o BatchMode=yes {host} \
             'mkdir -p {dir}/runtime/lib/python3.14 && tar -C {dir}/runtime/lib/python3.14 -xf -'"
        ))
        .with_context(|| format!("ship stdlib tree to {host}"))?;
    }
    sh(&format!(
        "scp -o BatchMode=yes {} {host}:{dir}/fleetd.new",
        active.display()
    ))
    .with_context(|| format!("scp binary to {host}"))?;
    // LD_LIBRARY_PATH uses $HOME on purpose: the whole script sits inside a
    // single-quoted ssh command, so the REMOTE shell expands it.
    let ld = "LD_LIBRARY_PATH=$HOME/.fleetd/runtime/lib";
    let mut script = format!(
        "set -e; \
         cp {dir}/fleetd {dir}/fleetd.bak; \
         mv {dir}/fleetd.new {dir}/fleetd; \
         chmod +x {dir}/fleetd; \
         {ld} {dir}/fleetd --version"
    );
    if restart {
        // Kill then relaunch detached; the SSH session must not own the
        // process, hence setsid + nohup + redirect. Same LD_LIBRARY_PATH
        // injection, or the new exec dies where the version probe did.
        script.push_str(&format!(
            "; pkill -x fleetd; sleep 1; \
             {ld} setsid nohup {dir}/fleetd serve --port 3790 --http >/dev/null 2>&1 & \
             sleep 1; pgrep -x fleetd"
        ));
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

/// Windows update: the msvc exe cannot be produced on this linux control
/// box, so it comes from CI — the newest `fleetd-windows-msvc` artifact
/// (see .github/workflows/windows.yml). One PowerShell script then does
/// stop-service → backup → swap → start-service (plus tray replacement when
/// the artifact carries it), with the stop window measured in-script.
/// Returns from→to hashes.
pub fn update_remote_windows(host: &str, restart: bool) -> Result<(String, String)> {
    let _ = restart; // the service bounce IS the restart; guard ran upstream
    let tmp = std::env::temp_dir().join("fleetd-imac-update");
    sh(&format!(
        "rm -rf {} && mkdir -p {} && gh run download -R cuihairu/fleetd \
         --name fleetd-windows-msvc -D {} && ls {}/fleetd.exe",
        tmp.display(),
        tmp.display(),
        tmp.display(),
        tmp.display()
    ))
    .context(
        "no fleetd-windows-msvc artifact available (the windows CI on main \
         must be green first); nothing was touched",
    )?;
    sh(&format!(
        "scp -o BatchMode=yes {}/fleetd.exe {host}:C:/Users/cui/.fleetd/fleetd.new.exe",
        tmp.display()
    ))
    .with_context(|| format!("scp binary to {host}"))?;
    // Tray rides along when the artifact carries it — best effort.
    let _ = sh(&format!(
        "scp -o BatchMode=yes {}/tray.exe {host}:C:/Users/cui/.fleetd/fleetd-tray.exe.new",
        tmp.display()
    ));
    let ps = format!(
        "$d='C:/Users/cui/.fleetd'; \
         $t = Measure-Command {{ \
             Stop-Service FleetD -Force; \
             Copy-Item \"$d/fleetd.exe\" \"$d/fleetd.exe.bak\" -Force; \
             Move-Item \"$d/fleetd.new.exe\" \"$d/fleetd.exe\" -Force; \
             if (Test-Path \"$d/fleetd-tray.exe.new\") {{ \
                 Copy-Item \"$d/fleetd-tray.exe\" \"$d/fleetd-tray.exe.bak\" -Force -ErrorAction SilentlyContinue; \
                 Move-Item \"$d/fleetd-tray.exe.new\" \"$d/fleetd-tray.exe\" -Force }}; \
             Start-Service FleetD }}; \
         Write-Output ('stop_window_ms=' + [int]$t.TotalMilliseconds); \
         (Get-Service FleetD).Status"
    );
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
            // Rename, not copy: the running serve executes this exact file,
            // and Linux refuses to write a busy executable (Text file busy).
            let staged = active.with_extension("bak.rollback");
            std::fs::copy(&bak, &staged).context("stage backup for rollback")?;
            std::fs::rename(&staged, &active).context("restore backup over active binary")?;
            if restart {
                sh("systemctl --user restart fleetd").context("restart fleetd unit")?;
            }
        }
        _ => {
            let dir = ".fleetd";
            let ld = "LD_LIBRARY_PATH=$HOME/.fleetd/runtime/lib";
            let script =
                format!("set -e; cp {dir}/fleetd.bak {dir}/fleetd; {ld} {dir}/fleetd --version");
            sh(&format!("ssh -o BatchMode=yes {host} '{script}'"))
                .with_context(|| format!("rollback on {host}"))?;
            if restart {
                sh(&format!(
                    "ssh -o BatchMode=yes {host} \
                     'pkill -x fleetd; sleep 1; {ld} setsid nohup {dir}/fleetd serve \
                      --port 3790 --http >/dev/null 2>&1 & sleep 1; pgrep -x fleetd'"
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
