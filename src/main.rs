// Headless dispatch channel (2026-11-27): spawn `claude -p` per task, JSONL
// event stream as the source of truth; tmux demoted to debug surface.
// Embedded CPython (PyO3): linux flagship links system libpython; windows seats
// link python3.dll (abi3) and use the bundled runtime/ directory. See pyhost.
// Windows tray: standalone fleetd-tray.exe (crates/fleetd-tray).

use anyhow::{Context, Result};
use fleetd_core::config::{FleetConfig, Host};
// Unified entry aliases after the workspace split (2026-11-27): call
// sites keep their short paths — the split moves boundaries only.
use agent::agent_loop;
use fleetd_core::topology;
use report::{fleet_view, summary};
use store::ledger;
use probe::probe;
use store::resources;
use scheduler::jobs;
#[cfg(target_os = "windows")]
use service::scm_service;
use scheduler::{events, headless, pyhost};
// The `scheduler` submodule must NOT be imported under its own name: it
// shadows the crate, making every `scheduler::jobs` resolve to the module.
use scheduler::scheduler as sched;
use service as svc;
use mcp::mcp_server;
use mcp::tools;
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
        anyhow::bail!("host {} unreachable (ssh exit {})", h.name, out.status);
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
    // Gate check: a disabled seat must never receive dispatches (2026-11-26
    // falcon incident). The gate lives in the ledger keyed by seat name;
    // host name and seat name are the same identifier in this deployment.
    let gate = ledger::open()?;
    let gated: Vec<String> = cfg
        .hosts
        .iter()
        .filter(|h| ledger::is_disabled(&gate, &h.name).unwrap_or(false))
        .map(|h| h.name.clone())
        .collect();
    if !gated.is_empty() {
        println!("DISPATCH gate: skipping disabled seats: {}", gated.join(", "));
    }
    let mut running = Vec::new();
    for h in &cfg.hosts {
        // skip unreachable hosts entirely; they never get picked
        // skip gated (disabled) seats too — absolute, not advisory
        if gated.iter().any(|g| g == &h.name) {
            continue;
        }
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
            anyhow::bail!("local launch failed");
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
            anyhow::bail!("scp to {} failed", host.ssh);
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
            anyhow::bail!("remote launch on {} failed", host.ssh);
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
    // SCM dispatch: when started by the Windows service controller
    // (`sc start FleetD` → binPath has --service), main must hand control to
    // the service dispatcher immediately or the 30 s start clock kills us
    // (error 1053, observed live 2026-11-27).
    #[cfg(target_os = "windows")]
    if std::env::args().any(|a| a == "--service") {
        scm_service::run().map_err(|e| anyhow::anyhow!("service error: {e:?}"))?;
        return Ok(());
    }
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
        Some("status") | Some("hosts") | Some("cleanup") | Some("dispatch-file") | None
    );
    // --- standalone tray process: fleetd tray (pulls /v1/status) ---
    if args.get(1).map(String::as_str) == Some("tray") {
        #[cfg(target_os = "windows")]
        {
            tray::spawn();
            return Ok(());
        }
        #[cfg(not(target_os = "windows"))]
        {
            // macOS/Linux desktop faces are future work (NSStatusItem /
            // AppIndicator over the status socket — docs/vocabulary.md).
            // Headless platforms simply have no tray.
            println!("fleetd tray: not implemented on this platform yet (windows only)");
            return Ok(());
        }
    }
    // --- relay F2 ship-side heartbeat loop: fleetd agent --flagship <url> ---
    if args.get(1).map(String::as_str) == Some("agent") {
        // --hub is a legacy alias for --flagship (rename 2026-11-27; old
        // scripts and muscle memory keep working).
        let flagship = args
            .iter()
            .position(|a| a == "--flagship" || a == "--hub")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        let token = args
            .iter()
            .position(|a| a == "--token")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        let seat = args
            .iter()
            .position(|a| a == "--seat")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str);
        let workdir = args
            .iter()
            .position(|a| a == "--workdir")
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
            .unwrap_or(".")
            .to_string();
        let cfg = agent_loop::load_or_init(flagship, token, seat)?;
        agent_loop::run_loop(cfg, &workdir, "bypassPermissions")?;
        return Ok(());
    }
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
        // --- fleet aggregation (flagship role): pull ship ledgers, whole-fleet view ---
        Some("fleet") => {
            let t = topology::Topology::load()?;
            match args.get(2).map(String::as_str) {
                // Remote overview: fleetd fleet overview <flagship-url> [--follow]
                Some("overview") => {
                    let flagship = args
                        .get(3)
                        .context("usage: fleetd fleet overview <flagship-url> [--follow]")?
                        .clone();
                    let follow = args.iter().any(|a| a == "--follow");
                    fleet_view::run(&flagship, follow).map_err(|e| anyhow::anyhow!(e))?;
                }
                Some("ships") => {
                    println!("self ship = {}{}", t.ship.name, if t.is_flagship() { " (FLAGSHIP)" } else { "" });
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
        Some("seat") => {
            // seat stop|disable|enable|list — absolute control over claude
            // seats (falcon incident 2026-11-26: stop must kill session,
            // processes AND write a gate no auto-start path may bypass).
            let c = ledger::open()?;
            match args.get(2).map(String::as_str) {
                Some("stop") | Some("disable") => {
                    let name = args.get(3).context("usage: fleetd seat stop <name>")?;
                    let hard = args.get(2) == Some(&"stop".to_string());
                    if hard {
                        // Layer 1: session
                        let _ = std::process::Command::new("tmux")
                            .args(["kill-session", "-t", name])
                            .output();
                        // Layer 2: processes living in the seat workdir
                        let _ = std::process::Command::new("pkill")
                            .args(["-f", &format!("workspaces/{name}")])
                            .output();
                    }
                    // Layer 3: gate
                    ledger::set_gate(&c, name, true, "user")?;
                    println!("{name}: {} + gate=disabled", if hard { "session+procs killed" } else { "gate only" });
                }
                Some("enable") => {
                    let name = args.get(3).context("usage: fleetd seat enable <name>")?;
                    ledger::set_gate(&c, name, false, "user")?;
                    println!("{name}: gate cleared (idle)");
                }
                Some("list") => {
                    let mut st = c.prepare(
                        "SELECT seat, status, stopped_by FROM seats ORDER BY seat")?;
                    let rows = st.query_map([], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
                    })?;
                    for r in rows {
                        let (seat, status, by) = r?;
                        let gate = if status == "disabled" { " [GATED]" } else { "" };
                        println!("{seat:20} {status}{gate}{}", by.map(|b| format!(" by={b}")).unwrap_or_default());
                    }
                }
                Some("gate-check") => {
                    let name = args.get(3).context("usage: fleetd seat gate-check <name>")?;
                    println!("{}", if ledger::is_disabled(&c, name)? { "DISABLED" } else { "ALLOWED" });
                }
                Some("fire") => {
                    // seat fire <name> <task-file>: paste a task one-liner into
                    // the seat's claude composer (absorbs fire_seat.sh).
                    let name = args.get(3).context("usage: fleetd seat fire <name> <task.md>")?;
                    let task = args.get(4).context("usage: fleetd seat fire <name> <task.md>")?;
                    if ledger::is_disabled(&c, name)? {
                        anyhow::bail!("{name} is GATED — fleetd seat enable {name} first");
                    }
                    let has = std::process::Command::new("tmux")
                        .args(["has-session", "-t", name])
                        .output()?.status.success();
                    if !has { anyhow::bail!("{name}: NO_SESSION"); }
                    let text = format!("执行 {task} (dispatch {})", chrono::Local::now().format("%Y-%m-%d"));
                    let mut paste = std::process::Command::new("tmux")
                        .args(["load-buffer", "-b", "fleetd-fire", "-"])
                        .stdin(std::process::Stdio::piped())
                        .spawn()?;
                    if let Some(mut stdin) = paste.stdin.take() {
                        use std::io::Write;
                        stdin.write_all(text.as_bytes())?;
                    }
                    let _ = paste.wait();
                    let _ = std::process::Command::new("tmux")
                        .args(["paste-buffer", "-b", "fleetd-fire", "-t", name, "-p"])
                        .output()?;
                    // delivery check: task path visible in the pane
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let cap = std::process::Command::new("tmux")
                        .args(["capture-pane", "-p", "-S", "-30", "-t", name])
                        .output()?;
                    let body = String::from_utf8_lossy(&cap.stdout);
                    let delivered = body.contains(task.as_str());
                    // fold retry (composer fold hides long text)
                    if body.contains("paste again to expand") {
                        let _ = std::process::Command::new("tmux")
                            .args(["paste-buffer", "-b", "fleetd-fire", "-t", name, "-p"])
                            .output()?;
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                    ledger::record_dispatch(&c, name, task, None)?;
                    // composer submit: the paste lands as text; Enter commits it
                    let _ = std::process::Command::new("tmux")
                        .args(["send-keys", "-t", name, "Enter"])
                        .output()?;
                    println!("{name} | {}", if delivered { "DELIVERED" } else { "PASTED (verify fold)" });
                }
                _ => println!("usage: fleetd seat stop|disable|enable|fire|list|gate-check [name]"),
            }
        }
        Some("blocked") => {
            // blocked — list every seat's blocked todo items so the user can
            // pick one, resolve it interactively (attach / provide creds /
            // approve), then unblock. These are exactly the items the
            // scheduler no longer burns cycles on (blocked != open).
            let root = std::path::Path::new("/home/cui/workspaces");
            let mut any = false;
            let mut entries: Vec<(String, i64)> = Vec::new();
            let rd = std::fs::read_dir(root)?;
            for e in rd { let e = e?;
                if !e.file_type()?.is_dir() {
                    continue;
                }
                let seat = e.file_name().to_string_lossy().to_string();
                let todo = e.path().join("todo.md");
                let todo = if todo.exists() {
                    todo
                } else {
                    let t = e.path().join("TODO.md");
                    if t.exists() { t } else { continue }
                };
                let body = std::fs::read_to_string(&todo).unwrap_or_default();
                // open items carrying a blocker marker, same rule as the hook
                let lines: Vec<&str> = body
                    .lines()
                    .filter(|l| {
                        let t = l.trim_start();
                        t.starts_with("- [ ]")
                    })
                    .collect();
                let open = lines.len() as i64;
                if open == 0 {
                    continue;
                }
                let blocked: Vec<&str> = lines
                    .into_iter()
                    .filter(|l| l.contains("BLOCKED") || l.contains("阻塞") || l.contains('⛔'))
                    .collect();
                if blocked.is_empty() {
                    continue;
                }
                entries.push((seat.clone(), blocked.len() as i64));
                println!("== {seat} (open={open}, blocked={}) ==", blocked.len());
                for l in &blocked {
                    let t = l.trim();
                    // first line of the item + any BLOCKED marker, trimmed to 110 cols
                    let snippet: String = t.chars().take(110).collect();
                    println!("  {snippet}");
                }
                any = true;
            }
            if any {
                let total: i64 = entries.iter().map(|(_, b)| b).sum();
                println!(
                    "\n{} seat(s), {} blocked item(s). Resolve one (attach / provide creds / approve), then clear its marker — the scheduler picks it up automatically.",
                    entries.len(),
                    total
                );
            } else {
                println!("no blocked items anywhere");
            }
        }
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
            // Natural-language dispatch: fleetd dispatch "修一下 kode 的登录超时"
            // — resolves repo by fuzzy match over /home/cui/workspaces (name +
            // todo content), picks an idle seat on the resolved repo, fires.
            // No IDs, no file paths — the user speaks, fleetd finds the target.
            let nl = args[2..].join(" ");
            anyhow::ensure!(!nl.is_empty(), "usage: fleetd dispatch \"自然语言任务描述\"");
            let c = ledger::open()?;
            let root = std::path::Path::new("/home/cui/workspaces");
            // score every workspace dir against the description (name tokens + todo hits)
            let mut scored: Vec<(i64, String)> = Vec::new();
            let nl_low = nl.to_lowercase();
            let tokens: Vec<&str> = nl_low
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .filter(|t| t.len() >= 2)
                .collect();
            let rd = std::fs::read_dir(root)?;
            for e in rd { let e = e?;
                if !e.file_type()?.is_dir() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().to_lowercase();
                let mut score = 0i64;
                for t in &tokens {
                    // Exact workspace-name match dominates: a worktree copy
                    // (e.g. pentimento-ui) has a bigger todo and would win on
                    // raw token frequency, but the user naming a repo means
                    // that repo, not its double.
                    if name == *t {
                        score += 1000;
                    } else if name.contains(t) {
                        score += 10;
                    }
                }
                let todo = e.path().join("todo.md");
                let todo = if todo.exists() {
                    todo
                } else {
                    let t2 = e.path().join("TODO.md");
                    if t2.exists() { t2 } else {
                        if score > 0 { scored.push((score, e.file_name().to_string_lossy().to_string())); }
                        continue;
                    }
                };
                let body = std::fs::read_to_string(&todo).unwrap_or_default().to_lowercase();
                for t in &tokens {
                    // count token hits inside the todo body (capped to avoid flood)
                    let hits = body.matches(t).count().min(20) as i64;
                    score += hits;
                }
                if score > 0 {
                    scored.push((score, e.file_name().to_string_lossy().to_string()));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            let (top_score, seat_dir) = scored
                .first()
                .cloned()
                .context("no workspace matched the description; name the repo explicitly")?;
            let runner = scored
                .get(1)
                .filter(|(s, _)| *s >= top_score / 2)
                .map(|(_, n)| n.as_str());
            match runner {
                Some(r) => println!("match: {seat_dir} (also near: {r})"),
                None => println!("match: {seat_dir}"),
            }
            // fire the natural-language task itself (seat fire handles tmux paste)
            let task = nl.clone();
            let has = std::process::Command::new("tmux")
                .args(["has-session", "-t", &seat_dir])
                .output()?.status.success();
            if !has { anyhow::bail!("{seat_dir}: NO_SESSION — cannot deliver"); }
            if ledger::is_disabled(&c, &seat_dir)? {
                anyhow::bail!("{seat_dir} is GATED — fleetd seat enable {seat_dir} first");
            }
            let text = format!("{task} (fleetd dispatch {})", chrono::Local::now().format("%Y-%m-%d %H:%M"));
            let mut paste = std::process::Command::new("tmux")
                .args(["load-buffer", "-b", "fleetd-fire", "-"])
                .stdin(std::process::Stdio::piped())
                .spawn()?;
            if let Some(mut stdin) = paste.stdin.take() {
                use std::io::Write;
                stdin.write_all(text.as_bytes())?;
            }
            let _ = paste.wait();
            let _ = std::process::Command::new("tmux")
                .args(["paste-buffer", "-b", "fleetd-fire", "-t", &seat_dir, "-p"])
                .output()?;
            std::thread::sleep(std::time::Duration::from_secs(2));
            let cap = std::process::Command::new("tmux")
                .args(["capture-pane", "-p", "-S", "-30", "-t", &seat_dir])
                .output()?;
            let body = String::from_utf8_lossy(&cap.stdout);
            let delivered = body.contains(&task);
            let _ = std::process::Command::new("tmux")
                .args(["send-keys", "-t", &seat_dir, "Enter"])
                .output()?;
            ledger::record_dispatch(&c, &seat_dir, &task, None)?;
            println!("{seat_dir} | {}", if delivered { "DELIVERED" } else { "PASTED (verify fold)" });
        }
        Some("dispatch-file") => {
            let task_file = args
                .get(2)
                .context("usage: fleetd dispatch-file <task.md> <repo>")?;
            let repo = args
                .get(3)
                .context("usage: fleetd dispatch-file <task.md> <repo>")?;
            // --headless: new structured channel (2026-11-27). Spawns one
            // `claude -p` process on the picked host, streams JSONL events to
            // /tmp/fleetd-runs/<id>/, outcome read from the event file.
            if args.iter().any(|a| a == "--headless") {
                // --agent claude|codex|opencode (default claude)
                let kind = args
                    .iter()
                    .position(|a| a == "--agent")
                    .and_then(|i| args.get(i + 1))
                    .map(String::as_str)
                    .unwrap_or("claude");
                let _ = repo; // reserved: repo binding for the run workspace
                let run_id = format!(
                    "{}-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    &task_file
                        .rsplit('/')
                        .next()
                        .unwrap_or("task")
                        .trim_end_matches(".md")
                );
                let host = "local"; // step 1: local runs; remote pick lands with step 2
                headless::launch(host, kind, &run_id, task_file, "bypassPermissions")
                    .context("headless launch failed")?;
                // Wait for the protocol-level ack (agent start marker) before
                // declaring delivery — no screen scraping.
                let mut delivered = false;
                for _ in 0..30 {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if headless::confirm_start(host, kind, &run_id) {
                        delivered = true;
                        break;
                    }
                }
                let ledger_c = ledger::open()?;
                ledger::record_dispatch(&ledger_c, &format!("headless-{kind}"), &run_id, None)?;
                println!(
                    "HEADLESS {kind}/{run_id} | {} | stream: /tmp/fleetd-runs/{run_id}/stream.jsonl",
                    if delivered { "DELIVERED" } else { "NO-INIT (poll: fleetd watch)" }
                );
            } else {
                dispatch(cfg.as_ref().context("config required")?, task_file, repo)?;
            }
        }
        // --- relay F2: queue pull-dispatch + agent liveness ---
        Some("relay") => {
            match args.get(2).map(String::as_str) {
                Some("queue") => {
                    let seat = args.get(3).context("usage: fleetd relay queue <seat> <task.md> <repo>")?;
                    let task_file = args.get(4).context("usage: fleetd relay queue <seat> <task.md> <repo>")?;
                    let repo = args.get(5).context("usage: fleetd relay queue <seat> <task.md> <repo>")?;
                    let id = relay::forward::queue_pull(seat, task_file, repo)?;
                    println!("queued pull id={id} seat={seat} (delivered on next heartbeat)");
                }
                Some("alive") => {
                    for (seat, ts) in relay::relay::alive_since(15)? {
                        println!("{seat} last-heartbeat {ts}s-epoch");
                    }
                }
                _ => println!("usage: fleetd relay queue <seat> <task.md> <repo> | relay alive"),
            }
        }
        // --- self service registration (per-OS native mechanism) ---
        Some("service") => {
            let exe = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "fleetd".into());
            match args.get(2).map(String::as_str) {
                Some("start") | Some("stop") => {
                    let verb = args.get(2).map(String::as_str).unwrap();
                    match service::detect() {
                        service::Platform::Windows => {
                            let r = Command::new("cmd")
                                .args(["/c", &format!("sc {} FleetD", verb)])
                                .output()?;
                            println!("windows: sc {} FleetD → {}", verb, r.status.success());
                        }
                        service::Platform::Linux => {
                            let r = Command::new("systemctl")
                                .args(["--user", verb, "fleetd"])
                                .output()?;
                            println!("linux: systemctl --user {} → {}", verb, r.status.success());
                        }
                        service::Platform::Macos => {
                            let verb_launch = if verb == "start" { "load" } else { "unload" };
                            let plist = format!(
                                "{}/Library/LaunchAgents/ai.fleetd.plist",
                                std::env::var("HOME").unwrap_or_default()
                            );
                            let r = Command::new("launchctl").args([verb_launch, &plist]).output()?;
                            println!("macos: launchctl {} → {}", verb_launch, r.status.success());
                        }
                    }
                }
                Some("status") => {
                    match service::detect() {
                        service::Platform::Windows => {
                            let r = Command::new("cmd")
                                .args(["/c", "sc query FleetD"])
                                .output()?;
                            print!("{}", String::from_utf8_lossy(&r.stdout));
                        }
                        _ => {
                            let r = Command::new("systemctl")
                                .args(["--user", "status", "fleetd"])
                                .output()?;
                            print!("{}", String::from_utf8_lossy(&r.stdout));
                        }
                    }
                }
                Some("install") | Some("uninstall") => {
                    let uninstall = args.get(2).map(String::as_str) == Some("uninstall");
                    match service::detect() {
                        service::Platform::Linux => {
                            let unit_path = format!(
                                "{}/.config/systemd/user/fleetd.service",
                                std::env::var("HOME").unwrap_or_default()
                            );
                            if uninstall {
                                let _ = Command::new("systemctl").args(["--user", "stop", "fleetd"]).output();
                                let _ = std::fs::remove_file(&unit_path);
                                let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
                                println!("linux: unit removed");
                            } else {
                                std::fs::create_dir_all(
                                    std::path::Path::new(&unit_path).parent().unwrap()
                                )?;
                                std::fs::write(&unit_path, service::systemd_unit(&exe))?;
                                let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
                                let en = Command::new("systemctl")
                                    .args(["--user", "enable", "--now", "fleetd"])
                                    .output()?;
                                println!(
                                    "linux: unit written to {unit_path}, enable --now = {}",
                                    en.status.success()
                                );
                            }
                        }
                        service::Platform::Windows => {
                            // Real SCM service + a separate tray ONLOGON task.
                            if uninstall {
                                let _ = Command::new("cmd").args(["/c", "sc stop FleetD"]).output();
                                let _ = Command::new("cmd").args(["/c", "sc delete FleetD"]).output();
                                let _ = Command::new("schtasks")
                                    .args(["/Delete", "/TN", "FleetDTray", "/F"])
                                    .output();
                                println!("windows: service deleted");
                            } else {
                                for c in service::windows_commands(&exe) {
                                    let _ = Command::new("cmd").args(["/c", &c]).output();
                                }
                                println!("windows: SCM service created+started, tray ONLOGON task registered");
                            }
                        }
                        service::Platform::Macos => {
                            let plist = format!(
                                "{}/Library/LaunchAgents/ai.fleetd.plist",
                                std::env::var("HOME").unwrap_or_default()
                            );
                            if uninstall {
                                let _ = Command::new("launchctl").args(["unload", &plist]).output();
                                let _ = std::fs::remove_file(&plist);
                                println!("macos: plist unloaded+removed");
                            } else {
                                std::fs::create_dir_all(
                                    std::path::Path::new(&plist).parent().unwrap()
                                )?;
                                std::fs::write(&plist, service::launchd_plist(&exe))?;
                                let l = Command::new("launchctl").args(["load", &plist]).output()?;
                                println!("macos: plist loaded = {}", l.status.success());
                            }
                        }
                    }
                }
                _ => {
                    println!("usage: fleetd service install|uninstall|start|stop|status");
                }
            }
        }
        // --- agent toolchain lifecycle: install/inventory across hosts ---
        Some("tools") => {
            match args.get(2).map(String::as_str) {
                Some("list") => {
                    let hosts = ["local", "work-runner", "imac"];
                    for t in [tools::Tool::Claude, tools::Tool::Codex, tools::Tool::Opencode] {
                        println!("{}:", t.name());
                        for (h, v) in tools::inventory(&hosts, t) {
                            println!("  {h:<14} {v}");
                        }
                    }
                }
                Some("install") => {
                    let agent = args.get(3).context("usage: fleetd tools install <claude|codex|opencode> [--host <alias>]")?;
                    let tool = tools::Tool::from_str(agent)
                        .context("unknown agent; supported: claude codex opencode")?;
                    let host = args
                        .iter()
                        .position(|a| a == "--host")
                        .and_then(|i| args.get(i + 1))
                        .map(String::as_str)
                        .unwrap_or("local");
                    println!("installing {} on {host} (official installer)...", tool.name());
                    match tools::install(host, tool) {
                        Ok(v) => println!("OK: {} now {v}", tool.name()),
                        Err(e) => anyhow::bail!("install failed: {e}"),
                    }
                }
                _ => {
                    println!("usage: fleetd tools list | tools install <agent> [--host <alias>]");
                }
            }
        }
        // --- headless run observability: tail the event stream ---
        Some("watch") => {
            let run_id = args.get(2).context("usage: fleetd watch <run-id> [--host <alias>] [lines]")?;
            let host = args
                .iter()
                .position(|a| a == "--host")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str)
                .unwrap_or("local");
            let lines: usize = args
                .get(3)
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            let kind = args
                .iter()
                .position(|a| a == "--agent")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str)
                .unwrap_or("claude");
            match headless::poll_state(host, kind, run_id) {
                headless::RunState::Finished(sub) => println!("state: FINISHED result={sub}"),
                headless::RunState::Working => println!("state: WORKING"),
                headless::RunState::NotStarted => println!("state: NOT-STARTED"),
            }
            print!("{}", headless::tail(host, run_id, lines));
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
            // Tray is a SEPARATE process now (`fleetd tray`, 2026-11-27 用户令
            // 「tray 和 serve 拆开」): serve never spawns it; the OS service
            // face (FleetDTray ONLOGON / launchd agent) launches `fleetd tray`
            // in the interactive session. Serve headless always.
            run_serve_cli(&args)?;
        }
        _ => {
            println!("fleetd: dispatch <自然语言任务> | status | hosts | cleanup | dispatch-file <task.md> <repo> | event <seat> | blocked | seat ... | serve");
            status(cfg.as_ref().context("config required")?)?;
        }
    }
    Ok(())
}


/// CLI serve path: flags respected (`--port`, `--public`, `--no-scheduler`).
fn run_serve_cli(args: &[String]) -> Result<()> {
    let port = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(3790u16);
    if !args.iter().any(|a| a == "--no-scheduler") {
        std::thread::spawn(|| {
            sched::serve(|| {
                let c = store::ledger::open()?;
                scheduler::jobs::tick(&c)
            });
        });
    }
    // 通信面(2026-11-27 钦定):本机默认走 UNIX socket(~/.fleetd/flagship.sock,
    // Windows 同 AF_UNIX);--http 显式开启才起 TCP HTTP 面(跨机 relay/面板)。
    let http = args.iter().any(|a| a == "--http");
    if http {
        let bind = if args.iter().any(|a| a == "--public") { "0.0.0.0" } else { "127.0.0.1" };
        events::serve_on(bind, port)?;
    } else {
        events::serve_on_unix(&scheduler::events::flagship_socket_path())?;
    }
    Ok(())
}

/// SCM path (windows, `--service` arg from `sc create`): fixed flags —
/// scheduler on, loopback bind (the service IS the flagship).
#[cfg(target_os = "windows")]
pub fn run_serve_from_service() -> anyhow::Result<()> {
    let args = vec![
        "fleetd".to_string(),
        "serve".to_string(),
        "--port".to_string(),
        "3790".to_string(),
    ];
    run_serve_cli(&args)
}

#[cfg(not(target_os = "windows"))]
pub fn run_serve_from_service() -> anyhow::Result<()> {
    anyhow::bail!("SCM service face is windows-only")
}
