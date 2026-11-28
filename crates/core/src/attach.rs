//! Attach-face JSONL parsing (FLEETD-ATTACH-001): reduce raw transcript /
//! stream JSONL lines to short human-readable event lines. Pure logic —
//! no file IO, no clock; callers own the tail-walk and the printing.
//!
//! Design constraint (docs/agent-support.md §4.1): fleetd never forwards
//! raw terminal streams or raw JSON — everything is reduced to plain-text
//! summaries. Unparseable lines (half-written JSONL tails, noise records
//! like cost-state) are skipped silently, never errors.

use crate::presence::iso_to_epoch;

/// Assistant text/thinking summaries are clipped to this many chars.
pub const TEXT_SUMMARY_CHARS: usize = 120;
/// Tool input summaries are clipped to this many chars (design spec: 80).
pub const TOOL_INPUT_CHARS: usize = 80;

/// One attachable event reduced from a JSONL line.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachEvent {
    /// Event timestamp in epoch seconds (None = line carried no usable one).
    pub ts: Option<f64>,
    pub kind: AttachKind,
}

/// What kind of assistant-side event this is. User tool_result and
/// attachment lines are recognized by the parser but produce no output —
/// the readable attach stream is assistant-side only.
#[derive(Debug, Clone, PartialEq)]
pub enum AttachKind {
    /// Assistant text block.
    Text(String),
    /// Assistant thinking block (marked so a human can tell it apart).
    Thinking(String),
    /// Tool invocation: name + one-line input summary.
    Tool { name: String, input: String },
}

/// Collapse every whitespace run (incl. newlines) to single spaces and clip
/// to `max` chars with an ellipsis — only when content was actually cut
/// (whitespace-only collapsing never adds one). The readable stream must
/// stay one line per event even when the source text is a multi-paragraph
/// block.
pub fn one_line(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if out.chars().count() >= max {
            return format!("{out}…");
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

/// One-line summary of a tool_use input object: the most descriptive field
/// (command / file_path / pattern / url / …) or the compact JSON fallback.
fn summarize_input(input: &serde_json::Value) -> String {
    for key in ["command", "file_path", "path", "pattern", "url", "query"] {
        if let Some(s) = input.get(key).and_then(|x| x.as_str()) {
            return one_line(s, TOOL_INPUT_CHARS);
        }
    }
    one_line(&input.to_string(), TOOL_INPUT_CHARS)
}

fn content_events(ts: Option<f64>, blocks: &serde_json::Value) -> Vec<AttachEvent> {
    let mut out = Vec::new();
    let Some(blocks) = blocks.as_array() else {
        return out;
    };
    for b in blocks {
        let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "text" => {
                let s = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !s.is_empty() {
                    out.push(AttachEvent {
                        ts,
                        kind: AttachKind::Text(one_line(s, TEXT_SUMMARY_CHARS)),
                    });
                }
            }
            "thinking" => {
                let s = b.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                if !s.is_empty() {
                    out.push(AttachEvent {
                        ts,
                        kind: AttachKind::Thinking(one_line(s, TEXT_SUMMARY_CHARS)),
                    });
                }
            }
            "tool_use" => {
                let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                let input = b.get("input").cloned().unwrap_or_default();
                out.push(AttachEvent {
                    ts,
                    kind: AttachKind::Tool {
                        name: name.to_string(),
                        input: summarize_input(&input),
                    },
                });
            }
            _ => {}
        }
    }
    out
}

/// Parse one JSONL line into 0..n attach events. One assistant line can
/// carry several content blocks (text + thinking + tool_use) — each becomes
/// its own output line, in order. Covers the claude transcript/stream-json
/// shape; codex `item.completed` agent messages map to Text too. Broken
/// JSON, user tool_result, attachments and unknown noise types yield an
/// empty vec (skipped, not errors).
pub fn parse_event_line(line: &str) -> Vec<AttachEvent> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(iso_to_epoch);
    match v.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            let blocks = v
                .get("message")
                .and_then(|m| m.get("content"))
                .unwrap_or(&serde_json::Value::Null);
            content_events(ts, blocks)
        }
        // codex exec --json: {"type":"item.completed","item":{"type":
        // "agent_message","text":…}} — the only readable-output shape.
        Some("item.completed") => {
            let item = v.get("item").unwrap_or(&serde_json::Value::Null);
            if item.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
                let s = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !s.is_empty() {
                    return vec![AttachEvent {
                        ts,
                        kind: AttachKind::Text(one_line(s, TEXT_SUMMARY_CHARS)),
                    }];
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Render one event as its attach line: `[HH:MM:SS] ▍…` for text/thinking,
/// `[HH:MM:SS] ⚙ Tool: input` for tool_use. Missing timestamps render as
/// `[--:--:--]` (local clock otherwise, per the design's line format).
pub fn format_event(ev: &AttachEvent) -> String {
    let clock = match ev.ts {
        Some(t) => {
            let secs = t as i64;
            let millis = ((t - secs as f64) * 1000.0) as u32;
            chrono::DateTime::from_timestamp(secs, millis * 1_000_000)
                .map(|d| {
                    d.with_timezone(&chrono::Local)
                        .format("%H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| "--:--:--".into())
        }
        None => "--:--:--".into(),
    };
    match &ev.kind {
        AttachKind::Text(s) => format!("[{clock}] ▍{s}"),
        AttachKind::Thinking(s) => format!("[{clock}] ▍~ {s}"),
        AttachKind::Tool { name, input } => format!("[{clock}] ⚙ {name}: {input}"),
    }
}

/// Render events in order, one attach line each.
pub fn render(events: &[AttachEvent]) -> String {
    events
        .iter()
        .map(format_event)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Backward read chunk size for the tail walk.
const TAIL_CHUNK: u64 = 64 * 1024;

/// Tail-read a JSONL file and return its last `max_events` parsed events.
/// Walks backwards in chunks and stops as soon as enough complete lines are
/// in hand — never loads the whole file (persona-size transcripts run to
/// thousands of events); `max_bytes` hard-caps the walk when lines are
/// gigantic. The first line of the first chunk may be partial (chunk
/// boundary) and is dropped unless the walk reached the file start.
pub fn tail_events(
    path: &std::path::Path,
    max_events: usize,
    max_bytes: u64,
) -> std::io::Result<Vec<AttachEvent>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut collected: Vec<u8> = Vec::new();
    let mut pos = len;
    let mut reached_start = false;
    // Some lines parse to 0 events (tool_result/noise), so aim for ~2x
    // lines before giving up on the count.
    let want_lines = max_events * 2 + 4;
    while collected.iter().filter(|&&b| b == b'\n').count() < want_lines {
        let start = pos.saturating_sub(TAIL_CHUNK);
        let mut chunk = vec![0u8; (pos - start) as usize];
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&collected);
        collected = chunk;
        pos = start;
        if pos == 0 {
            reached_start = true;
            break;
        }
        if collected.len() as u64 >= max_bytes {
            break;
        }
    }
    let text = String::from_utf8_lossy(&collected);
    let mut lines: Vec<&str> = text.lines().collect();
    if !reached_start && !lines.is_empty() {
        lines.remove(0); // partial head line from the chunk boundary
    }
    let mut events: Vec<AttachEvent> = Vec::new();
    for line in lines {
        events.extend(parse_event_line(line));
    }
    if events.len() > max_events {
        events.drain(..events.len() - max_events);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(blocks: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-11-28T09:30:00Z","message":{{"content":{blocks}}}}}"#
        )
    }

    #[test]
    fn assistant_text_block_parses() {
        let evs = parse_event_line(&assistant(
            r#"[{"type":"text","text":"done, pushed to main"}]"#,
        ));
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind, AttachKind::Text("done, pushed to main".into()));
        assert!(evs[0].ts.is_some());
        let line = format_event(&evs[0]);
        assert!(line.contains('▍'), "text lines carry the ▍ marker");
        assert!(line.contains("done, pushed"));
    }

    #[test]
    fn tool_use_parses_name_and_command() {
        let blocks =
            r#"[{"type":"tool_use","name":"Bash","input":{"command":"git status --short"}}]"#;
        let evs = parse_event_line(&assistant(blocks));
        assert_eq!(
            evs[0].kind,
            AttachKind::Tool {
                name: "Bash".into(),
                input: "git status --short".into()
            }
        );
        let line = format_event(&evs[0]);
        assert!(line.contains("⚙ Bash: git status --short"));
    }

    #[test]
    fn tool_input_clipped_to_eighty_chars() {
        let long = "x".repeat(200);
        let blocks =
            format!(r#"[{{"type":"tool_use","name":"Bash","input":{{"command":"{long}"}}}}]"#);
        let evs = parse_event_line(&assistant(&blocks));
        match &evs[0].kind {
            AttachKind::Tool { input, .. } => {
                assert!(input.chars().count() <= TOOL_INPUT_CHARS + 1); // + ellipsis
                assert!(input.ends_with('…'));
            }
            other => panic!("expected tool event, got {other:?}"),
        }
    }

    #[test]
    fn thinking_block_marked() {
        let blocks = r#"[{"type":"thinking","thinking":"plan: read ledger first"}]"#;
        let evs = parse_event_line(&assistant(blocks));
        assert_eq!(
            evs[0].kind,
            AttachKind::Thinking("plan: read ledger first".into())
        );
        assert!(format_event(&evs[0]).contains("▍~"));
    }

    #[test]
    fn one_assistant_line_yields_events_in_block_order() {
        let blocks = r#"[{"type":"text","text":"running check"},{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]"#;
        let evs = parse_event_line(&assistant(blocks));
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0].kind, AttachKind::Text(_)));
        assert!(matches!(evs[1].kind, AttachKind::Tool { .. }));
    }

    #[test]
    fn broken_half_line_is_skipped() {
        // Transcripts get half-written lines at the tail; they must vanish.
        assert!(parse_event_line(r#"{"type":"assistant","timestamp":"2026-11"#).is_empty());
        assert!(parse_event_line("not json at all").is_empty());
        assert!(parse_event_line("").is_empty());
    }

    #[test]
    fn noise_and_user_lines_produce_no_output() {
        // Real transcript noise + user tool_result lines: recognized shapes
        // with no assistant-side output.
        let cost = r#"{"type":"cost-state","totalCostUSD":1.5}"#;
        assert!(parse_event_line(cost).is_empty());
        let user = r#"{"type":"user","timestamp":"2026-11-28T09:30:00Z","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
        assert!(parse_event_line(user).is_empty());
    }

    #[test]
    fn line_without_timestamp_renders_placeholder_clock() {
        let raw =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"no ts here"}]}}"#;
        let evs = parse_event_line(raw);
        assert_eq!(evs[0].ts, None);
        let line = format_event(&evs[0]);
        assert!(line.starts_with("[--:--:--]"), "got: {line}");
    }

    #[test]
    fn multiline_text_collapses_to_one_line() {
        let blocks = r#"[{"type":"text","text":"line one\n\nline two   spaced"}]"#;
        let evs = parse_event_line(&assistant(blocks));
        match &evs[0].kind {
            AttachKind::Text(s) => assert_eq!(s, "line one line two spaced"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn codex_agent_message_maps_to_text() {
        let raw =
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"codex says done"}}"#;
        let evs = parse_event_line(raw);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind, AttachKind::Text("codex says done".into()));
    }

    #[test]
    fn tail_events_reads_only_the_tail() {
        let dir = std::env::temp_dir().join(format!("fleetd-attach-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mut body = String::new();
        for i in 0..300 {
            body.push_str(&format!(
                r#"{{"type":"user","timestamp":"2026-11-28T09:00:{:02}Z","message":{{"content":[{{"type":"tool_result","content":"noise {i}"}}]}}}}"#,
                i % 60
            ));
            body.push('\n');
        }
        body.push_str(&assistant(r#"[{"type":"text","text":"tail event A"}]"#));
        body.push('\n');
        body.push_str(&assistant(
            r#"[{"type":"tool_use","name":"Bash","input":{"command":"tail event B cmd"}}]"#,
        ));
        body.push('\n');
        std::fs::write(&path, body).unwrap();
        let evs = tail_events(&path, 50, 4 * 1024 * 1024).unwrap();
        assert_eq!(evs.len(), 2, "only the two assistant events exist");
        assert!(matches!(&evs[0].kind, AttachKind::Text(s) if s.contains("tail event A")));
        assert!(format_event(&evs[1]).contains("⚙ Bash: tail event B cmd"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_events_handles_empty_file() {
        let dir = std::env::temp_dir().join(format!("fleetd-attach-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(tail_events(&path, 10, 1024).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
