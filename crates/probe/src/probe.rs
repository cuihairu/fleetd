//! Fact collection: probe hosts for installed tools and attached devices.
//!
//! Probes are read-only shell one-liners run locally or over ssh. Failures are
//! honest: a missing tool simply doesn't appear; an unreachable host yields an
//! offline marker. Nothing here installs anything.

use anyhow::Result;
use std::process::Command;

use store::resources::{upsert, DeviceState, ResourceKind};

/// Run a probe command locally or over ssh. Returns stdout (trimmed) or None.
fn probe(host: &str, remote_cmd: &str) -> Option<String> {
    let out = if host == "local" {
        Command::new("bash")
            .arg("-c")
            .arg(remote_cmd)
            .output()
            .ok()?
    } else {
        Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
            .arg(remote_cmd)
            .output()
            .ok()?
    };
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Known tool probes: (resource name, detection command, detail extractor).
/// Detail = first line of stdout, trimmed to 40 chars (version strings etc).
const TOOL_PROBES: &[(&str, &str)] = &[
    ("cargo", "cargo --version"),
    ("rustc", "rustc --version"),
    ("node", "node --version"),
    ("pnpm", "pnpm --version"),
    ("go", "go version"),
    ("python3", "python3 --version"),
    ("adb", "adb version | head -1"),
    ("dotnet", "dotnet --version"),
    ("docker", "docker --version"),
    ("claude", "claude --version"),
    ("git", "git --version"),
    ("gh", "gh --version"),
];

/// Probe one host for installed tools, upserting facts into the ledger.
pub fn probe_tools(c: &rusqlite::Connection, host: &str) -> Result<usize> {
    let mut found = 0;
    for (name, cmd) in TOOL_PROBES {
        if let Some(detail) = probe(host, cmd) {
            let detail: String = detail
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            upsert(
                c,
                ResourceKind::Tool,
                host,
                name,
                &detail,
                DeviceState::Online,
                "",
            )?;
            found += 1;
        }
    }
    Ok(found)
}

/// Probe adb devices on a host (local or remote). Each device becomes a
/// `device` resource: name = adb serial, detail = model/state line.
pub fn probe_adb_devices(c: &rusqlite::Connection, host: &str) -> Result<usize> {
    let mut found = 0;
    // adb devices -l output: serial device product:model:... device:...
    if let Some(out) = probe(host, "adb devices -l 2>/dev/null") {
        for line in out.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let serial = parts[0];
            let usb_state = parts[1]; // device | offline | unauthorized
            let model = parts
                .iter()
                .find(|p| p.starts_with("model:"))
                .map(|p| p.trim_start_matches("model:"))
                .unwrap_or("android");
            let state = if usb_state == "device" {
                DeviceState::Online
            } else {
                DeviceState::Offline
            };
            let detail = format!("android {model} ({usb_state})");
            upsert(c, ResourceKind::Device, host, serial, &detail, state, "")?;
            found += 1;
        }
    }
    Ok(found)
}

/// Snapshot host live resources (cpu cores, mem, disk) into the resources
/// table as a `host` row (detail carries the sample).
pub fn probe_host_resources(c: &rusqlite::Connection, host: &str) -> Result<()> {
    let cmd = "nproc; free -g | awk '/^Mem/{print $2\"G mem, \"$7\"G avail\"}'; df -h / | awk 'NR==2{print $4\" free on /\"}'";
    let detail = match probe(host, cmd) {
        Some(s) => s.replace('\n', " · "),
        None => {
            // unreachable or non-linux: record offline honestly
            upsert(
                c,
                ResourceKind::Host,
                host,
                host,
                "unreachable or non-linux probe",
                DeviceState::Offline,
                "",
            )?;
            return Ok(());
        }
    };
    let state = if detail.contains("unreachable") {
        DeviceState::Offline
    } else {
        DeviceState::Online
    };
    upsert(c, ResourceKind::Host, host, host, &detail, state, "")
        .map_err(|e| anyhow::anyhow!("upsert host: {e}"))?;
    Ok(())
}

/// Probe everything for one host (tools + adb devices + host resources).
pub fn probe_all(c: &rusqlite::Connection, host: &str) -> Result<(usize, usize)> {
    let tools = probe_tools(c, host)?;
    let devices = probe_adb_devices(c, host)?;
    probe_host_resources(c, host)?;
    Ok((tools, devices))
}

/// Probe the known fleet hosts.
pub fn probe_fleet(c: &rusqlite::Connection, hosts: &[String]) -> Result<()> {
    for h in hosts {
        let _ = probe_all(c, h);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_probe_finds_core_tools() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        store::resources::init(&c).unwrap();
        let n = probe_tools(&c, "local").unwrap();
        // local dev box must have at least git+python3+rust toolchain
        assert!(n >= 3, "expected >=3 tools, found {n}");
        let rows = store::resources::query(&c, Some(ResourceKind::Tool), Some("local")).unwrap();
        assert!(rows.iter().any(|r| r.name == "git"));
    }

    #[test]
    fn unreachable_host_records_offline() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        store::resources::init(&c).unwrap();
        // a host alias that cannot exist — ssh fails fast with BatchMode
        probe_host_resources(&c, "nonexistent-host-xyz").unwrap();
        let rows =
            store::resources::query(&c, Some(ResourceKind::Host), Some("nonexistent-host-xyz"))
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "offline");
    }

    #[test]
    fn adb_absence_is_honest_zero() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        store::resources::init(&c).unwrap();
        // local box may or may not have adb; either way this must not error
        let n = probe_adb_devices(&c, "local").unwrap();
        let _ = n; // 0 when no adb / no devices
    }
}
