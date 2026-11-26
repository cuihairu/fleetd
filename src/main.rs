mod agent;
mod config;
mod dispatcher;
mod events;
mod ledger;
mod jobs;
mod mcp_server;
mod probe;
mod resources;
mod topology;

// Windows tray (wood gear, rotating frames); compiled only on windows targets.
#[cfg(target_os = "windows")]
mod tray;
mod scheduler;

use anyhow::{bail, Context, Result};
use config::{FleetConfig, Host};
use rusqlite::Connection;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Count running claude processes on a host.
/// Returns Err if the host is unreachable (ssh fails), so callers can skip it.
/// local: pgrep -c claude ; windows: tasklist | findstr /c:"claude.exe"
fn count_claude(h: &Host) -> Result<usize> {
    let out = if h.ssh == "local" {
        if cfg!(target_os = "windows") {
            Command::new("tasklist")
                .arg("/fi")
                .arg("imagename eq claude.exe")
                .output()?
        } else {
            Command::new("pgrep").arg("-c").arg("claude").output()?
        }
    } else if h.os == "windows" {
        Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", &h.ssh])
            .arg("tasklist /fi \"imagename eq claude.exe\"")
            .output()?
    } else {
        Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", &h.ssh])
            .arg("pgrep -c claude || true")
            .output()?
    };

    // ssh non-zero exit = unreachable / auth failure -> treat as Err.
    if h.ssh != "local" && !out.status.success() {
        bail!("host {} unreachable (ssh exit {})", h.name, out.status);
    }

    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // local pgrep prints the count; ssh wraps it; tasklist prints lines.
    if h.ssh == "local" && !cfg!(target_os = "windows") {
        Ok(text.trim().parse().unwrap_or(0))
    } else {
        Ok(text.matches("claude").count())
    }
}

fn db_path() -> PathBuf {
    dirs::home_dir()
        .expect("home")
        .join(".hermes")
        .join("fleetd.db")
}

fn init_db(c: &Connection) -> Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            id TEXT PRIMARY KEY,
            host TEXT NOT NULL,
            repo TEXT NOT NULL,
            task_file TEXT NOT NULL,
            state TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            finished_at INTEGER
        );",
    )?;
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Detect OS of a host without config: probe via ssh (uname or ver).
fn detect_os(h: &Host) -> Result<String> {
    if h.ssh == "local" {
        if cfg!(target_os = "windows") {
            return Ok("windows".into());
        }
        return Ok("linux".into());
    }
    let out = Command::new("ssh")
        .args(["-o", "ConnectTimeout=8", &h.ssh])
        .arg("uname -s 2>/dev/null || echo Windows_NT")
        .output()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    if s.contains("Windows") || s.contains("MINGW") || s.contains("MSYS") {
        Ok("windows".into())
    } else if s.contains("Darwin") {
        Ok("macos".into())
    } else {
        Ok("linux".into())
    }
}

/// Show fleet status: per host running/max, and the pick result.
fn status(cfg: &FleetConfig) -> Result<()> {
    let mut running = Vec::new();
    println!("{:12} {:>8} {:>8} {:>6}", "HOST", "RUNNING", "MAX", "LOAD");
    for h in &cfg.hosts {
        match count_claude(h) {
            Ok(n) => {
                running.push((h.name.clone(), n));
                let load = if h.max_concurrent == 0 {
                    0.0
                } else {
                    n as f32 / h.max_concurrent as f32
                };
                println!(
                    "{:12} {:>8} {:>8} {:>6.2}",
                    h.name, n, h.max_concurrent, load
                );
            }
            Err(_) => {
                println!(
                    "{:12} {:>8} {:>8} {:>6}",
                    h.name, "-", h.max_concurrent, "UNREACHABLE"
                );
            }
        }
    }
    if let Some(p) = cfg.pick(&running) {
        println!("\nLEAST LOADED -> {}", p.name);
    } else {
        println!("\nALL HOSTS AT CAPACITY (or none reachable)");
    }
    Ok(())
}

/// Show hosts with their detected OS / workdir / scratch / cleanup.
fn hosts(cfg: &FleetConfig) -> Result<()> {
    println!(
        "{:12} {:8} {:28} {:24} {:>6}",
        "HOST", "OS", "WORKDIR", "SCRATCH", "CLEAN_H"
    );
    for h in &cfg.hosts {
        let os = detect_os(h).unwrap_or_else(|_| h.os.clone());
        println!(
            "{:12} {:8} {:28} {:24} {:>6}",
            h.name,
            os,
            h.workdir_or_home(),
            h.scratch_dir(),
            h.cleanup_hours
        );
    }
    Ok(())
}

/// Delete scratch entries older than cleanup_hours on each host.
fn cleanup(cfg: &FleetConfig) -> Result<()> {
    for h in &cfg.hosts {
        if h.cleanup_hours == 0 {
            continue;
        }
        let scratch = h.scratch_dir();
        if h.ssh == "local" {
            let _ = cleanup_local(&scratch, h.cleanup_hours);
            println!("{}: cleaned {}", h.name, scratch);
        } else {
            // remote: find old files and remove; best-effort via ssh
            let cmd = if h.os == "windows" {
                format!(
                    "powershell -NoProfile -Command \"Get-ChildItem '{}' -Recurse -File -ErrorAction SilentlyContinue | Where-Object {{ $_.LastWriteTime -lt (Get-Date).AddHours(-{}) }} | Remove-Item -Force -ErrorAction SilentlyContinue\"",
                    scratch, h.cleanup_hours
                )
            } else {
                format!(
                    "find '{}' -type f -mmin +{} -delete 2>/dev/null || true",
                    scratch,
                    h.cleanup_hours * 60
                )
            };
            let _ = Command::new("ssh")
                .args(["-o", "ConnectTimeout=8", &h.ssh])
                .arg(&cmd)
                .status();
            println!("{}: cleaned {} (>{}h)", h.name, scratch, h.cleanup_hours);
        }
    }
    Ok(())
}

fn cleanup_local(scratch: &str, hours: u64) -> Result<()> {
    let dir = Path::new(scratch);
    if !dir.is_dir() {
        return Ok(());
    }
    let cutoff = now().saturating_sub(hours * 3600);
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if mtime < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Dispatch one task file to the least-loaded host, record in ledger.
fn dispatch(cfg: &FleetConfig, task_file: &str, repo: &str) -> Result<()> {
    let mut running = Vec::new();
    for h in &cfg.hosts {
        // skip unreachable hosts entirely; they never get picked
        if let Ok(n) = count_claude(h) {
            running.push((h.name.clone(), n));
        }
    }
    let host = cfg
        .pick(&running)
        .context("no host with capacity; wait for a slot or raise max_concurrent")?;

    let cur = running
        .iter()
        .find(|(n, _)| n == &host.name)
        .map(|(_, c)| *c)
        .unwrap_or(0);
    println!(
        "DISPATCH repo={} -> host={} (running {}/{} load {:.2} os={} workdir={})",
        repo,
        host.name,
        cur,
        host.max_concurrent,
        cur as f32 / host.max_concurrent as f32,
        host.os,
        host.workdir_or_home()
    );

    let task_path = PathBuf::from(task_file);
    let workdir = host.workdir_or_home();
    let scratch = host.scratch_dir();

    // Ensure scratch + workdir exist before scp (prevents "dest open" failures).
    if host.ssh == "local" {
        let _ = std::fs::create_dir_all(&scratch);
        let _ = std::fs::create_dir_all(&workdir);
    } else {
        let mkdir_cmd = if host.os == "windows" {
            format!(
                "cmd /c \"if not exist {} mkdir {} & if not exist {} mkdir {}\"",
                scratch, scratch, workdir, workdir
            )
        } else {
            format!("mkdir -p '{}' '{}'", scratch, workdir)
        };
        let _ = Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", &host.ssh])
            .arg(&mkdir_cmd)
            .status();
    }

    if host.ssh == "local" {
        let cmd = format!(
            "cd '{}' && nohup claude -p --dangerously-skip-permissions --permission-mode bypassPermissions \"$(cat '{}')\" > '{}/fleetd-{}.log 2>&1 &",
            workdir,
            task_path.display(),
            scratch,
            repo
        );
        let status = Command::new("bash").arg("-c").arg(&cmd).status()?;
        if !status.success() {
            bail!("local launch failed");
        }
    } else {
        // copy task file then run in workdir
        let remote_path = format!("{}/fleetd-{}.md", scratch, repo);
        let scp_ok = Command::new("scp")
            .args(["-o", "ConnectTimeout=8", &task_file])
            .arg(format!("{}:{}", host.ssh, remote_path))
            .status()?
            .success();
        if !scp_ok {
            bail!("scp to {} failed", host.ssh);
        }
        let remote_cmd = if host.os == "windows" {
            format!(
                "cmd /c \"cd /d {} && claude -p --dangerously-skip-permissions --permission-mode bypassPermissions \\\"$(type {})\\\" > {}\\fleetd-{}.log 2>&1\"",
                workdir, remote_path, scratch, repo
            )
        } else {
            format!(
                "cd '{}' && nohup claude -p --dangerously-skip-permissions --permission-mode bypassPermissions \"$(cat '{}')\" > '{}/fleetd-{}.log' 2>&1 &",
                workdir, remote_path, scratch, repo
            )
        };
        let status = Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", &host.ssh])
            .arg(&remote_cmd)
            .status()?;
        if !status.success() {
            bail!("remote launch on {} failed", host.ssh);
        }
    }

    // Record in ledger.
    let conn = Connection::open(db_path())?;
    init_db(&conn)?;
    let id = format!("{}-{}", now(), repo);
    conn.execute(
        "INSERT INTO tasks (id, host, repo, task_file, state, created_at) VALUES (?1, ?2, ?3, ?4, 'dispatched', ?5)",
        rusqlite::params![id, host.name, repo, task_path.display().to_string(), now()],
    )?;
    println!("LEDGER id={} host={} state=dispatched", id, host.name);
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cfg_path = args
        .iter()
        .position(|a| a == "--config")
        .map(|i| PathBuf::from(&args[i + 1]));
    // Host config is only required by commands that dispatch across hosts.
    // Ship-local commands (event/mcp/init/job/fleet) must work without it —
    // a remote ship has no ~/.hermes/fleet.toml and still must come up.
    let needs_hosts = matches!(
        args.get(1).map(String::as_str),
        Some("status") | Some("hosts") | Some("cleanup") | Some("dispatch") | None
    );
    let cfg = if needs_hosts {
        Some(FleetConfig::load(cfg_path)?)
    } else {
        None
    };

    match args.get(1).map(String::as_str) {
        // --- user-space job scheduler ---
        Some("job") => {
            let c = ledger::open()?;
            match args.get(2).map(String::as_str) {
                Some("add") => {
                    let name = args.get(3).context("usage: fleetd job add <name> <schedule> <command>")?;
                    let schedule = args.get(4).context("usage: fleetd job add <name> <schedule> <command>")?;
                    let command = args.get(5).context("usage: fleetd job add <name> <schedule> <command>")?;
                    jobs::job_add(&c, name, schedule, command)?;
                }
                Some("list") => jobs::job_list(&c)?,
                Some("run") => {
                    let name = args.get(3).context("usage: fleetd job run <name>")?;
                    jobs::job_run(&c, name)?;
                }
                Some("enable") | Some("disable") => {
                    let on = args.get(2).map(String::as_str) == Some("enable");
                    let name = args.get(3).context("usage: fleetd job enable|disable <name>")?;
                    jobs::job_enable(&c, name, on)?;
                }
                Some("remove") | Some("rm") => {
                    let name = args.get(3).context("usage: fleetd job remove <name>")?;
                    jobs::job_remove(&c, name)?;
                }
                Some("stats") => jobs::job_stats(&c, args.get(3).map(String::as_str))?,
                _ => println!("fleetd job: add <name> <schedule> <cmd> | list | run <name> | enable|disable <name> | remove <name> | stats [name]"),
            }
        }
        // --- fleet aggregation (hub role): pull ship ledgers, whole-fleet view ---
        Some("fleet") => {
            let t = topology::Topology::load()?;
            match args.get(2).map(String::as_str) {
                Some("ships") => {
                    println!("self ship = {}{}", t.ship.name, if t.is_hub() { " (HUB)" } else { "" });
                    for s in &t.fleet.ships {
                        println!("  ship {} via ssh {}", s.name, s.ssh);
                    }
                }
                Some("pull") => {
                    for s in &t.fleet.ships {
                        let path = topology::pull_ship_ledger(&s.ssh, &s.name)?;
                        println!("pulled {} -> {}", s.name, path.display());
                    }
                }
                Some("status") => {
                    // aggregate dispatch rows from self + pulled ship ledgers
                    let mut total_fired: i64 = 0;
                    let mut total_stalled: i64 = 0;
                    let c = ledger::open()?;
                    for r in ledger::progress(&c, 1)? {
                        total_fired += r.fired;
                        total_stalled += r.stalled;
                    }
                    let ships_dir = dirs::home_dir().expect("home").join(".fleetd/ships");
                    if let Ok(entries) = std::fs::read_dir(&ships_dir) {
                        for e in entries.flatten() {
                            let path = e.path();
                            if path.extension().map(|x| x == "db").unwrap_or(false) {
                                if let Ok(rc) = rusqlite::Connection::open(&path) {
                                    for r in ledger::progress(&rc, 1).unwrap_or_default() {
                                        total_fired += r.fired;
                                        total_stalled += r.stalled;
                                    }
                                }
                            }
                        }
                    }
                    println!("FLEET: fired={} stalled={} (self + pulled ships)", total_fired, total_stalled);
                }
                _ => println!("fleetd fleet: ships | pull | status"),
            }
        }
        // --- resource plane (hosts/tools/devices facts) ---
        Some("resources") => {
            let c = ledger::open()?;
            resources::init(&c).context("init resources table")?;
            match args.get(2).map(String::as_str) {
                Some("probe") => {
                    // fleetd resources probe [host...] — default: config hosts
                    let hosts: Vec<String> = match args.get(3) {
                        Some(_) => args[3..].to_vec(),
                        None => cfg.as_ref().context("config required")?.hosts.iter().map(|h| h.name.clone()).collect(),
                    };
                    for h in &hosts {
                        let (t, d) = probe::probe_all(&c, h)?;
                        println!("{h}: tools={t} devices={d}");
                    }
                }
                Some("list") => {
                    let kind = args.get(3).and_then(|k| resources::ResourceKind::from_str(k));
                    let host = args.get(4).map(String::as_str);
                    let rows = resources::query(&c, kind, host)?;
                    println!("{:<10} {:<12} {:<22} {:<10} {:<8} {}", "kind", "host", "name", "state", "holder", "detail");
                    for r in rows {
                        println!("{:<10} {:<12} {:<22} {:<10} {:<8} {}", r.kind, r.host, r.name, r.state, r.holder, r.detail);
                    }
                }
                Some("claim") => {
                    let host = args.get(3).context("usage: fleetd resources claim <host> <device> <holder>")?;
                    let dev = args.get(4).context("usage: fleetd resources claim <host> <device> <holder>")?;
                    let holder = args.get(5).context("usage: fleetd resources claim <host> <device> <holder>")?;
                    let ok = resources::claim_device(&c, host, dev, holder)?;
                    println!("{host}/{dev}: {}", if ok { "claimed" } else { "NOT claimable (busy/offline/missing)" });
                }
                Some("release") => {
                    let host = args.get(3).context("usage: fleetd resources release <host> <device>")?;
                    let dev = args.get(4).context("usage: fleetd resources release <host> <device>")?;
                    let ok = resources::release_device(&c, host, dev)?;
                    println!("{host}/{dev}: {}", if ok { "released" } else { "not found" });
                }
                _ => println!("fleetd resources: probe [host...] | list [kind] [host] | claim <host> <device> <holder> | release <host> <device>"),
            }
        }
        // --- MCP server for coding agents (stdio) ---
        Some("mcp") => {
            // blocks until the client disconnects; needs an async runtime
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(mcp_server::run_stdio())?;
        }
        // --- init / bootstrap ---
        Some("init") => {
            let c = ledger::open()?;
            println!("fleetd ledger initialized at {}", ledger::db_path().display());
            let n: i64 = c.query_row("SELECT COUNT(*) FROM dispatches", [], |r| r.get(0)).unwrap_or(0);
            println!("dispatches={n} seats={}",
                c.query_row("SELECT COUNT(*) FROM seats", [], |r| r.get(0)).unwrap_or(0));
        }
        Some("status") => {
            status(cfg.as_ref().context("config required")?)?;
            // Event-plane progress panel (SQLite-backed, survives restarts)
            let c = ledger::open()?;
            let rows = ledger::progress(&c, 1)?;
            if !rows.is_empty() {
                println!("\n--- 1d dispatch progress (ledger) ---");
                println!(
                    "{:<20} {:>6} {:>8} {:>9} {:>8} {:>8}",
                    "seat", "fired", "done", "inflight", "stall", "partial"
                );
                for r in rows {
                    println!(
                        "{:<20} {:>6} {:>8} {:>9} {:>8} {:>8}",
                        r.seat, r.fired, r.completed, r.inflight, r.stalled, r.partial
                    );
                }
            }
        }
        Some("hosts") => hosts(cfg.as_ref().context("config required")?)?,
        Some("cleanup") => cleanup(cfg.as_ref().context("config required")?)?,
        Some("dispatch") => {
            let task_file = args
                .get(2)
                .context("usage: fleetd dispatch <task.md> <repo>")?;
            let repo = args
                .get(3)
                .context("usage: fleetd dispatch <task.md> <repo>")?;
            dispatch(cfg.as_ref().context("config required")?, task_file, repo)?;
        }
        // --- event plane (completion callbacks) ---
        Some("event") => {
            // fleetd event <seat> — body from stdin (JSON) or minimal fallback
            let seat = args.get(2).context("usage: fleetd event <seat>")?.clone();
            let mut raw = String::new();
            std::io::stdin().read_to_string(&mut raw)?;
            let body = if raw.trim().is_empty() {
                format!(r#"{{"seat":"{seat}"}}"#)
            } else {
                raw
            };
            let verdict = events::ingest(&body)?;
            println!("{seat} -> {verdict}");
        }
        Some("serve") => {
            // Windows: show the rotating wood gear in the tray so the person
            // at the machine can see fleetd is running. --no-tray suppresses it.
            #[cfg(target_os = "windows")]
            let _tray = if args.iter().any(|a| a == "--no-tray") {
                None
            } else {
                crate::tray::spawn()
            };
            // fleetd serve [--port 3790] [--no-scheduler]
            let port = args
                .iter()
                .position(|a| a == "--port")
                .and_then(|i| args.get(i + 1))
                .and_then(|p| p.parse().ok())
                .unwrap_or(3790);
            // background scheduler thread — same daemon, user-space, no root.
            if !args.iter().any(|a| a == "--no-scheduler") {
                std::thread::spawn(|| {
                    crate::scheduler::serve(|| {
                        let c = crate::ledger::open()?;
                        crate::jobs::tick(&c)
                    });
                });
            }
            events::serve(port)?;
        }
        _ => {
            println!("fleetd: status | hosts | cleanup | dispatch <task.md> <repo> | event <seat> | serve");
            status(cfg.as_ref().context("config required")?)?;
        }
    }
    Ok(())
}
