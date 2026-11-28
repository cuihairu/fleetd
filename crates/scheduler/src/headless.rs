// Headless dispatch: spawn one non-interactive agent process per task on the
// target host, persist its JSONL event stream, and derive delivery + final
// outcome from protocol events instead of screen scraping.
//
// Design (2026-11-27, user-approved direction): tmux is demoted to a debug
// surface. Unattended dispatch = one headless process per task. The event
// file on disk is the single source of truth. Scope (user ruling): claude /
// codex / opencode — the three CLIs actually in use; pi and others are out
// of scope until needed.
//
// Sessions outlive the launching SSH connection: the remote side uses
// setsid + nohup so the process survives disconnects, and `fleetd watch`
// tails the file afterwards from any machine.

use std::process::Command;

/// Where run artifacts live on the target host.
pub const RUN_DIR: &str = "/tmp/fleetd-runs";

/// Per-agent headless contract: how to spawn, which stream lines mean
/// "started" and "finished". Verified against the real CLIs on 2026-11-27:
///   claude:   -p --output-format stream-json; init = "subtype":"init";
///             result = "type":"result" (subtype success/error)
///   codex:    exec --json; init = "thread.started"; failure = "turn.failed";
///             success has no explicit event — inferred by quiescence
///   opencode: run --format json; init = "step_start"; no done event —
///             success inferred by quiescence (file stops growing)
pub struct HeadlessSpec {
    pub kind: &'static str,
    /// Launch template; {dir}=run dir, {tf}=quoted task copy path, {pm}=mode.
    pub launch_cmd: &'static str,
    /// Stream marker proving the agent accepted the task (delivery ack).
    pub start_marker: &'static str,
    /// Stream marker for a definite failure (empty = none).
    pub fail_marker: &'static str,
    /// Stream marker for a definite success (empty = infer from quiescence).
    pub ok_marker: &'static str,
}

pub const SPECS: &[HeadlessSpec] = &[
    // claude --bg variant (2026-11-27): the background session is
    // attachable via `claude attach <id>` — the official multi-terminal
    // face. The launch prints the session id; we capture it into
    // session-id for the ledger and `fleetd attach`.
    HeadlessSpec {
        kind: "claude",
        launch_cmd: "mkdir -p {dir} && cp {tf} {dir}/task.md && { setsid nohup claude --bg --permission-mode {pm} \"$(cat {dir}/task.md)\" > {dir}/session-id 2> {dir}/stderr.log & } && echo done",
        start_marker: "",
        fail_marker: "",
        ok_marker: "",
    },
    HeadlessSpec {
        kind: "codex",
        launch_cmd: "mkdir -p {dir} && cp {tf} {dir}/task.md && { setsid nohup codex exec --json \"$(cat {dir}/task.md)\" > {dir}/stream.jsonl 2> {dir}/stderr.log & } && echo done",
        start_marker: "\"thread.started\"",
        fail_marker: "\"turn.failed\"",
        ok_marker: "",
    },
    HeadlessSpec {
        kind: "opencode",
        launch_cmd: "mkdir -p {dir} && cp {tf} {dir}/task.md && { setsid nohup opencode run --format json \"$(cat {dir}/task.md)\" > {dir}/stream.jsonl 2> {dir}/stderr.log & } && echo done",
        start_marker: "\"step_start\"",
        fail_marker: "",
        ok_marker: "",
    },
];

pub fn spec_for(kind: &str) -> Option<&'static HeadlessSpec> {
    SPECS.iter().find(|s| s.kind == kind)
}

/// Escape a string for safe inclusion inside a single-quoted shell argument.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the remote command line that launches one headless run.
///
/// Layout on the host:
///   <run_dir>/<run_id>/stream.jsonl   event stream (source of truth)
///   <run_dir>/<run_id>/task.md        the task text as sent
///   <run_dir>/<run_id>/stderr.log     agent stderr
///
/// setsid + nohup detach the run from the SSH session; the whole redirect
/// goes to background as one job. Terminal state comes from the event
/// stream (see poll_state), not from a process exit code — the spawner
/// returns immediately.
pub fn remote_launch_cmd(
    kind: &str,
    run_id: &str,
    task_file: &str,
    permission_mode: &str,
) -> String {
    let dir = format!("{RUN_DIR}/{run_id}");
    let spec = spec_for(kind).unwrap_or_else(|| &SPECS[0]);
    spec.launch_cmd
        .replace("{dir}", &dir)
        .replace("{tf}", &sh_quote(task_file))
        .replace("{pm}", &sh_quote(permission_mode))
}

/// Spawn the headless run on `host` over SSH (or locally when host == "local").
pub fn launch(
    host: &str,
    kind: &str,
    run_id: &str,
    task_file: &str,
    permission_mode: &str,
) -> std::io::Result<()> {
    let cmd = remote_launch_cmd(kind, run_id, task_file, permission_mode);
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

/// Read the run's stream file head (local or over SSH).
fn stream_head(host: &str, run_id: &str, bytes: usize) -> String {
    let path = format!("{RUN_DIR}/{run_id}/stream.jsonl");
    let out = if host == "local" {
        Command::new("head")
            .args(["-c", &bytes.to_string(), &path])
            .output()
    } else {
        Command::new("ssh")
            .args(["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", host])
            .arg(format!("head -c {bytes} {} 2>/dev/null", sh_quote(&path)))
            .output()
    };
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => String::new(),
    }
}

/// Delivery ack: the agent's start marker appeared in the stream.
pub fn confirm_start(host: &str, kind: &str, run_id: &str) -> bool {
    match spec_for(kind) {
        Some(s) => stream_head(host, run_id, 65536).contains(s.start_marker),
        None => false,
    }
}

/// Terminal state of a run, read from the event stream. Agents with an
/// explicit success marker (claude) report exact success/error; codex
/// reports error on turn.failed; agents without explicit markers
/// (opencode, codex success) infer completion from a quiescence window.
pub enum RunState {
    /// start marker present, no terminal event yet — still running.
    Working,
    /// Terminal state: "success" | "error" | "done"(quiescence-inferred).
    Finished(String),
    /// No stream content yet — launch may have failed before creating it.
    NotStarted,
}

pub fn poll_state(host: &str, kind: &str, run_id: &str) -> RunState {
    poll_state_quiet(host, kind, run_id, 3)
}

/// `quiet_secs` = quiescence window for agents without an explicit done
/// event (0 = skip the wait; tests and fast callers pass 0).
pub fn poll_state_quiet(host: &str, kind: &str, run_id: &str, quiet_secs: u64) -> RunState {
    let spec = match spec_for(kind) {
        Some(s) => s,
        None => return RunState::NotStarted,
    };
    let text = stream_head(host, run_id, 1_048_576);
    if text.is_empty() {
        return RunState::NotStarted;
    }
    let mut saw_start = false;
    for line in text.lines() {
        if line.contains(spec.start_marker) {
            saw_start = true;
        }
        if !spec.fail_marker.is_empty() && line.contains(spec.fail_marker) {
            return RunState::Finished("error".into());
        }
        if !spec.ok_marker.is_empty() && line.contains(spec.ok_marker) {
            let sub = if line.contains("\"subtype\":\"success\"") {
                "success"
            } else {
                "error"
            };
            return RunState::Finished(sub.into());
        }
    }
    if !saw_start {
        return RunState::NotStarted;
    }
    // No explicit ok marker (codex success / opencode): completion is
    // inferred from quiescence — file size unchanged across a short window.
    if spec.ok_marker.is_empty() {
        let size_now = stream_head(host, run_id, usize::MAX).len();
        if quiet_secs > 0 {
            std::thread::sleep(std::time::Duration::from_secs(quiet_secs));
        }
        let size_after = stream_head(host, run_id, usize::MAX).len();
        if size_now == size_after && size_now > 0 {
            return RunState::Finished("done".into());
        }
    }
    RunState::Working
}

/// Fetch the last N lines of the event stream (the `fleetd watch` face).
pub fn tail(host: &str, run_id: &str, n: usize) -> String {
    let path = format!("{RUN_DIR}/{run_id}/stream.jsonl");
    let out = if host == "local" {
        Command::new("tail")
            .args(["-n", &n.to_string(), &path])
            .output()
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
    fn specs_cover_three_agents() {
        assert!(spec_for("claude").is_some());
        assert!(spec_for("codex").is_some());
        assert!(spec_for("opencode").is_some());
        assert!(spec_for("pi").is_none()); // out of scope by user ruling
    }

    #[test]
    fn claude_cmd_quotes_task_file() {
        let cmd = remote_launch_cmd("claude", "r1", "/tmp/my task.md", "acceptEdits");
        assert!(cmd.contains("'/tmp/my task.md'"));
        assert!(cmd.contains("--permission-mode 'acceptEdits'"));
        // --bg mode: session id lands in session-id (attachable face), no
        // stream.jsonl — state comes from `claude agents --json`.
        assert!(cmd.contains("session-id"));
        assert!(cmd.contains("claude --bg"));
        assert!(cmd.contains("setsid nohup"));
    }

    #[test]
    fn codex_cmd_uses_exec_json() {
        let cmd = remote_launch_cmd("codex", "r2", "/tmp/t.md", "unused");
        assert!(cmd.contains("codex exec --json"));
        assert!(!cmd.contains("--permission-mode"));
    }

    #[test]
    fn opencode_cmd_uses_format_json() {
        let cmd = remote_launch_cmd("opencode", "r3", "/tmp/t.md", "unused");
        assert!(cmd.contains("opencode run --format json"));
    }

    #[test]
    fn claude_cmd_uses_setsid_detach() {
        let cmd = remote_launch_cmd("claude", "r4", "/tmp/t.md", "bypassPermissions");
        assert!(cmd.contains("setsid"));
        assert!(cmd.contains("& }")); // format! renders }} as single }
        assert!(cmd.contains("echo done"));
    }
}
