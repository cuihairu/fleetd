use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

/// A registered machine in the fleet.
#[derive(Debug, Clone, Deserialize)]
pub struct Host {
    pub name: String,
    /// SSH target alias (e.g. "imac", "openclaw") or "local" for this host.
    pub ssh: String,
    /// Maximum concurrent claude tasks this host can run.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// OS family for command selection: "linux" | "windows" | "macos".
    #[serde(default = "default_os")]
    pub os: String,
    /// Working directory where dispatched tasks run (per-repo checkout root).
    #[serde(default)]
    pub workdir: String,
    /// Scratch dir for task files and logs; cleaned periodically.
    #[serde(default)]
    pub scratch: String,
    /// Delete scratch entries older than this many hours (0 = never clean).
    #[serde(default)]
    pub cleanup_hours: u64,
}

fn default_max_concurrent() -> usize {
    4
}

fn default_os() -> String {
    "linux".into()
}

impl Host {
    /// Effective scratch dir: configured, else a per-host default.
    pub fn scratch_dir(&self) -> String {
        if !self.scratch.is_empty() {
            return self.scratch.clone();
        }
        match self.os.as_str() {
            "windows" => r"C:\Users\cui\fleetd".to_string(),
            _ => "/tmp/fleetd".to_string(),
        }
    }

    /// Effective workdir: configured, else the user's home.
    pub fn workdir_or_home(&self) -> String {
        if !self.workdir.is_empty() {
            return self.workdir.clone();
        }
        match self.os.as_str() {
            "windows" => r"C:\Users\cui".to_string(),
            _ => "~".to_string(),
        }
    }
}

/// `[serve]` section — the flagship's listen face (FLEETD-DASHBOARD-001).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServeCfg {
    /// Listen address "host:port" (e.g. "127.0.0.1:3790"). Absent = the
    /// historical loopback default; "0.0.0.0:3790" reaches the dashboard
    /// from the ZT net / LAN. Remote peers get GET-only route policy
    /// (writes stay loopback/token — the control plane is not web-exposed).
    #[serde(default)]
    pub bind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FleetConfig {
    /// Legacy face: one host = one implicit claude executor. Kept for
    /// backward compatibility; `workers` supersedes it (worker-design.md).
    pub hosts: Vec<Host>,
    /// New face: capability-declared workers (2026-11-27 worker-design.md).
    /// Empty = derive from `hosts` (each host becomes one worker with a
    /// single claude executor), so old configs keep working unchanged.
    #[serde(default)]
    pub workers: Vec<Worker>,
    /// Optional `[serve]` section; absent = loopback default bind.
    #[serde(default)]
    pub serve: ServeCfg,
}

/// One declared executor on a worker: a dispatchable capability with its
/// own concurrency budget.
#[derive(Debug, Clone, Deserialize)]
pub struct ExecutorCfg {
    /// claude | codex | opencode | shell | ahk-ui | …
    pub kind: String,
    #[serde(default = "default_executor_concurrency")]
    pub max_concurrent: usize,
    #[serde(default)]
    pub workdir: String,
}

fn default_executor_concurrency() -> usize {
    1
}

/// A worker node: a machine (or logical execution site) with declared
/// capabilities. `channel` routes dispatches: "local", "ssh:<alias>" or
/// "agent" (relay F2 pull; lands with the heartbeat face).
#[derive(Debug, Clone, Deserialize)]
pub struct Worker {
    pub name: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    pub executors: Vec<ExecutorCfg>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_os")]
    pub os: String,
}

fn default_channel() -> String {
    "local".to_string()
}

impl Worker {
    /// Total concurrent capacity across executors (aggregation view).
    pub fn max_concurrent(&self) -> usize {
        self.executors.iter().map(|e| e.max_concurrent).sum()
    }

    /// Does this worker declare `kind` capability?
    pub fn has_executor(&self, kind: &str) -> bool {
        self.executors.iter().any(|e| e.kind == kind)
    }

    /// Derive workers from the legacy hosts list: one worker per host with
    /// a single claude executor. Tags carry the OS so `--requires linux`
    /// keeps working.
    pub fn from_hosts(hosts: &[Host]) -> Vec<Worker> {
        hosts
            .iter()
            .map(|h| Worker {
                name: h.name.clone(),
                channel: if h.ssh == "local" {
                    "local".into()
                } else {
                    format!("ssh:{}", h.ssh)
                },
                executors: vec![ExecutorCfg {
                    kind: "claude".into(),
                    max_concurrent: h.max_concurrent,
                    workdir: h.workdir.clone(),
                }],
                tags: vec![h.os.clone()],
                os: h.os.clone(),
            })
            .collect()
    }
}

impl FleetConfig {
    /// Load from default path ~/.hermes/fleet.toml or an explicit path.
    pub fn load(path: Option<PathBuf>) -> anyhow::Result<Self> {
        let p = path.unwrap_or_else(|| {
            dirs::home_dir()
                .expect("home dir")
                .join(".hermes")
                .join("fleet.toml")
        });
        let raw =
            fs::read_to_string(&p).map_err(|e| anyhow::anyhow!("read {}: {}", p.display(), e))?;
        let cfg: FleetConfig =
            toml::from_str(&raw).map_err(|e| anyhow::anyhow!("parse {}: {}", p.display(), e))?;
        if cfg.hosts.is_empty() {
            anyhow::bail!("no hosts in {}", p.display());
        }
        Ok(cfg)
    }

    /// All workers: declared `workers` if present, else derived from legacy
    /// `hosts` (one claude executor per host). Never empty on valid config.
    pub fn workers(&self) -> Vec<Worker> {
        if self.workers.is_empty() {
            Worker::from_hosts(&self.hosts)
        } else {
            self.workers.clone()
        }
    }

    /// Least-loaded host with remaining capacity.
    /// load = running / max_concurrent; pick min load among those with running < max.
    pub fn pick(&self, running: &[(String, usize)]) -> Option<Host> {
        let mut best: Option<(f32, Host)> = None;
        for h in &self.hosts {
            let cur = running
                .iter()
                .find(|(n, _)| n == &h.name)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            if cur >= h.max_concurrent {
                continue; // at capacity
            }
            let load = if h.max_concurrent == 0 {
                f32::MAX
            } else {
                cur as f32 / h.max_concurrent as f32
            };
            match &best {
                Some((bl, _)) if *bl <= load => {}
                _ => best = Some((load, h.clone())),
            }
        }
        best.map(|(_, h)| h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, max_concurrent: usize, os: &str) -> Host {
        Host {
            name: name.into(),
            ssh: name.into(),
            max_concurrent,
            os: os.into(),
            workdir: String::new(),
            scratch: String::new(),
            cleanup_hours: 0,
        }
    }

    fn cfg(hosts: Vec<Host>) -> FleetConfig {
        FleetConfig {
            hosts,
            workers: Vec::new(),
            serve: ServeCfg::default(),
        }
    }

    #[test]
    fn picks_least_loaded() {
        let c = cfg(vec![host("a", 4, "linux"), host("b", 4, "linux")]);
        let running = vec![("a".into(), 1), ("b".into(), 2)];
        assert_eq!(c.pick(&running).unwrap().name, "a");
    }

    #[test]
    fn skips_at_capacity() {
        let c = cfg(vec![host("a", 1, "linux"), host("b", 4, "linux")]);
        let running = vec![("a".into(), 1), ("b".into(), 0)];
        assert_eq!(c.pick(&running).unwrap().name, "b");
    }

    #[test]
    fn none_when_full() {
        let c = cfg(vec![host("a", 1, "linux"), host("b", 1, "linux")]);
        let running = vec![("a".into(), 1), ("b".into(), 1)];
        assert!(c.pick(&running).is_none());
    }

    #[test]
    fn zero_running_picks_first() {
        let c = cfg(vec![host("a", 2, "linux"), host("b", 2, "linux")]);
        assert_eq!(c.pick(&[]).unwrap().name, "a");
    }

    #[test]
    fn windows_default_scratch() {
        assert_eq!(
            host("w", 1, "windows").scratch_dir(),
            r"C:\Users\cui\fleetd"
        );
        assert_eq!(host("l", 1, "linux").scratch_dir(), "/tmp/fleetd");
    }

    #[test]
    fn explicit_scratch_wins() {
        let mut h = host("w", 1, "windows");
        h.scratch = r"D:\scratch".into();
        assert_eq!(h.scratch_dir(), r"D:\scratch");
    }

    #[test]
    fn workers_derive_from_legacy_hosts() {
        let c = cfg(vec![host("dev", 4, "linux")]);
        let ws = c.workers();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].name, "dev");
        assert_eq!(ws[0].channel, "ssh:dev"); // host() helper sets ssh=name
        assert!(ws[0].has_executor("claude"));
        assert!(!ws[0].has_executor("shell"));
        assert_eq!(ws[0].max_concurrent(), 4);
    }

    #[test]
    fn declared_workers_win_over_hosts() {
        let cfg: FleetConfig = toml::from_str(
            r#"
[[hosts]]
name = "legacy"
ssh = "local"
[[workers]]
name = "w1"
channel = "ssh:imac"
os = "windows"
executors = [
  { kind = "claude", max_concurrent = 2 },
  { kind = "shell", max_concurrent = 8 },
]
tags = ["desktop"]
"#,
        )
        .unwrap();
        let ws = cfg.workers();
        assert_eq!(ws.len(), 1); // declared workers replace derived ones
        assert_eq!(ws[0].name, "w1");
        assert!(ws[0].has_executor("shell"));
        assert_eq!(ws[0].max_concurrent(), 10);
    }
}
