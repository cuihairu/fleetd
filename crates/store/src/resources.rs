//! Resource plane: hosts, installed tools, and verifiable devices.
//!
//! Scope (user-approved 2026-11-26): fleetd tracks FACTS — what machines
//! exist, what tools are installed on them, what verification devices are
//! online and who is using them. Tool INSTALLATION is out of scope by
//! design: agents request dependencies in the task brief, the user approves,
//! Hermes/human installs; fleetd only records what exists.
//!
//! Tables: `resources` (hosts+tools+devices in one registry, kind-differentiated).

use rusqlite::Connection;

/// Resource kinds stored in the `resources` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// A machine that can run tasks (already in config hosts; mirrored here
    /// with live load/cpu/mem samples).
    Host,
    /// An installed tool on a host (name, version) — the caps[] fact.
    Tool,
    /// A verifiable device attached to a host (adb phone, emulator, windows
    /// box used for UI validation) with an occupancy state.
    Device,
    /// A virtualization endpoint tracked as one row (kind=vm,name=pve):
    /// the PVE node with its VM/LXC counts. vm:pve is the only VM provider
    /// (user decision 2026-11-28: vagrant/openstack rejected).
    Vm,
}

impl ResourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResourceKind::Host => "host",
            ResourceKind::Tool => "tool",
            ResourceKind::Device => "device",
            ResourceKind::Vm => "vm",
        }
    }
    pub fn from_str(s: &str) -> Option<ResourceKind> {
        match s {
            "host" => Some(ResourceKind::Host),
            "tool" => Some(ResourceKind::Tool),
            "device" => Some(ResourceKind::Device),
            "vm" => Some(ResourceKind::Vm),
            _ => None,
        }
    }
}

/// Device occupancy state (only meaningful for ResourceKind::Device).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Online,
    Busy,
    Offline,
}

impl DeviceState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeviceState::Online => "online",
            DeviceState::Busy => "busy",
            DeviceState::Offline => "offline",
        }
    }
    pub fn from_str(s: &str) -> Option<DeviceState> {
        match s {
            "online" => Some(DeviceState::Online),
            "busy" => Some(DeviceState::Busy),
            "offline" => Some(DeviceState::Offline),
            _ => None,
        }
    }
}

pub fn init(c: &Connection) -> Result<(), rusqlite::Error> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS resources (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL,             -- host | tool | device
            host TEXT NOT NULL,             -- owning machine (ssh alias or 'local')
            name TEXT NOT NULL,             -- tool name / device id / host name
            detail TEXT NOT NULL DEFAULT '',-- version, model, resolution, os build...
            state TEXT NOT NULL DEFAULT 'online', -- device occupancy; hosts/tools: online|offline
            holder TEXT NOT NULL DEFAULT '',-- who is using it (seat/task id), empty = free
            updated_at INTEGER NOT NULL,
            UNIQUE(kind, host, name)
         );
         CREATE INDEX IF NOT EXISTS idx_resources_kind ON resources(kind, host);",
    )
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Upsert one resource fact.
#[allow(clippy::too_many_arguments)]
pub fn upsert(
    c: &Connection,
    kind: ResourceKind,
    host: &str,
    name: &str,
    detail: &str,
    state: DeviceState,
    holder: &str,
) -> Result<(), rusqlite::Error> {
    c.execute(
        "INSERT INTO resources(kind,host,name,detail,state,holder,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(kind,host,name) DO UPDATE SET
           detail=excluded.detail, state=excluded.state,
           holder=excluded.holder, updated_at=excluded.updated_at",
        rusqlite::params![
            kind.as_str(),
            host,
            name,
            detail,
            state.as_str(),
            holder,
            now()
        ],
    )?;
    Ok(())
}

/// A row as returned by queries.
#[derive(Debug, Clone)]
pub struct ResourceRow {
    pub kind: String,
    pub host: String,
    pub name: String,
    pub detail: String,
    pub state: String,
    pub holder: String,
    pub updated_at: i64,
}

/// Query resources, optionally filtered.
pub fn query(
    c: &Connection,
    kind: Option<ResourceKind>,
    host: Option<&str>,
) -> Result<Vec<ResourceRow>, rusqlite::Error> {
    let mut st = c.prepare(
        "SELECT kind,host,name,detail,state,holder,updated_at FROM resources
         WHERE (?1 IS NULL OR kind=?1) AND (?2 IS NULL OR host=?2)
         ORDER BY kind, host, name",
    )?;
    let kind_s = kind.map(|k| k.as_str().to_string());
    let rows = st
        .query_map(rusqlite::params![kind_s.as_deref(), host], |r| {
            Ok(ResourceRow {
                kind: r.get(0)?,
                host: r.get(1)?,
                name: r.get(2)?,
                detail: r.get(3)?,
                state: r.get(4)?,
                holder: r.get(5)?,
                updated_at: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Claim a device (set busy + holder) only when it is online and free.
/// Returns false when the device is busy/offline/missing — caller must not proceed.
pub fn claim_device(
    c: &Connection,
    host: &str,
    name: &str,
    holder: &str,
) -> Result<bool, rusqlite::Error> {
    let n = c.execute(
        "UPDATE resources SET state='busy', holder=?1, updated_at=?2
         WHERE kind='device' AND host=?3 AND name=?4 AND state='online'",
        rusqlite::params![holder, now(), host, name],
    )?;
    Ok(n > 0)
}

/// Release a device (back to online, holder cleared). Idempotent.
pub fn release_device(c: &Connection, host: &str, name: &str) -> Result<bool, rusqlite::Error> {
    let n = c.execute(
        "UPDATE resources SET state='online', holder='', updated_at=?1
         WHERE kind='device' AND host=?3 AND name=?2",
        rusqlite::params![now(), name, host],
    )?;
    Ok(n > 0)
}

/// One host's fact line, assembled from resource rows. This struct is the
/// single rendering source for all three query outlets (CLI `resources
/// summary`, HTTP `/v1/resources`, MCP `ops.host-facts`) — FLEETD-RES-002:
/// same data, same source, no outlet-specific re-derivation.
#[derive(Debug, Clone, PartialEq)]
pub struct HostSummary {
    pub host: String,
    /// Installed tools found (daemon/carrier metadata rows excluded).
    pub tools: usize,
    /// Container daemon cell, e.g. "online(29.7.2)" or "offline".
    pub docker: Option<String>,
    pub podman: Option<String>,
    /// Carrier classification: "native-linux" | "wsl2" | "docker-desktop".
    pub carrier: Option<String>,
    /// PVE fleet size, e.g. "2vm+2lxc".
    pub vm: Option<String>,
}

/// Parse a vm-row detail ("vm2/lxc2 (pve-manager/8.4.14)") into (vm, lxc)
/// counts. Non-numeric or "?" counts → None (the summary then omits the
/// cell instead of inventing a zero).
pub fn parse_vm_counts(detail: &str) -> Option<(usize, usize)> {
    let rest = detail.strip_prefix("vm")?;
    let slash = rest.find('/')?;
    let v: usize = rest[..slash].parse().ok()?;
    let l: usize = rest[slash + 1..]
        .strip_prefix("lxc")?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    Some((v, l))
}

/// Daemon cell for the summary line: "online(<server version>)" when the
/// daemon answered, bare state otherwise.
fn daemon_cell(name: &str, rows: &[&ResourceRow]) -> Option<String> {
    rows.iter().find(|r| r.kind == "tool" && r.name == name).map(|r| {
        if r.state == "online" && !r.detail.is_empty() && r.detail != "daemon unreachable" {
            format!("online({})", r.detail)
        } else {
            r.state.clone()
        }
    })
}

/// Group resource rows into per-host fact lines, one per host present in
/// the rows (hosts never probed simply don't appear).
pub fn summarize(rows: &[ResourceRow]) -> Vec<HostSummary> {
    let mut hosts: Vec<String> = rows.iter().map(|r| r.host.clone()).collect();
    hosts.sort();
    hosts.dedup();
    let mut out = Vec::new();
    for host in hosts {
        let hr: Vec<&ResourceRow> = rows.iter().filter(|r| r.host == host).collect();
        let tools = hr
            .iter()
            .filter(|r| r.kind == "tool" && !r.name.ends_with("-daemon") && r.name != "carrier")
            .count();
        let vm = hr
            .iter()
            .find(|r| r.kind == "vm")
            .and_then(|r| parse_vm_counts(&r.detail))
            .map(|(v, l)| format!("{v}vm+{l}lxc"));
        out.push(HostSummary {
            host,
            tools,
            docker: daemon_cell("docker-daemon", &hr),
            podman: daemon_cell("podman-daemon", &hr),
            carrier: hr
                .iter()
                .find(|r| r.kind == "tool" && r.name == "carrier")
                .map(|r| {
                    r.detail
                        .split(" (")
                        .next()
                        .unwrap_or("")
                        .to_string()
                }),
            vm,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init(&c).unwrap();
        c
    }

    #[test]
    fn upsert_and_query() {
        let c = mem();
        upsert(
            &c,
            ResourceKind::Tool,
            "openclaw",
            "cargo",
            "1.88",
            DeviceState::Online,
            "",
        )
        .unwrap();
        upsert(
            &c,
            ResourceKind::Tool,
            "openclaw",
            "node",
            "22.23",
            DeviceState::Online,
            "",
        )
        .unwrap();
        upsert(
            &c,
            ResourceKind::Tool,
            "imac",
            "dotnet",
            "8.0",
            DeviceState::Online,
            "",
        )
        .unwrap();
        let all = query(&c, Some(ResourceKind::Tool), None).unwrap();
        assert_eq!(all.len(), 3);
        let oncl = query(&c, Some(ResourceKind::Tool), Some("openclaw")).unwrap();
        assert_eq!(oncl.len(), 2);
    }

    #[test]
    fn upsert_overwrites_detail() {
        let c = mem();
        upsert(
            &c,
            ResourceKind::Tool,
            "local",
            "adb",
            "34.0",
            DeviceState::Online,
            "",
        )
        .unwrap();
        upsert(
            &c,
            ResourceKind::Tool,
            "local",
            "adb",
            "35.0",
            DeviceState::Online,
            "",
        )
        .unwrap();
        let rows = query(&c, Some(ResourceKind::Tool), Some("local")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].detail, "35.0");
    }

    #[test]
    fn claim_only_when_online_and_free() {
        let c = mem();
        upsert(
            &c,
            ResourceKind::Device,
            "local",
            "adb-emu-5554",
            "pixel8 api34",
            DeviceState::Online,
            "",
        )
        .unwrap();
        // first claim wins
        assert!(claim_device(&c, "local", "adb-emu-5554", "kode").unwrap());
        // second claim on busy fails
        assert!(!claim_device(&c, "local", "adb-emu-5554", "croupier").unwrap());
        let rows = query(&c, Some(ResourceKind::Device), Some("local")).unwrap();
        assert_eq!(rows[0].state, "busy");
        assert_eq!(rows[0].holder, "kode");
        // release then re-claim by another seat
        assert!(release_device(&c, "local", "adb-emu-5554").unwrap());
        assert!(claim_device(&c, "local", "adb-emu-5554", "croupier").unwrap());
    }

    #[test]
    fn claim_missing_device_fails() {
        let c = mem();
        assert!(!claim_device(&c, "local", "nope", "kode").unwrap());
    }

    #[test]
    fn release_is_idempotent() {
        let c = mem();
        upsert(
            &c,
            ResourceKind::Device,
            "imac",
            "win11-box",
            "22h2",
            DeviceState::Online,
            "",
        )
        .unwrap();
        assert!(release_device(&c, "imac", "win11-box").unwrap());
        // releasing again still "succeeds" (already online) — idempotent
        assert!(release_device(&c, "imac", "win11-box").unwrap());
    }

    #[test]
    fn kind_roundtrip() {
        assert_eq!(ResourceKind::from_str("device"), Some(ResourceKind::Device));
        assert_eq!(ResourceKind::from_str("bogus"), None);
        assert_eq!(DeviceState::from_str("busy"), Some(DeviceState::Busy));
        assert_eq!(DeviceState::from_str("x"), None);
        assert_eq!(ResourceKind::from_str("vm"), Some(ResourceKind::Vm));
        assert_eq!(ResourceKind::Vm.as_str(), "vm");
    }

    #[test]
    fn summarize_renders_one_line_facts() {
        let c = mem();
        upsert(&c, ResourceKind::Host, "work-runner", "work-runner", "cpus=4 mem=8G", DeviceState::Online, "").unwrap();
        upsert(&c, ResourceKind::Tool, "work-runner", "cargo", "cargo 1.88", DeviceState::Online, "").unwrap();
        upsert(&c, ResourceKind::Tool, "work-runner", "docker", "docker 29.7.2", DeviceState::Online, "").unwrap();
        upsert(&c, ResourceKind::Tool, "work-runner", "docker-daemon", "29.7.2", DeviceState::Online, "").unwrap();
        upsert(&c, ResourceKind::Tool, "work-runner", "carrier", "native-linux (Debian GNU/Linux 13)", DeviceState::Online, "").unwrap();
        upsert(&c, ResourceKind::Tool, "work-runner", "docker-daemon", "daemon unreachable", DeviceState::Offline, "").unwrap();
        // ^ overwrite with offline: last write wins, cell must drop the version
        upsert(&c, ResourceKind::Vm, "shanghai-pve", "pve", "vm2/lxc2 (pve-manager/8.4.14)", DeviceState::Online, "").unwrap();
        let rows = query(&c, None, None).unwrap();
        let s = summarize(&rows);
        assert_eq!(s.len(), 2);
        let wr = s.iter().find(|x| x.host == "work-runner").unwrap();
        assert_eq!(wr.tools, 2, "daemon+carrier rows are metadata, not tools");
        assert_eq!(wr.docker, Some("offline".into()));
        assert_eq!(wr.carrier, Some("native-linux".into()));
        assert_eq!(wr.vm, None);
        let pve = s.iter().find(|x| x.host == "shanghai-pve").unwrap();
        assert_eq!(pve.vm, Some("2vm+2lxc".into()));
        assert_eq!(pve.tools, 0);
        // and the parse face behind that cell round-trips the detail form
        assert_eq!(parse_vm_counts("vm2/lxc2 (pve-manager/8.4.14)"), Some((2, 2)));
        assert_eq!(parse_vm_counts("vm?/lxc2"), None);
        assert_eq!(parse_vm_counts("unreachable"), None);
    }
}
