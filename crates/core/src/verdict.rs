//! verdict: pure classification of agent lifecycle outcomes.
//! Split from ledger (user-approved layering 2026-11-27): types and pure
//! functions live in core; SQLite IO lives in store.

use serde::Serialize;

/// One agent lifecycle event as reported by a Stop hook (or CLI fallback).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SeatEvent {
    pub seat: String,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub open: Option<i64>,
    /// Blocked (non-actionable) todo items. These are counted separately from
    /// `open` so the scheduler never nudges a seat into work it cannot do —
    /// a fully-blocked seat must go Noop, not burn cycles re-attempting the
    /// same blocked item (user rule: no empty-burning blocked todos).
    #[serde(default)]
    pub blocked: Option<i64>,
    #[serde(default)]
    pub dirty: Option<i64>,
    #[serde(default = "default_kind")]
    pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// HEAD advanced past baseline and the tree is clean.
    Completed,
    /// HEAD advanced but uncommitted changes remain.
    Partial,
    /// HEAD unchanged but the tree is dirty — agent stalled mid-task.
    Stalled,
    /// HEAD unchanged and the tree is clean — nothing was produced.
    NoProgress,
    /// No in-flight dispatch to judge against.
    Untracked,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Completed => "completed",
            Verdict::Partial => "partial",
            Verdict::Stalled => "stalled",
            Verdict::NoProgress => "no_progress",
            Verdict::Untracked => "untracked",
        }
    }
}

pub fn classify(baseline: Option<&str>, head: Option<&str>, dirty: Option<i64>) -> Verdict {
    let dirty = dirty.unwrap_or(0);
    match (baseline, head) {
        (None, _) => Verdict::Untracked,
        // HEAD unknown: cannot judge progress — honestly untracked.
        (Some(_), None) => Verdict::Untracked,
        (Some(b), Some(h)) if h != b => {
            if dirty == 0 {
                Verdict::Completed
            } else {
                Verdict::Partial
            }
        }
        (Some(_), Some(_)) => {
            if dirty > 0 {
                Verdict::Stalled
            } else {
                Verdict::NoProgress
            }
        }
    }
}

fn default_kind() -> String {
    "stop".into()
}
