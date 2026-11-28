//! Ship (per-machine fleetd agent) and fleet (aggregation) topology config.
//!
//! Architecture (user order 2026-11-26): every machine runs one fleetd as a
//! "ship" — it owns that machine's hook ingestion, resource probes, job
//! scheduler and its own ledger. A flagship machine additionally runs the
//! "fleet" aggregation: it pulls each ship's facts and gives a whole-fleet
//! view plus cross-ship dispatch decisions.
//!
//! Config: `~/.fleetd/fleet.toml`
//! ```toml
//! [ship]
//! name = "flagship"              # this ship's name (events are tagged with it)
//!
//! [fleet]                   # optional: only the flagship has this section
//! flagship = true
//! ships = [                 # ships to aggregate (excluding self)
//!   { name = "work-runner", ssh = "work-runner" },
//!   { name = "openclaw",    ssh = "openclaw" },
//! ]
//! ```

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ShipCfg {
    /// This ship's name (defaults to hostname). Event rows are tagged with it.
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FleetShip {
    pub name: String,
    /// SSH alias used to pull this ship's ledger (read-only scp/ssh).
    pub ssh: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FleetCfg {
    /// This ship aggregates the fleet (flagship role).
    #[serde(default)]
    pub flagship: bool,
    /// Other ships to aggregate.
    #[serde(default)]
    pub ships: Vec<FleetShip>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Topology {
    #[serde(default)]
    pub ship: ShipCfg,
    #[serde(default)]
    pub fleet: FleetCfg,
}

impl Topology {
    pub fn load() -> Result<Self> {
        let p = Self::path();
        if !p.exists() {
            // No topology file = single-ship default (name = hostname), no fleet.
            let name = hostname();
            return Ok(Topology {
                ship: ShipCfg { name },
                fleet: FleetCfg::default(),
            });
        }
        let raw = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let t: Topology = toml::from_str(&raw).with_context(|| format!("parse {}", p.display()))?;
        let mut t = t;
        if t.ship.name.is_empty() {
            t.ship.name = hostname();
        }
        Ok(t)
    }

    pub fn path() -> PathBuf {
        dirs::home_dir()
            .expect("home dir")
            .join(".fleetd")
            .join("fleet.toml")
    }

    /// Is this ship the fleet flagship (aggregation role)?
    pub fn is_flagship(&self) -> bool {
        self.fleet.flagship
    }
}

pub fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// Pull a remote ship's ledger into the flagship's aggregation area.
/// Uses scp over the ship's ssh alias; returns the local copy path.
pub fn pull_ship_ledger(ssh: &str, ship_name: &str) -> Result<PathBuf> {
    let flagship_dir = dirs::home_dir()
        .expect("home dir")
        .join(".fleetd")
        .join("ships");
    fs::create_dir_all(&flagship_dir)?;
    let local = flagship_dir.join(format!("{ship_name}.db"));
    let remote_path = "~/.fleetd/ledger.db";
    let status: Result<()> = if ssh == "local" {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        fs::copy(PathBuf::from(home).join(".fleetd/ledger.db"), &local)
            .map(|_| ())
            .map_err(|e| e.into())
    } else {
        let out = std::process::Command::new("ssh")
            .args([
                "-o",
                "ConnectTimeout=8",
                "-o",
                "BatchMode=yes",
                ssh,
                "cat",
                remote_path,
            ])
            .output()?;
        if out.status.success() {
            fs::write(&local, &out.stdout)?;
            Ok(())
        } else {
            bail!(
                "ssh cat failed on {ssh}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        }
    };
    status.with_context(|| format!("pull ledger from {ssh}"))?;
    Ok(local)
}
