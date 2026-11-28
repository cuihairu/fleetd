//! gate: the adversarial review gate between "agent says done" and "done"
//! (FLEETD-GATE-001, 2026-11-28). A seat reporting completion no longer
//! lands as done directly: the dispatch enters the `review` state and an
//! external checker (orchestrator / review seat) must pass it with evidence.
//!
//! Pure layer — types and the decision function only; the SQLite state
//! transitions live in store::ledger, the CLI/MCP faces in the binary/mcp.

use crate::verdict::Verdict;

/// How a task's completion may be verified. Declared in the task file
/// (`acceptance: <mode>`); anything unreadable or undeclared defaults to
/// adversarial — the safe gate ("no evidence, no done").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Acceptance {
    /// Task text is its own receipt (pure docs / ledger ticks): verdict
    /// Completed lands as done without review.
    SelfCheck,
    /// A checker must review the effect and pass it with evidence (default).
    #[default]
    Adversarial,
    /// Like adversarial, plus the user must verify in person (hard gate for
    /// beacon-island style tasks where only the user can see the effect).
    UserVerify,
}

impl Acceptance {
    pub fn as_str(&self) -> &'static str {
        match self {
            Acceptance::SelfCheck => "self-check",
            Acceptance::Adversarial => "adversarial",
            Acceptance::UserVerify => "user-verify",
        }
    }

    /// Parse one mode token. Strict: unknown names are None so callers fall
    /// back to the default instead of inventing a weaker gate.
    pub fn parse(s: &str) -> Option<Acceptance> {
        match s.trim().to_ascii_lowercase().as_str() {
            "self-check" | "self_check" | "selfcheck" => Some(Acceptance::SelfCheck),
            "adversarial" => Some(Acceptance::Adversarial),
            "user-verify" | "user_verify" | "userverify" => Some(Acceptance::UserVerify),
            _ => None,
        }
    }
}

/// What the gate knows about the dispatched task. Extensible: acceptance is
/// the only field the gate reads today; task files carry more (scope, repos)
/// that later gates may consult.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskSpec {
    pub acceptance: Acceptance,
}

impl TaskSpec {
    /// Parse a task file body. Undeclared acceptance stays adversarial.
    pub fn from_task_text(text: &str) -> TaskSpec {
        TaskSpec {
            acceptance: parse_acceptance(text).unwrap_or_default(),
        }
    }
}

/// Scan task text for an `acceptance: <mode>` declaration. Tolerates a
/// markdown list marker and surrounding whitespace; the value is matched
/// case-insensitively. Returns the LAST declaration so a task that widens
/// the gate mid-file wins over an early weaker one.
pub fn parse_acceptance(text: &str) -> Option<Acceptance> {
    let mut found = None;
    for line in text.lines() {
        let t = line.trim().trim_start_matches('-').trim_start_matches('*');
        let t = t.trim();
        if let Some(rest) = t
            .strip_prefix("acceptance:")
            .or_else(|| t.strip_prefix("acceptance："))
        {
            if let Some(a) = Acceptance::parse(rest) {
                found = Some(a);
            }
        }
    }
    found
}

/// What the gate decides when a seat reports a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Completion enters the review state; a checker must pass it with
    /// evidence before the dispatch lands as done.
    EnterReview,
    /// Completion lands as done directly (self-check tasks only).
    DirectDone,
    /// Not a completion — existing verdict semantics apply (resume notes,
    /// close with the partial/stalled outcome). Never enters review.
    Reject,
}

impl GateDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            GateDecision::EnterReview => "enter_review",
            GateDecision::DirectDone => "direct_done",
            GateDecision::Reject => "reject",
        }
    }
}

/// The gate itself: verdict × acceptance → state-machine decision.
/// Only Verdict::Completed can move toward done; every other verdict keeps
/// its existing meaning (Reject here = "not the gate's business").
pub fn gate(verdict: Verdict, task: &TaskSpec) -> GateDecision {
    match verdict {
        Verdict::Completed => match task.acceptance {
            Acceptance::SelfCheck => GateDecision::DirectDone,
            Acceptance::Adversarial | Acceptance::UserVerify => GateDecision::EnterReview,
        },
        _ => GateDecision::Reject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(a: Acceptance) -> TaskSpec {
        TaskSpec { acceptance: a }
    }

    // --- gate(): three acceptance tiers × Completed / non-Completed ---

    #[test]
    fn completed_self_check_lands_directly() {
        assert_eq!(
            gate(Verdict::Completed, &spec(Acceptance::SelfCheck)),
            GateDecision::DirectDone
        );
    }

    #[test]
    fn completed_adversarial_enters_review() {
        assert_eq!(
            gate(Verdict::Completed, &spec(Acceptance::Adversarial)),
            GateDecision::EnterReview
        );
    }

    #[test]
    fn completed_user_verify_enters_review() {
        assert_eq!(
            gate(Verdict::Completed, &spec(Acceptance::UserVerify)),
            GateDecision::EnterReview
        );
    }

    #[test]
    fn default_task_is_adversarial() {
        assert_eq!(
            gate(Verdict::Completed, &TaskSpec::default()),
            GateDecision::EnterReview
        );
    }

    #[test]
    fn partial_never_enters_review() {
        for a in [
            Acceptance::SelfCheck,
            Acceptance::Adversarial,
            Acceptance::UserVerify,
        ] {
            assert_eq!(gate(Verdict::Partial, &spec(a)), GateDecision::Reject);
        }
    }

    #[test]
    fn stalled_and_no_progress_reject() {
        assert_eq!(
            gate(Verdict::Stalled, &spec(Acceptance::Adversarial)),
            GateDecision::Reject
        );
        assert_eq!(
            gate(Verdict::NoProgress, &spec(Acceptance::UserVerify)),
            GateDecision::Reject
        );
    }

    #[test]
    fn untracked_rejects() {
        assert_eq!(
            gate(Verdict::Untracked, &spec(Acceptance::SelfCheck)),
            GateDecision::Reject
        );
    }

    // --- acceptance parsing ---

    #[test]
    fn acceptance_declaration_parsed_from_task_text() {
        let t = "# task\n\nacceptance: self-check\n\n- [ ] tick the box";
        assert_eq!(TaskSpec::from_task_text(t), spec(Acceptance::SelfCheck));
    }

    #[test]
    fn acceptance_in_markdown_list_item_parsed() {
        let t = "- acceptance: user-verify\n";
        assert_eq!(TaskSpec::from_task_text(t), spec(Acceptance::UserVerify));
    }

    #[test]
    fn undeclared_or_unknown_acceptance_defaults_adversarial() {
        assert_eq!(
            TaskSpec::from_task_text("# task, no declaration"),
            TaskSpec::default()
        );
        assert_eq!(
            TaskSpec::from_task_text("acceptance: trust-me"),
            spec(Acceptance::Adversarial)
        );
    }

    #[test]
    fn acceptance_parse_normalizes_case_and_aliases() {
        assert_eq!(Acceptance::parse("Self-Check"), Some(Acceptance::SelfCheck));
        assert_eq!(
            Acceptance::parse("  adversarial  "),
            Some(Acceptance::Adversarial)
        );
        assert_eq!(
            Acceptance::parse("user_verify"),
            Some(Acceptance::UserVerify)
        );
        assert_eq!(Acceptance::parse(""), None);
    }

    #[test]
    fn last_declaration_wins() {
        // A task that starts as self-check but hardens later must gate.
        let t = "acceptance: self-check\n...\nacceptance: adversarial";
        assert_eq!(parse_acceptance(t), Some(Acceptance::Adversarial));
    }

    #[test]
    fn full_width_colon_accepted() {
        let t = "acceptance：self-check";
        assert_eq!(parse_acceptance(t), Some(Acceptance::SelfCheck));
    }
}
