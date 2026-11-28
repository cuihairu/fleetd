//! Event ingestion surface: the control plane's completion-callback endpoint.
//!
//! Two entry points (same handler body):
//! - `fleetd event` — CLI fallback for hooks that prefer curl-to-self
//! - `fleetd serve` — long-running HTTP listener so Stop hooks stay dumb
//!
//! The Stop hook never classifies anything itself: it only reports
//! {seat, head, open, dirty}; this module compares against the dispatch
//! baseline and records the verdict (completed / partial / stalled /
//! no_progress / untracked). The tmux death hooks (FLEETD-TMUXHOOK-001)
//! post `kind=pane_died` on the same face — observational: recorded, never
//! closes a dispatch; the presence reconcile layer owns death semantics.

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(windows)]
use std::os::windows::net::UnixListener;

use fleetd_core::verdict::SeatEvent;
use store::ledger;

const MAX_BODY: usize = 256 * 1024;

/// Parse a JSON seat event body. Kept strict on the fields the verdict needs.
pub fn parse_body(raw: &str) -> Result<SeatEvent> {
    let ev: SeatEvent = serde_json::from_str(raw).context("parse event JSON")?;
    if ev.seat.is_empty() || ev.seat.contains('/') || ev.seat.contains('\\') {
        bail!("invalid seat name: {}", ev.seat);
    }
    Ok(ev)
}

/// Ingest one event into the ledger and return the verdict.
pub fn ingest(raw: &str) -> Result<String> {
    let ev = parse_body(raw)?;
    let c = ledger::open()?;
    let verdict = ledger::ingest(&c, &ev)?;
    Ok(verdict.as_str().to_string())
}

/// Ingest + run the auto-dispatch decision (serve path only).
/// Returns (verdict, decision_summary).
pub fn ingest_and_dispatch(raw: &str, auto: bool) -> Result<(String, String)> {
    let ev = parse_body(raw)?;
    let c = ledger::open()?;
    let verdict = ledger::ingest(&c, &ev)?;
    if !auto {
        return Ok((verdict.as_str().into(), "auto disabled".into()));
    }
    let v = verdict; // Copy since 2026-11-27 derive
    let open = ev.open.unwrap_or(0);
    let blocked = ev.blocked.unwrap_or(0);
    let dirty = ev.dirty.unwrap_or(0);
    // No queued tag source yet (queue lands later); nudge todo when open.
    let decision =
        fleetd_core::dispatcher::decide(v, open, blocked, None, dirty, true, true, false);
    let summary = match &decision {
        fleetd_core::dispatcher::Decision::Noop { reason } => format!("noop: {reason}"),
        _ => format!("would: {:?}", decision),
    };
    Ok((verdict.as_str().into(), summary))
}

/// Minimal HTTP/1.1 handler — no framework. Reads one request, answers JSON.
/// Peer-address abstraction: TCP streams expose a peer IP for the loopback
/// gate; UDS carries none — the socket file itself is the local boundary.
trait PeerAddr {
    fn peer_addr(&self) -> std::io::Result<Option<std::net::SocketAddr>>;
}

impl PeerAddr for std::net::TcpStream {
    fn peer_addr(&self) -> std::io::Result<Option<std::net::SocketAddr>> {
        std::net::TcpStream::peer_addr(self).map(Some)
    }
}

#[cfg(unix)]
impl PeerAddr for std::os::unix::net::UnixStream {
    fn peer_addr(&self) -> std::io::Result<Option<std::net::SocketAddr>> {
        Ok(None)
    }
}

#[cfg(windows)]
impl PeerAddr for std::os::windows::net::UnixStream {
    fn peer_addr(&self) -> std::io::Result<Option<std::net::SocketAddr>> {
        Ok(None)
    }
}

/// 泛化到 Read+Write:HTTP 解析只做读写,UnixListener/插座面同链路复用
/// (2026-11-27 UNIX socket 默认面拆分)。
pub fn handle_stream<S: Read + Write + PeerAddr>(
    stream: &mut S,
    relay_token: Option<&str>,
) -> Result<()> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            header_end = pos;
            break;
        }
        if buf.len() > MAX_BODY {
            bail!("request too large");
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let content_len: usize = lines
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.trim().parse().unwrap_or(0))
        .next()
        .unwrap_or(0);

    let body_start = header_end + 4;
    while buf.len() < body_start + content_len {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = String::from_utf8_lossy(&buf[body_start..(body_start + content_len).min(buf.len())])
        .to_string();

    // Relay-face auth (relay-design F5): when a token is configured, the
    // fleet/summary GET and the heartbeat POST require it; /v1/events stays
    // open for the loopback hook path (hooks run on the flagship itself).
    let token_ok = relay_token.is_none()
        || request_line.contains("/v1/events")
        || head.lines().any(|l| {
            l.to_ascii_lowercase().starts_with("authorization:")
                && l.contains(relay_token.unwrap_or(""))
        });

    let (status, payload) = match request_line.split_whitespace().next() {
        Some("POST") if request_line.contains("/v1/events") => match ingest(&body) {
            Ok(v) => (200, format!(r#"{{"verdict":"{}"}}"#, v)),
            Err(e) => (400, format!(r#"{{"error":"{}"}}"#, e)),
        },
        // Agent heartbeat (relay F2): ships behind NAT/unstable networks
        // POST {seat, head, dirty} on an outbound timer; the response
        // carries pending pull-dispatches. Auth: relay token (F5).
        Some("POST") if request_line.contains("/v1/agent/heartbeat") => {
            if !token_ok {
                (403, r#"{"error":"forbidden"}"#.to_string())
            } else {
                match relay::relay::heartbeat(&body) {
                    Ok(reply) => match serde_json::to_string(&reply) {
                        Ok(j) => (200, j),
                        Err(e) => (500, format!(r#"{{"error":"{}"}}"#, e)),
                    },
                    Err(e) => (400, format!(r#"{{"error":"{}"}}"#, e)),
                }
            }
        }
        // Read-only fleet summary for remote dashboards (2026-11-27).
        // Loopback by default; token required when the relay face is on.
        Some("GET") if request_line.contains("/v1/fleet/summary") => {
            if !token_ok {
                (403, r#"{"error":"forbidden"}"#.to_string())
            } else {
                match report::summary::build(7) {
                    Ok(s) => match serde_json::to_string(&s) {
                        Ok(j) => (200, j),
                        Err(e) => (500, format!(r#"{{"error":"{}"}}"#, e)),
                    },
                    Err(e) => (500, format!(r#"{{"error":"{}"}}"#, e)),
                }
            }
        }
        // Local status face for the tray (2026-11-27 用户令「tray 尽量用
        // UNIX socket 通信」): loopback-only, no token — the tray pulls
        // version/port/in-flight from here each tick instead of sharing
        // process memory, so the tray keeps working when serve moves to
        // another process. Non-loopback peers get 403.
        Some("GET") if request_line.contains("/v1/status") => {
            // Loopback gate: TCP peers must be loopback; UDS has no peer
            // address — the socket file itself is the local boundary, so a
            // successful connect already proves same-machine.
            let peer = PeerAddr::peer_addr(stream)
                .ok()
                .flatten()
                .map(|a| a.ip().is_loopback())
                .unwrap_or(true);
            if !peer {
                (403, r#"{"error":"loopback only"}"#.to_string())
            } else {
                let busy = store::ledger::in_flight_count();
                (
                    200,
                    format!(
                        r#"{{"version":"{}","in_flight":{}}}"#,
                        env!("CARGO_PKG_VERSION"),
                        busy
                    ),
                )
            }
        }
        // Presence face (FLEETD-PRESENCE-001): the reconciled liveness of
        // every tmux seat, as written by the 60 s presence watch. Loopback
        // gate only (same as /v1/status; no --public face was chartered).
        // `?state=<name>` narrows to one state, e.g. ?state=blocked-style
        // dashboards ask for ?state=idle. `last_commit` = newest lifecycle
        // event ts for the seat from the ledger.
        Some("GET")
            if request_line.contains("/v1/fleet")
                && !request_line.contains("/v1/fleet/summary") =>
        {
            let peer = PeerAddr::peer_addr(stream)
                .ok()
                .flatten()
                .map(|a| a.ip().is_loopback())
                .unwrap_or(true);
            if !peer {
                (403, r#"{"error":"loopback only"}"#.to_string())
            } else {
                let state_filter = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .split_once('?')
                    .map(|(_, q)| {
                        q.split('&')
                            .find_map(|kv| kv.strip_prefix("state="))
                            .unwrap_or("")
                            .to_string()
                    })
                    .unwrap_or_default();
                match store::ledger::open()
                    .and_then(|c| store::ledger::presence_rows(&c, &state_filter))
                {
                    Ok(rows) => {
                        let arr: Vec<serde_json::Value> = rows
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "seat": r.seat,
                                    "state": r.state,
                                    "open": r.open,
                                    "blocked": r.blocked,
                                    "last_event_ts": r.last_event_ts,
                                    "last_commit": r.last_commit,
                                })
                            })
                            .collect();
                        match serde_json::to_string(&arr) {
                            Ok(j) => (200, j),
                            Err(e) => (500, format!(r#"{{"error":"{}"}}"#, e)),
                        }
                    }
                    Err(e) => (500, format!(r#"{{"error":"{}"}}"#, e)),
                }
            }
        }
        _ => (404, r#"{"error":"not found"}"#.to_string()),
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(resp.as_bytes())?;
    Ok(())
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Serve events forever on `bind_addr`. Blocks the caller. Pass "127.0.0.1"
/// for the loopback-only default, or "0.0.0.0" when agents reach the flagship
/// over the network (fleetd agent pull mode / dev public relay).
/// 本机 socket 面(2026-11-27 默认):AF_UNIX,路径 ~/.fleetd/flagship.sock。
/// Windows 10 1803+ 原生 AF_UNIX(std::os::windows::net::UnixListener 同 API)。
/// 每次 bind 前清残留 socket 文件。
#[cfg(unix)]
pub fn serve_on_unix(sock_path: &std::path::Path) -> Result<()> {
    let relay_token = std::env::var("FLEETD_RELAY_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if let Some(dir) = sock_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| "create socket dir")?;
    }
    let _ = std::fs::remove_file(sock_path); // stale socket from a crashed run
    let listener =
        UnixListener::bind(sock_path).with_context(|| format!("bind unix socket {sock_path:?}"))?;
    eprintln!("fleetd flagship socket: {sock_path:?} (local tray face)");
    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                if let Err(e) = handle_stream(&mut s, relay_token.as_deref()) {
                    eprintln!("event handling error: {e:#}");
                }
            }
            Err(e) => eprintln!("accept error: {e:#}"),
        }
    }
    Ok(())
}

/// Windows 侧 AF_UNIX 面(签名一致;Windows 10 1803+ 支持)。
#[cfg(windows)]
pub fn serve_on_unix(sock_path: &std::path::Path) -> Result<()> {
    let relay_token = std::env::var("FLEETD_RELAY_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if let Some(dir) = sock_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| "create socket dir")?;
    }
    let _ = std::fs::remove_file(sock_path);
    let listener =
        UnixListener::bind(sock_path).with_context(|| format!("bind unix socket {sock_path:?}"))?;
    eprintln!("fleetd flagship socket: {sock_path:?} (local tray face)");
    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                if let Err(e) = handle_stream(&mut s, relay_token.as_deref()) {
                    eprintln!("event handling error: {e:#}");
                }
            }
            Err(e) => eprintln!("accept error: {e:#}"),
        }
    }
    Ok(())
}

/// Socket 路径约定:~/.fleetd/flagship.sock(全平台同一规则)。
pub fn flagship_socket_path() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home)
        .join(".fleetd")
        .join("flagship.sock")
}

pub fn serve_on(bind_addr: &str, port: u16) -> Result<()> {
    let relay_token = std::env::var("FLEETD_RELAY_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if relay_token.is_some() && bind_addr != "127.0.0.1" {
        eprintln!(
            "fleetd relay face: http://{bind_addr}:{port}/v1/agent/heartbeat + /v1/fleet/summary (token required)"
        );
    }
    let listener =
        TcpListener::bind((bind_addr, port)).with_context(|| format!("bind {bind_addr}:{port}"))?;
    eprintln!("fleetd event endpoint: http://{bind_addr}:{port}/v1/events");
    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                if let Err(e) = handle_stream(&mut s, relay_token.as_deref()) {
                    eprintln!("event handling error: {e:#}");
                }
            }
            Err(e) => eprintln!("accept error: {e:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_event() {
        let raw = r#"{"seat":"kode","head":"abc1234","open":9,"dirty":2}"#;
        let ev = parse_body(raw).unwrap();
        assert_eq!(ev.seat, "kode");
        assert_eq!(ev.head.as_deref(), Some("abc1234"));
        assert_eq!(ev.open, Some(9));
        assert_eq!(ev.dirty, Some(2));
        assert_eq!(ev.kind, "stop");
    }

    #[test]
    fn rejects_empty_seat() {
        let raw = r#"{"seat":""}"#;
        assert!(parse_body(raw).is_err());
    }

    #[test]
    fn rejects_path_seat() {
        let raw = r#"{"seat":"a/b"}"#;
        assert!(parse_body(raw).is_err());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_body("{not json").is_err());
    }
}
