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

#[derive(Debug, Clone, Deserialize)]
pub struct FleetConfig {
    pub hosts: Vec<Host>,
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
        FleetConfig { hosts }
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
}
