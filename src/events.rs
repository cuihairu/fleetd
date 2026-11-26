//! Event ingestion surface: the control plane's completion-callback endpoint.
//!
//! Two entry points (same handler body):
//! - `fleetd event` — CLI fallback for hooks that prefer curl-to-self
//! - `fleetd serve` — long-running HTTP listener so Stop hooks stay dumb
//!
//! The Stop hook never classifies anything itself: it only reports
//! {seat, head, open, dirty}; this module compares against the dispatch
//! baseline and records the verdict (completed / partial / stalled /
//! no_progress / untracked).

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use crate::ledger::{self, SeatEvent};

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
    let v = verdict;
    let open = ev.open.unwrap_or(0);
    let dirty = ev.dirty.unwrap_or(0);
    // No queued tag source yet (queue lands later); nudge todo when open.
    let decision = crate::dispatcher::decide(
        v, open, None, dirty, true, true, false,
    );
    let summary = match &decision {
        crate::dispatcher::Decision::Noop { reason } => format!("noop: {reason}"),
        _ => format!("would: {:?}", decision),
    };
    Ok((verdict.as_str().into(), summary))
}

/// Minimal HTTP/1.1 handler — no framework. Reads one request, answers JSON.
pub fn handle_stream(stream: &mut TcpStream) -> Result<()> {
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

    let (status, payload) = match request_line.split_whitespace().next() {
        Some("POST") if request_line.contains("/v1/events") => match ingest(&body) {
            Ok(v) => (200, format!(r#"{{"verdict":"{}"}}"#, v)),
            Err(e) => (400, format!(r#"{{"error":"{}"}}"#, e)),
        },
        // Read-only fleet summary for remote dashboards (2026-11-27).
        // No auth in step 1: bind is loopback by default; token lands with
        // the public relay face (relay-design F5).
        Some("GET") if request_line.contains("/v1/fleet/summary") => {
            match crate::summary::build(7) {
                Ok(s) => match serde_json::to_string(&s) {
                    Ok(j) => (200, j),
                    Err(e) => (500, format!(r#"{{"error":"{}"}}"#, e)),
                },
                Err(e) => (500, format!(r#"{{"error":"{}"}}"#, e)),
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
/// for the loopback-only default, or "0.0.0.0" when agents reach the hub
/// over the network (fleetd agent pull mode / dev public relay).
pub fn serve_on(bind_addr: &str, port: u16) -> Result<()> {
    let listener =
        TcpListener::bind((bind_addr, port)).with_context(|| format!("bind {bind_addr}:{port}"))?;
    eprintln!("fleetd event endpoint: http://{bind_addr}:{port}/v1/events");
    for stream in listener.incoming() {
        match stream {
            Ok(mut s) => {
                if let Err(e) = handle_stream(&mut s) {
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
