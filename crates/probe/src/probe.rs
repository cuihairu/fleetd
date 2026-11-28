//! Fact collection: probe hosts for installed tools, container daemon
//! states, host carriers, and PVE capacity.
//!
//! Probes are read-only shell one-liners run locally or over ssh. Failures
//! are honest: a missing tool simply doesn't appear; an unreachable host
//! yields an offline marker; a CLI without a running daemon yields an
//! offline daemon row. Nothing here installs anything.
//!
//! OS routing (FLEETD-RES-002): Linux/macOS hosts run bash one-liners;
//! Windows hosts get ONE PowerShell script shipped over the ssh file channel
//! (`scp` + `powershell -File`) — never bash syntax on Windows. That branch
//! fixed the openclaw offline mislabel (bash probes on a Windows box always
//! failed, so the host row said "unreachable").
//!
//! The parsing faces are pure functions (unit-testable without any box):
//! `parse_windows_output`, `daemon_verdict`, `classify_carrier_linux`,
//! `classify_carrier_windows`, `pve_verdict`, `normalize_runtime`.

use anyhow::Result;
use std::process::Command;

use store::resources::{upsert, DeviceState, ResourceKind};

/// Host OS flavor deciding which command syntax a probe uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Linux,
    Windows,
    MacOs,
}

impl HostOs {
    /// Parse the config `os` field ("linux"/"windows"/"macos"). Unknown → None.
    pub fn from_config(s: &str) -> Option<HostOs> {
        match s.to_ascii_lowercase().as_str() {
            "linux" => Some(HostOs::Linux),
            "windows" => Some(HostOs::Windows),
            "macos" | "mac" | "darwin" => Some(HostOs::MacOs),
            _ => None,
        }
    }
}

/// The resource-probe registry: hosts the default sweep covers. openclaw and
/// imac are Windows boxes (PowerShell channel); the rest run bash. Mirrors
/// the fleet inventory in the fleet-load-dispatch skill.
pub const FLEET_HOSTS: &[(&str, HostOs)] = &[
    ("local", HostOs::Linux),
    ("work-runner", HostOs::Linux),
    ("openclaw", HostOs::Windows),
    ("imac", HostOs::Windows),
];

/// The PVE endpoint (kind=vm,name=pve). vm:pve is the only VM provider
/// (user decision 2026-11-28: vagrant is retiring, openstack has no infra).
pub const PVE_HOST: &str = "shanghai-pve";

/// Default sweep targets: registry hosts + the PVE endpoint.
pub fn registry_hosts() -> Vec<String> {
    let mut v: Vec<String> = FLEET_HOSTS.iter().map(|(n, _)| n.to_string()).collect();
    v.push(PVE_HOST.to_string());
    v
}

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

/// Runtime name normalization over the user-ratified enum (2026-11-28):
/// docker | podman | nerdctl(containerd) | incus(lxd). Aliases map onto the
/// canonical label; rejected runtimes (vagrant/openstack) normalize to None.
pub fn normalize_runtime(s: &str) -> Option<&'static str> {
    match s.to_ascii_lowercase().split('(').next().unwrap_or("").trim() {
        "docker" => Some("docker"),
        "podman" => Some("podman"),
        "nerdctl" | "containerd" => Some("nerdctl(containerd)"),
        "incus" | "lxd" => Some("incus(lxd)"),
        _ => None,
    }
}

/// Known tool probes (Linux/macOS bash syntax): (resource name, command).
/// Detail = first line of stdout. Windows hosts keep their own command
/// table inside WIN_PROBE_PS1 (`go version` has no --version flag there).
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
    ("podman", "podman --version"),
    ("nerdctl", "nerdctl --version"),
    ("incus", "incus --version"),
];

/// Probe one host for installed tools (Linux/macOS path), upserting facts
/// into the ledger. Returns the names found (daemon probing reuses this set
/// to decide whether a daemon row is meaningful).
pub fn probe_tools(c: &rusqlite::Connection, host: &str) -> Result<Vec<String>> {
    let mut found = Vec::new();
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
            found.push((*name).to_string());
        }
    }
    Ok(found)
}

/// Container daemon-state verdict from a `info --format '{{.ServerVersion}}'`
/// probe. A CLI version string alone proves nothing about the service —
/// this is the online/offline decision point (FLEETD-RES-002): only a
/// version-looking answer marks the daemon online.
pub fn daemon_verdict(info_out: Option<&str>) -> (DeviceState, String) {
    match info_out {
        Some(v)
            if v.chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false) =>
        {
            let first = v.lines().next().unwrap_or("").trim().to_string();
            (DeviceState::Online, first)
        }
        _ => (DeviceState::Offline, "daemon unreachable".to_string()),
    }
}

/// Probe container daemon state for a Linux/macOS host. Scope is docker +
/// podman (user-ratified); podman has the `version --format` fallback
/// because its info template differs across releases. Emits one
/// `<runtime>-daemon` tool row (online + server version / offline).
fn probe_daemons(
    c: &rusqlite::Connection,
    host: &str,
    tools_found: &[String],
) -> Result<()> {
    for rt in ["docker", "podman"] {
        // No CLI on the host → no daemon row (nothing to judge).
        if !tools_found.iter().any(|t| t == rt) {
            continue;
        }
        let mut out = probe(host, &format!("{rt} info --format '{{{{.ServerVersion}}}}'"));
        if out.is_none() && rt == "podman" {
            out = probe(host, "podman version --format '{{.Server.Version}}'");
        }
        let (state, detail) = daemon_verdict(out.as_deref());
        upsert(
            c,
            ResourceKind::Tool,
            host,
            &format!("{rt}-daemon"),
            &detail,
            state,
            "",
        )?;
    }
    Ok(())
}

/// Carrier classification for Linux hosts (pure). /proc/version containing
/// "microsoft" (any case) = WSL2; otherwise an /etc/os-release PRETTY_NAME
/// proves native-linux. Returns (carrier, evidence) — evidence goes into
/// the row detail. macOS stays enum-reserved (orbstack/colima unimplemented
/// until a Mac joins the fleet).
pub fn classify_carrier_linux(
    proc_version: Option<&str>,
    os_pretty: Option<&str>,
) -> Option<(String, String)> {
    if let Some(pv) = proc_version {
        if pv.to_ascii_lowercase().contains("microsoft") {
            let evidence: String = pv.chars().take(80).collect();
            return Some(("wsl2".to_string(), evidence));
        }
    }
    os_pretty.map(|p| ("native-linux".to_string(), p.to_string()))
}

/// Carrier classification for Windows hosts (pure): a live docker daemon on
/// Windows is Docker Desktop in practice; otherwise WSL2 when the WSL
/// feature answers; bare Windows has no carrier (None → no row).
pub fn classify_carrier_windows(wsl_ok: bool, docker_online: bool) -> Option<String> {
    if docker_online {
        Some("docker-desktop".to_string())
    } else if wsl_ok {
        Some("wsl2".to_string())
    } else {
        None
    }
}

/// Carrier probe (Linux path): /proc/version + /etc/os-release in ONE ssh
/// round trip. No carrier evidence → no row (honest absence).
fn probe_carrier_linux(c: &rusqlite::Connection, host: &str) -> Result<()> {
    let cmd =
        "head -n1 /proc/version 2>/dev/null; grep '^PRETTY_NAME=' /etc/os-release 2>/dev/null";
    let Some(out) = probe(host, cmd) else {
        return Ok(());
    };
    let mut proc_version = None;
    let mut os_pretty = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("PRETTY_NAME=") {
            os_pretty = Some(p.trim_matches('"').to_string());
        } else if !line.trim().is_empty() {
            proc_version = Some(line.to_string());
        }
    }
    if let Some((carrier, evidence)) =
        classify_carrier_linux(proc_version.as_deref(), os_pretty.as_deref())
    {
        upsert(
            c,
            ResourceKind::Tool,
            host,
            "carrier",
            &format!("{carrier} ({evidence})"),
            DeviceState::Online,
            "",
        )?;
    }
    Ok(())
}

/// The one PowerShell probe script (Windows branch). Shipped as a FILE over
/// the scp channel and run with `powershell -File` — inline
/// `powershell -Command` quoting over ssh is a known failure shape
/// (selfupdate lessons: scripts go through files). Emits a TAB-separated
/// line protocol parsed by `parse_windows_output`:
///   TOOL\t<name>\t<detail>
///   HOST\twindows\t<cpus>\t<memG>\t<diskG free>\t<caption>
///   WSL\t0|1
///   DAEMON\t<runtime>\tonline|offline\t<server version when online>
const WIN_PROBE_PS1: &str = r#"$ErrorActionPreference = 'Continue'
$lines = New-Object System.Collections.Generic.List[string]
$specs = [ordered]@{
  'cargo' = '--version'; 'rustc' = '--version'; 'node' = '--version'
  'pnpm' = '--version'; 'go' = 'version'; 'python' = '--version'
  'adb' = 'version'; 'dotnet' = '--version'; 'docker' = '--version'
  'claude' = '--version'; 'git' = '--version'; 'gh' = '--version'
  'podman' = '--version'; 'nerdctl' = '--version'; 'incus' = '--version'
}
foreach ($t in $specs.Keys) {
  $c = Get-Command $t -ErrorAction SilentlyContinue
  if ($null -ne $c) {
    $v = ''
    try { $v = (Invoke-Expression ($t + ' ' + $specs[$t]) 2>$null | Select-Object -First 1) } catch { $v = '' }
    if ($null -eq $v) { $v = '' }
    $v = ($v | Out-String).Trim()
    if ($v -eq '') { $v = $c.Source }
    $lines.Add("TOOL`t$t`t$v")
  }
}
$cs = Get-CimInstance Win32_ComputerSystem
$os = Get-CimInstance Win32_OperatingSystem
$disk = Get-CimInstance Win32_LogicalDisk -Filter "DeviceID='C:'"
$cpus = $cs.NumberOfLogicalProcessors
$memG = [math]::Round($cs.TotalPhysicalMemory / 1GB)
$diskG = [math]::Round($disk.FreeSpace / 1GB)
$lines.Add("HOST`twindows`t$cpus`t$memG`t$diskG`t$($os.Caption)")
$wslOk = 0
try { wsl --status 2>$null | Out-Null; if ($LASTEXITCODE -eq 0) { $wslOk = 1 } } catch { $wslOk = 0 }
$lines.Add("WSL`t$wslOk")
foreach ($rt in @('docker','podman')) {
  if ($null -ne (Get-Command $rt -ErrorAction SilentlyContinue)) {
    $dv = ''
    try { $dv = (Invoke-Expression ($rt + " info --format '{{.ServerVersion}}'") 2>$null | Out-String).Trim() } catch { $dv = '' }
    if ($dv -eq '' -and $rt -eq 'podman') {
      try { $dv = (podman version --format '{{.Server.Version}}' 2>$null | Out-String).Trim() } catch { $dv = '' }
    }
    if ($dv -ne '') { $lines.Add("DAEMON`t$rt`tonline`t$dv") } else { $lines.Add("DAEMON`t$rt`toffline`t") }
  }
}
[Console]::Out.Write(($lines -join "`n") + "`n")
"#;

/// Parsed facts from one Windows probe run (line protocol of WIN_PROBE_PS1).
#[derive(Debug, Default, PartialEq)]
pub struct WindowsFacts {
    /// (name, detail) tool rows.
    pub tools: Vec<(String, String)>,
    /// HOST line payload: windows\tcpus\tmemG\tdiskG\tcaption.
    pub host: Option<String>,
    /// WSL feature available on the host.
    pub wsl: bool,
    /// (runtime, online, server-version) daemon lines.
    pub daemons: Vec<(String, bool, String)>,
}

/// Pure parser for the Windows probe line protocol — the Windows branch is
/// unit-testable without a Windows box. Banner/noise lines are ignored.
pub fn parse_windows_output(out: &str) -> WindowsFacts {
    let mut f = WindowsFacts::default();
    for line in out.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        match parts.first().copied() {
            Some("TOOL") if parts.len() >= 3 => {
                f.tools.push((parts[1].to_string(), parts[2].to_string()));
            }
            Some("HOST") if parts.len() >= 6 => {
                f.host = Some(parts[1..].join("\t"));
            }
            Some("WSL") if parts.len() >= 2 => f.wsl = parts[1] == "1",
            Some("DAEMON") if parts.len() >= 3 => {
                let online = parts[2] == "online";
                let ver = parts.get(3).copied().unwrap_or("").to_string();
                f.daemons.push((parts[1].to_string(), online, ver));
            }
            _ => {}
        }
    }
    f
}

/// Run the PowerShell probe on a Windows host: write the script to a temp
/// file, scp it (explicit filename, home-relative target — the only scp
/// form that reliably lands on Windows), run `powershell -File`, best-effort
/// delete. Returns the script's stdout or None (unreachable).
fn probe_windows(host: &str) -> Option<String> {
    let local = std::env::temp_dir().join(format!("fleetd-probe-{host}.ps1"));
    std::fs::write(&local, WIN_PROBE_PS1).ok()?;
    let scp = Command::new("scp")
        .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes"])
        .arg(&local)
        .arg(format!("{host}:fleetd-probe.ps1"))
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&local);
    if !scp.status.success() {
        return None;
    }
    let out = Command::new("ssh")
        .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
        .args([
            "powershell",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            "fleetd-probe.ps1",
        ])
        .output()
        .ok()?;
    // Best-effort cleanup; failure here must not fail the probe.
    let _ = Command::new("ssh")
        .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
        .args(["del", "fleetd-probe.ps1"])
        .output();
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

/// Write parsed Windows facts into the ledger. Split from
/// `probe_windows_host` so the Windows branch's DB face is testable on a
/// Linux dev box. Returns the tool count.
fn store_windows_facts(
    c: &rusqlite::Connection,
    host: &str,
    facts: &WindowsFacts,
) -> Result<usize> {
    for (name, detail) in &facts.tools {
        let d: String = detail.chars().take(60).collect();
        upsert(c, ResourceKind::Tool, host, name, &d, DeviceState::Online, "")?;
    }
    match &facts.host {
        Some(h) => {
            let d = h.split('\t').collect::<Vec<_>>().join(" · ");
            upsert(c, ResourceKind::Host, host, host, &d, DeviceState::Online, "")?;
        }
        None => {
            // Script ran but yielded no HOST line: reachable, thin facts.
            upsert(
                c,
                ResourceKind::Host,
                host,
                host,
                "reachable (no host facts)",
                DeviceState::Online,
                "",
            )?;
        }
    }
    let docker_online = facts
        .daemons
        .iter()
        .any(|(rt, on, _)| rt == "docker" && *on);
    if let Some(carrier) = classify_carrier_windows(facts.wsl, docker_online) {
        let evidence = if docker_online {
            "docker daemon online"
        } else {
            "wsl available"
        };
        upsert(
            c,
            ResourceKind::Tool,
            host,
            "carrier",
            &format!("{carrier} ({evidence})"),
            DeviceState::Online,
            "",
        )?;
    }
    for (rt, on, ver) in &facts.daemons {
        let (state, detail) = if *on {
            (DeviceState::Online, ver.clone())
        } else {
            (
                DeviceState::Offline,
                "daemon unreachable".to_string(),
            )
        };
        upsert(
            c,
            ResourceKind::Tool,
            host,
            &format!("{rt}-daemon"),
            &detail,
            state,
            "",
        )?;
    }
    Ok(facts.tools.len())
}

/// Full probe of a Windows host through the PowerShell file channel. The
/// fix for the openclaw mislabel: a reachable Windows box now lands a
/// state=online host row with real facts instead of "unreachable".
fn probe_windows_host(c: &rusqlite::Connection, host: &str) -> Result<usize> {
    match probe_windows(host) {
        Some(out) => store_windows_facts(c, host, &parse_windows_output(&out)),
        None => {
            upsert(
                c,
                ResourceKind::Host,
                host,
                host,
                "unreachable",
                DeviceState::Offline,
                "",
            )?;
            Ok(0)
        }
    }
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
            // unreachable: record offline honestly (Windows hosts are routed
            // to the PowerShell branch before this point)
            upsert(
                c,
                ResourceKind::Host,
                host,
                host,
                "unreachable or probe failed",
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

/// PVE endpoint verdict (pure). pveversion answering = node online; detail
/// carries the VM/LXC counts rendered by the summary as vm:pve=2vm+2lxc.
/// Counts that fail to parse render as "?" — never a fabricated zero.
pub fn pve_verdict(
    version: Option<&str>,
    qm: Option<&str>,
    pct: Option<&str>,
) -> (DeviceState, String) {
    let count = |s: Option<&str>| -> String {
        match s.and_then(|s| s.trim().parse::<usize>().ok()) {
            Some(n) => n.to_string(),
            None => "?".to_string(),
        }
    };
    match version {
        Some(v) if v.starts_with("pve-manager") => {
            let short = v.split_whitespace().next().unwrap_or(v);
            (
                DeviceState::Online,
                format!("vm{}/lxc{} ({})", count(qm), count(pct), short),
            )
        }
        _ => (DeviceState::Offline, "unreachable".to_string()),
    }
}

/// PVE endpoint probe (vm:pve — the only VM provider, user decision
/// 2026-11-28). One row kind=vm,name=pve, detail "vm<N>/lxc<N>
/// (pve-manager/…)". Returns the verdict for CLI printing.
pub fn probe_pve(c: &rusqlite::Connection, host: &str) -> Result<(DeviceState, String)> {
    let ver = probe(host, "pveversion");
    let qm = probe(host, "qm list 2>/dev/null | tail -n +2 | wc -l");
    let pct = probe(host, "pct list 2>/dev/null | tail -n +2 | wc -l");
    let (state, detail) = pve_verdict(ver.as_deref(), qm.as_deref(), pct.as_deref());
    upsert(c, ResourceKind::Vm, host, "pve", &detail, state, "")?;
    Ok((state, detail))
}

/// Probe everything for one host (tools + daemons + carrier + adb devices +
/// host resources), routed by OS. Returns (tools, devices).
pub fn probe_all(c: &rusqlite::Connection, host: &str, os: HostOs) -> Result<(usize, usize)> {
    match os {
        HostOs::Windows => {
            let tools = probe_windows_host(c, host)?;
            Ok((tools, 0))
        }
        HostOs::Linux | HostOs::MacOs => {
            let names = probe_tools(c, host)?;
            let devices = probe_adb_devices(c, host)?;
            probe_daemons(c, host, &names)?;
            probe_carrier_linux(c, host)?;
            probe_host_resources(c, host)?;
            Ok((names.len(), devices))
        }
    }
}

/// Probe an explicit host list (or the registry when the list is the
/// default sweep). Registry hosts carry their OS routing; the PVE endpoint
/// gets the vm:pve probe; unknown names fall back to the Linux path.
pub fn probe_fleet(c: &rusqlite::Connection, hosts: &[String]) -> Result<()> {
    for h in hosts {
        if h == PVE_HOST {
            let _ = probe_pve(c, h);
            continue;
        }
        let os = FLEET_HOSTS
            .iter()
            .find(|(n, _)| n == h)
            .map(|(_, os)| *os)
            .unwrap_or(HostOs::Linux);
        let _ = probe_all(c, h, os);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- runtime normalization (user-ratified enum) ----

    #[test]
    fn runtime_normalization_covers_ratified_enum() {
        assert_eq!(normalize_runtime("docker"), Some("docker"));
        assert_eq!(normalize_runtime("podman"), Some("podman"));
        assert_eq!(normalize_runtime("containerd"), Some("nerdctl(containerd)"));
        assert_eq!(normalize_runtime("nerdctl"), Some("nerdctl(containerd)"));
        assert_eq!(normalize_runtime("lxd"), Some("incus(lxd)"));
        assert_eq!(normalize_runtime("incus"), Some("incus(lxd)"));
        // rejected providers normalize to nothing
        assert_eq!(normalize_runtime("vagrant"), None);
        assert_eq!(normalize_runtime("openstack"), None);
        // the tool table actually carries the three added runtimes
        for rt in ["podman", "nerdctl", "incus"] {
            assert!(
                TOOL_PROBES.iter().any(|(n, _)| *n == rt),
                "TOOL_PROBES missing {rt}"
            );
        }
    }

    // ---- daemon state: version string != service running ----

    #[test]
    fn daemon_verdict_online_offline() {
        assert_eq!(
            daemon_verdict(Some("29.7.2")),
            (DeviceState::Online, "29.7.2".to_string())
        );
        assert_eq!(
            daemon_verdict(Some("5.6.1\n")),
            (DeviceState::Online, "5.6.1".to_string())
        );
        assert_eq!(daemon_verdict(None).0, DeviceState::Offline);
        assert_eq!(daemon_verdict(Some("")).0, DeviceState::Offline);
        // warning prose on stdout is not a server version → offline
        assert_eq!(daemon_verdict(Some("Cannot connect")).0, DeviceState::Offline);
    }

    // ---- Windows branch: line-protocol parse + ledger face ----

    #[test]
    fn windows_branch_parses_line_protocol() {
        let out = "TOOL\tgit\tgit version 2.47.1.windows.1\n\
                   TOOL\tclaude\t2.0.76 (Claude Code)\n\
                   HOST\twindows\t8\t16\t120\tMicrosoft Windows 11 Pro\n\
                   WSL\t1\n\
                   DAEMON\tdocker\toffline\t\n\
                   some banner noise\n";
        let f = parse_windows_output(out);
        assert_eq!(f.tools.len(), 2);
        assert_eq!(
            f.tools[0],
            ("git".to_string(), "git version 2.47.1.windows.1".to_string())
        );
        assert!(f.wsl);
        assert_eq!(
            f.daemons,
            vec![("docker".to_string(), false, String::new())]
        );
        assert!(
            f.host
                .as_deref()
                .unwrap_or("")
                .starts_with("windows\t8\t16\t120"),
            "host line keeps cpus/mem/disk/caption: {:?}",
            f.host
        );
        // carrier decision matrix on windows
        assert_eq!(classify_carrier_windows(f.wsl, false), Some("wsl2".into()));
        assert_eq!(
            classify_carrier_windows(f.wsl, true),
            Some("docker-desktop".into())
        );
        assert_eq!(classify_carrier_windows(false, false), None);
    }

    #[test]
    fn windows_facts_upsert_online_host_not_mislabel() {
        // The openclaw fix at the DB face: a reachable Windows box lands a
        // state=online host row plus tool/daemon/carrier rows.
        let c = rusqlite::Connection::open_in_memory().unwrap();
        store::resources::init(&c).unwrap();
        let facts = parse_windows_output(
            "TOOL\tgit\tgit version 2.47\n\
             TOOL\tcargo\tcargo 1.88\n\
             HOST\twindows\t8\t16\t120\tMicrosoft Windows 11 Pro\n\
             WSL\t0\n\
             DAEMON\tdocker\tonline\t27.5.1\n",
        );
        let tools = store_windows_facts(&c, "openclaw", &facts).unwrap();
        assert_eq!(tools, 2);
        let rows = store::resources::query(&c, None, Some("openclaw")).unwrap();
        let host = rows.iter().find(|r| r.kind == "host").unwrap();
        assert_eq!(host.state, "online", "no offline mislabel on reachable windows");
        let daemon = rows.iter().find(|r| r.name == "docker-daemon").unwrap();
        assert_eq!(daemon.state, "online");
        assert_eq!(daemon.detail, "27.5.1");
        let carrier = rows.iter().find(|r| r.name == "carrier").unwrap();
        assert!(
            carrier.detail.starts_with("docker-desktop"),
            "docker online beats wsl flag: {}",
            carrier.detail
        );
    }

    // ---- carrier classification (Linux face) ----

    #[test]
    fn carrier_classification_linux() {
        // WSL2 kernel string beats os-release
        let (v, _) = classify_carrier_linux(
            Some("Linux version 5.15.167.4-microsoft-standard-WSL2 (x86_64)"),
            Some("Ubuntu 22.04.5 LTS"),
        )
        .unwrap();
        assert_eq!(v, "wsl2");
        let (v, ev) = classify_carrier_linux(
            Some("Linux version 6.8.0-44-generic"),
            Some("Ubuntu 26.04.1 LTS"),
        )
        .unwrap();
        assert_eq!(v, "native-linux");
        assert_eq!(ev, "Ubuntu 26.04.1 LTS");
        // bare kernel without os-release → no carrier row
        assert_eq!(classify_carrier_linux(Some("Linux version 6.8"), None), None);
        assert_eq!(classify_carrier_linux(None, None), None);
    }

    // ---- PVE collection (mocked output) ----

    #[test]
    fn pve_collection_mock() {
        let (s, d) = pve_verdict(
            Some("pve-manager/8.4.14 (running kernel: 6.8.4-2-pve)"),
            Some("2"),
            Some("2"),
        );
        assert_eq!(s, DeviceState::Online);
        assert_eq!(d, "vm2/lxc2 (pve-manager/8.4.14)");
        // unreachable node → offline marker, not fabricated counts
        let (s, d) = pve_verdict(None, None, None);
        assert_eq!(s, DeviceState::Offline);
        assert_eq!(d, "unreachable");
        // unparseable counts render "?" and the summary cell drops out
        let (_, d) = pve_verdict(Some("pve-manager/8.4.14 (x)"), None, Some("2"));
        assert!(d.contains("vm?/lxc2"));
        assert_eq!(store::resources::parse_vm_counts(&d), None);
        // parse face behind vm:pve=2vm+2lxc
        assert_eq!(
            store::resources::parse_vm_counts("vm2/lxc2 (pve-manager/8.4.14)"),
            Some((2, 2))
        );
    }

    // ---- live faces (local box, unchanged behavior contracts) ----

    #[test]
    fn local_probe_finds_core_tools() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        store::resources::init(&c).unwrap();
        let found = probe_tools(&c, "local").unwrap();
        // local dev box must have at least git+python3+rust toolchain
        assert!(found.len() >= 3, "expected >=3 tools, found {}", found.len());
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
