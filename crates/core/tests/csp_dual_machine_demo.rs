//! Live dual-machine demo (FLEETD-SCHED-MACHINE-001 acceptance 2/4).
//!
//! One Job lands on the LocalMachine (this host's tmux), one on the
//! SshMachine (work-runner), both into REAL claude sessions; while both
//! slots are busy the next Job reroutes (mailbox/load), and a cancel stops
//! its seat for real. Run it explicitly:
//!   cargo test -p fleetd-core --test csp_dual_machine_demo -- --ignored --nocapture
//!
//! Requires: local tmux + `claude` in PATH; `ssh work-runner` BatchMode ok.
//! Not part of CI — it starts real processes and burns real tokens.

use fleetd_core::machine::{Caps, Job, JobState, Machine, MailboxMachine, Pool, Requires};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

fn sh(cmd: &str) -> String {
    String::from_utf8_lossy(
        &Command::new("bash")
            .arg("-c")
            .arg(cmd)
            .output()
            .expect("spawn bash")
            .stdout,
    )
    .trim()
    .to_string()
}

fn tmux_pane(host: &str, seat: &str) -> String {
    if host == "local" {
        sh(&format!("tmux capture-pane -p -t {seat} 2>/dev/null"))
    } else {
        sh(&format!(
            "ssh -o BatchMode=yes -o ConnectTimeout=8 {host} 'tmux capture-pane -p -t {seat} 2>/dev/null'"
        ))
    }
}

fn session_up(host: &str, seat: &str) -> bool {
    let pane = tmux_pane(host, seat);
    !pane.is_empty() && !pane.contains("no server") && !pane.contains("can't find")
}

/// Poll until the machine reports the job in `want`.
async fn wait_state(m: &fleetd_core::machine::MailboxMachine, id: &str, want: JobState) {
    for _ in 0..300 {
        if m.status(id).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "job {id} never reached {want:?} (is {:?})",
        m.status(id).await
    );
}

#[tokio::test]
#[ignore = "live demo: real tmux sessions + ssh work-runner + real tokens"]
async fn dual_machine_real_demo() {
    let local_seat = "csp-demo-local";
    let remote_seat = "csp-demo-remote";
    let remote_host = "work-runner";

    // --- setup: one real claude session per machine ---
    // Folders live under the trusted workspaces roots on both hosts. A
    // fresh folder would show the trust dialog with "No, exit" preselected,
    // and the delivery's Enter would confirm the exit (observed live); the
    // wait loop below also dismisses the dialog if it ever appears.
    sh(&format!(
        "mkdir -p /home/cui/workspaces/{local_seat}; tmux kill-session -t {local_seat} 2>/dev/null; \
         tmux new-session -d -s {local_seat} -c /home/cui/workspaces/{local_seat} -x 200 -y 40 \
         'bash -lc \"claude --dangerously-skip-permissions\"'"
    ));
    sh(&format!(
        "ssh -o BatchMode=yes -o ConnectTimeout=8 {remote_host} \
         'mkdir -p /root/workspaces/{remote_seat}; tmux kill-session -t {remote_seat} 2>/dev/null; \
          tmux new-session -d -s {remote_seat} -c /root/workspaces/{remote_seat} -x 200 -y 40 \
          \"bash -lc \\\"claude --dangerously-skip-permissions\\\"\"'"
    ));
    println!("waiting for both claude TUIs to come up...");
    // "TUI up" = the bypass-permissions footer is showing. A bare non-empty
    // pane is NOT enough — the fresh-folder trust dialog also fills the pane
    // (and its default action exits claude).
    let tui_up = |host: &str, seat: &str| -> bool {
        let pane = tmux_pane(host, seat);
        session_up(host, seat) && pane.contains("bypass permissions")
    };
    let mut up = false;
    for _ in 0..60 {
        // Dismiss the fresh-folder trust dialog when it appears: Down moves
        // to "Yes, I trust this folder", Enter confirms.
        for (host, seat) in [("local", local_seat), (remote_host, remote_seat)] {
            let pane = tmux_pane(host, seat);
            if pane.contains("Yes, I trust this folder") {
                let keys = format!("tmux send-keys -t {seat} Down Enter");
                if host == "local" {
                    sh(&keys);
                } else {
                    sh(&format!("ssh -o BatchMode=yes {host} '{keys}'"));
                }
            }
        }
        up = tui_up("local", local_seat) && tui_up(remote_host, remote_seat);
        if up {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(up, "claude sessions did not come up on both machines");

    // --- the pool: LocalMachine + SshMachine, one slot each ---
    let caps = |os: &str| Caps {
        executors: vec!["claude".into()],
        tags: vec![os.into()],
        os: os.into(),
    };
    let local = MailboxMachine::local("local", caps("linux"), 1);
    let remote = MailboxMachine::ssh(remote_host, caps("linux"), 1);
    let pool = Pool::new(vec![
        local.clone() as Arc<dyn Machine>,
        remote.clone() as Arc<dyn Machine>,
    ]);

    let task = |n: u32| {
        let marker = format!("CSP-DEMO-J{n}");
        // Seats follow the deterministic routing (j1/j3 -> local queue,
        // j2/j4 -> remote queue), matching the one-slot machines.
        Job {
            id: format!("demo-{n}"),
            seat: if n == 1 || n == 3 {
                local_seat.to_string()
            } else {
                remote_seat.to_string()
            },
            tag: marker.clone(),
            text: format!(
                "{marker}：这是 fleetd CSP 抽象的真机演示投递。只需回答一句「{marker} 已收到」\
                 并报告你所在目录，不要执行任何修改。"
            ),
            requires: Requires {
                executor: Some("claude".into()),
                ..Default::default()
            },
            ..Default::default()
        }
    };

    // --- j1: first send wins the local slot ---
    let h1 = pool.route(task(1)).await.expect("route j1");
    println!("j1 -> {}", h1.machine_id);
    assert_eq!(h1.machine_id, "local");
    // Let the mailbox drain j1 into its slot consumer (real delivery holds
    // the slot ~3.5 s — the yield also drives the current-thread drainer).
    wait_state(&local, "demo-1", JobState::Running).await;
    // --- j2: local slot busy -> load reroute to the ssh machine ---
    let h2 = pool.route(task(2)).await.expect("route j2");
    println!("j2 -> {} (reroute under load)", h2.machine_id);
    assert_eq!(h2.machine_id, remote_host);
    wait_state(&remote, "demo-2", JobState::Running).await;
    // --- j3/j4: fill both mailboxes (queued) ---
    let h3 = pool.route(task(3)).await.expect("route j3");
    let h4 = pool.route(task(4)).await.expect("route j4");
    println!(
        "j3 -> {} (mailbox queue), j4 -> {} (mailbox queue)",
        h3.machine_id, h4.machine_id
    );
    // --- j5: both mailboxes full -> backpressure reported ---
    let j5 = pool.route(task(5)).await;
    println!(
        "j5 -> {:?} (backpressure, as designed)",
        j5.err().map(|e| e.to_string())
    );

    // Both real sessions receive their task book: markers visible in panes.
    let mut seen = false;
    for _ in 0..150 {
        seen = tmux_pane("local", local_seat).contains("CSP-DEMO-J1")
            && tmux_pane(remote_host, remote_seat).contains("CSP-DEMO-J2");
        if seen {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(seen, "task text never surfaced in both panes");
    println!("VERIFIED: both machines received their Job (pane markers seen)");

    // --- cancel chain: stop j1's seat for real (seat stop layers 1+2) ---
    pool.cancel("demo-1").await.expect("cancel j1");
    let mut cancelled = false;
    for _ in 0..100 {
        if matches!(local.status("demo-1").await, JobState::Cancelled) {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(cancelled, "j1 must read Cancelled");
    let mut gone = false;
    for _ in 0..50 {
        if !session_up("local", local_seat) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(gone, "cancelled seat's tmux session must be dead");
    println!("VERIFIED: cancel stopped the seat (session killed), state=cancelled");

    // --- teardown ---
    sh(&format!(
        "tmux kill-session -t {local_seat} 2>/dev/null; rm -rf /home/cui/workspaces/{local_seat}"
    ));
    sh(&format!(
        "ssh -o BatchMode=yes {remote_host} 'tmux kill-session -t {remote_seat} 2>/dev/null; rm -rf /root/workspaces/{remote_seat}'"
    ));
    println!("DEMO COMPLETE");
}
