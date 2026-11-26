// Headless dispatch: spawn `claude -p --output-format stream-json` on the
// target host, persist the JSONL event stream, and derive delivery + final
// outcome from protocol events instead of screen scraping.
//
// Design (2026-11-27, user-approved direction): tmux is demoted to a debug
// surface. Unattended dispatch = one headless process per task. The event
// file on disk is the single source of truth:
//   - first `system/init` line  -> Delivered (protocol-level ack)
//   - `assistant` lines         -> live proof of work (watch / heartbeat)
//   - final `result` line       -> deterministic terminal state
//
// Sessions outlive the launching SSH connection: the remote side uses
// setsid + nohup so the process survives disconnects, and `fleetd watch`
// tails the file afterwards from any machine.

use std::process::Command;

/// Where run artifacts live on the target host.
pub const RUN_DIR: &str = "/tmp/fleetd-runs";

/// Escape a string for safe inclusion inside a single-quoted shell argument.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the remote command line that launches one headless claude run.
///
/// Layout on the host:
///   <run_dir>/<run_id>/stream.jsonl   event stream (source of truth)
///   <run_dir>/<run_id>/task.md        the task text as sent
///
/// setsid + nohup detach the process from the SSH session; `echo $?` after
/// the wait records the exit code without a shell wrapper process lingering.
pub fn remote_launch_cmd(run_id: &str, task_file: &str, permission_mode: &str) -> String {
    let dir = format!("{RUN_DIR}/{run_id}");
    // Structure: setsid + nohup detach the run from the SSH session; the
    // whole redirect goes to background as one job. Terminal state comes
    // from the `result` event in stream.jsonl (see poll_state), not from a
    // process exit code — the spawner returns immediately.
    format!(
        "mkdir -p {dir} && cp {tf} {dir}/task.md && {{ setsid nohup claude -p \"$(cat {dir}/task.md)\" \
--output-format stream-json --verbose --permission-mode {pm} \
> {dir}/stream.jsonl 2> {dir}/stderr.log & }} && echo done",
        dir = dir,
        tf = sh_quote(task_file),
        pm = sh_quote(permission_mode),
    )
}

/// Spawn the headless run on `host` over SSH (or locally when host == "local").
/// Returns Ok(()) when the process was launched; delivery confirmation is a
/// separate step (`confirm_start`) so callers can poll instead of blocking.
pub fn launch(host: &str, run_id: &str, task_file: &str, permission_mode: &str) -> std::io::Result<()> {
    let cmd = remote_launch_cmd(run_id, task_file, permission_mode);
    if host == "local" {
        Command::new("bash").arg("-c").arg(&cmd).spawn()?;
    } else {
        Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
            .arg(&cmd)
            .output()?;
    }
    Ok(())
}

/// Read the first `system`/`init` event from the run's stream file.
/// Returns Ok(true) once init has landed (protocol-level delivery ack).
pub fn confirm_start(host: &str, run_id: &str) -> bool {
    let path = format!("{RUN_DIR}/{run_id}/stream.jsonl");
    let out = if host == "local" {
        Command::new("head").args(["-c", "65536", &path]).output()
    } else {
        Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
            .arg(format!("head -c 65536 {} 2>/dev/null", sh_quote(&path)))
            .output()
    };
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.contains("\"subtype\":\"init\"") || l.contains("\"type\":\"assistant\"")),
        Err(_) => false,
    }
}

/// Terminal state of a run, read from the event stream. The `result` line
/// is the protocol-level terminal event; its subtype (success/error) is the
/// honest outcome — no exit-code file needed.
pub enum RunState {
    /// init/assistant events present, no result yet — still running.
    Working,
    /// The run produced a final `result` event (subtype carries success/error).
    Finished(String),
    /// No stream file yet — launch may have failed before creating it.
    NotStarted,
}

pub fn poll_state(host: &str, run_id: &str) -> RunState {
    let path = format!("{RUN_DIR}/{run_id}/stream.jsonl");
    let out = if host == "local" {
        Command::new("head").args(["-c", "1048576", &path]).output()
    } else {
        Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
            .arg(format!("head -c 1048576 {} 2>/dev/null", sh_quote(&path)))
            .output()
    };
    let text = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return RunState::NotStarted,
    };
    let mut saw_init = false;
    for line in text.lines() {
        if line.contains("\"subtype\":\"init\"") || line.contains("\"type\":\"assistant\"") {
            saw_init = true;
        }
        // The result event: {"...","type":"result","subtype":"success"|...}
        if line.contains("\"type\":\"result\"") {
            let subtype = if line.contains("\"subtype\":\"success\"") { "success" } else { "error" };
            return RunState::Finished(subtype.to_string());
        }
    }
    if saw_init { RunState::Working } else { RunState::NotStarted }
}

/// Fetch the last N lines of the event stream (the `fleetd watch` face).
pub fn tail(host: &str, run_id: &str, n: usize) -> String {
    let path = format!("{RUN_DIR}/{run_id}/stream.jsonl");
    let out = if host == "local" {
        Command::new("tail").args(["-n", &n.to_string(), &path]).output()
    } else {
        Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
            .arg(format!("tail -n {} {} 2>/dev/null", n, sh_quote(&path)))
            .output()
    };
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(e) => format!("watch failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_cmd_quotes_task_file() {
        let cmd = remote_launch_cmd("r1", "/tmp/my task.md", "acceptEdits");
        assert!(cmd.contains("'/tmp/my task.md'"));
        assert!(cmd.contains("--permission-mode 'acceptEdits'"));
        assert!(cmd.contains("stream.jsonl"));
        assert!(cmd.contains("setsid nohup"));
    }

    #[test]
    fn remote_cmd_uses_setsid_detach() {
        let cmd = remote_launch_cmd("r2", "/tmp/t.md", "bypassPermissions");
        assert!(cmd.contains("setsid"));
        assert!(cmd.contains("& }")); // format! renders }} as single }
        assert!(cmd.contains("echo done"));
    }
}
