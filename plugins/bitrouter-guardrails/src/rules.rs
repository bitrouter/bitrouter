//! Guardrail rules and matching.
//!
//! A [`RuleSet`] is an ordered list of [`GuardrailRule`]s, each a regex plus an
//! [`Action`]. The upstream hook runs the set over request content; the
//! downstream hook runs it over the response stream via a
//! [`SlidingWindowMatcher`] so a pattern that spans two stream deltas is still
//! caught.

use regex::{Regex, RegexBuilder};

/// Compiled-size ceiling for an operator-supplied guardrail pattern. The
/// `regex` crate is already backtracking-free (no classic ReDoS), but a
/// pathological counted-repetition pattern could still balloon the compiled
/// program — this caps it at 1 MiB so a bad config fails fast at load time
/// rather than OOM-ing.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// What to do when a rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Block the content — upstream this is a 400 deny, downstream a stream abort.
    Block,
    /// Redact the matched span — replace it with the redaction placeholder.
    Redact,
}

/// One guardrail rule.
#[derive(Debug, Clone)]
pub struct GuardrailRule {
    /// Human-readable rule name (surfaced in deny reasons / logs).
    pub name: String,
    /// The pattern to match.
    pub pattern: Regex,
    /// What to do on a match.
    pub action: Action,
}

impl GuardrailRule {
    /// Build a rule, compiling `pattern` as a regex under a 1 MiB
    /// compiled-size cap so a hostile pattern can't blow up the runtime.
    pub fn new(
        name: impl Into<String>,
        pattern: &str,
        action: Action,
    ) -> Result<Self, regex::Error> {
        let pattern = RegexBuilder::new(pattern)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()?;
        Ok(Self {
            name: name.into(),
            pattern,
            action,
        })
    }
}

/// The placeholder substituted for redacted spans.
pub const REDACTION: &str = "[REDACTED]";

/// An ordered set of guardrail rules.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<GuardrailRule>,
}

impl RuleSet {
    /// An empty rule set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a rule set from rules.
    pub fn from_rules(rules: impl IntoIterator<Item = GuardrailRule>) -> Self {
        Self {
            rules: rules.into_iter().collect(),
        }
    }

    /// Add a rule.
    pub fn push(&mut self, rule: GuardrailRule) {
        self.rules.push(rule);
    }

    /// Whether the set has no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The name of the first `Block` rule that matches `text`, if any.
    pub fn first_block(&self, text: &str) -> Option<&str> {
        self.rules
            .iter()
            .find(|r| r.action == Action::Block && r.pattern.is_match(text))
            .map(|r| r.name.as_str())
    }

    /// Apply every `Redact` rule to `text`. Returns the redacted string and
    /// whether anything was redacted.
    pub fn redact(&self, text: &str) -> (String, bool) {
        let mut out = text.to_string();
        let mut changed = false;
        for rule in &self.rules {
            if rule.action == Action::Redact {
                let replaced = rule.pattern.replace_all(&out, REDACTION).into_owned();
                if replaced != out {
                    changed = true;
                    out = replaced;
                }
            }
        }
        (out, changed)
    }
}

/// The verdict from feeding a streamed delta through a [`SlidingWindowMatcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowResult {
    /// Emit this (possibly redacted) delta downstream.
    Emit(String),
    /// A `Block` rule matched — the named rule. The stream must abort.
    Blocked(String),
}

/// Detection carry length: the trailing window of already-emitted text kept so
/// a `Block` pattern straddling a delta boundary is still detected. 128 chars
/// is a pragmatic ceiling on guardrail pattern length.
const CARRY_CHARS: usize = 128;

/// Runs a [`RuleSet`] over a streamed response. Every delta is emitted in the
/// same turn (streaming is never stalled); a short trailing window of recent
/// text is kept purely for **`Block` detection** across delta boundaries.
///
/// `Redact` rules are applied per-delta. A redactable pattern split exactly
/// across two deltas is a known limitation — `Block` (the security-critical
/// action) *is* cross-delta because abort can fire after partial emission.
#[derive(Debug)]
pub struct SlidingWindowMatcher {
    /// Shared via `Arc` so the per-delta matcher construction is allocation-
    /// free: a `RuleSet` holds compiled `Regex`es, and cloning it allocated
    /// fresh `Vec`s + `Regex` references per streamed text token.
    rules: std::sync::Arc<RuleSet>,
    carry: Vec<char>,
}

impl SlidingWindowMatcher {
    /// Build a matcher over a rule set with an empty carry window.
    pub fn new(rules: std::sync::Arc<RuleSet>) -> Self {
        Self {
            rules,
            carry: Vec::new(),
        }
    }

    /// Build a matcher restoring a carry window (the per-request carry is
    /// persisted in `StreamContext` metadata between `on_part` calls).
    pub fn with_carry(rules: std::sync::Arc<RuleSet>, carry: &str) -> Self {
        Self {
            rules,
            carry: carry.chars().collect(),
        }
    }

    /// The current carry window, to persist back into `StreamContext` metadata.
    pub fn carry(&self) -> String {
        self.carry.iter().collect()
    }

    /// Feed one streamed text delta. Returns the (possibly redacted) delta to
    /// emit, or a `Blocked` verdict if a `Block` rule matched the delta in the
    /// context of the recent carry window.
    pub fn feed(&mut self, delta: &str) -> WindowResult {
        let mut combined: String = self.carry.iter().collect();
        combined.push_str(delta);

        // Block detection runs over carry + delta — cross-boundary aware.
        if let Some(name) = self.rules.first_block(&combined) {
            return WindowResult::Blocked(name.to_string());
        }

        // Redaction is applied to this delta; emit it in the same turn.
        let (redacted, _) = self.rules.redact(delta);

        // Slide the carry window forward over the (original) combined text.
        let chars: Vec<char> = combined.chars().collect();
        let keep = chars.len().min(CARRY_CHARS);
        self.carry = chars[chars.len() - keep..].to_vec();

        WindowResult::Emit(redacted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> RuleSet {
        RuleSet::from_rules([
            GuardrailRule::new("ssn", r"\d{3}-\d{2}-\d{4}", Action::Redact).unwrap(),
            GuardrailRule::new("badword", r"(?i)forbidden", Action::Block).unwrap(),
        ])
    }

    #[test]
    fn block_rule_detected() {
        assert_eq!(rules().first_block("this is FORBIDDEN"), Some("badword"));
        assert_eq!(rules().first_block("this is fine"), None);
    }

    #[test]
    fn redact_rule_replaces_match() {
        let (out, changed) = rules().redact("my ssn is 123-45-6789 ok");
        assert!(changed);
        assert_eq!(out, "my ssn is [REDACTED] ok");
        let (out2, changed2) = rules().redact("nothing here");
        assert!(!changed2);
        assert_eq!(out2, "nothing here");
    }

    #[test]
    fn sliding_window_redacts_within_a_delta() {
        let mut m = SlidingWindowMatcher::new(std::sync::Arc::new(rules()));
        let r = m.feed("my ssn is 123-45-6789 done");
        assert_eq!(
            r,
            WindowResult::Emit("my ssn is [REDACTED] done".to_string())
        );
    }

    #[test]
    fn sliding_window_blocks_cross_delta_badword() {
        // Block IS cross-delta — abort can fire after partial emission.
        let mut m = SlidingWindowMatcher::new(std::sync::Arc::new(rules()));
        let r1 = m.feed("the word is forb");
        assert!(matches!(r1, WindowResult::Emit(_)));
        let r2 = m.feed("idden now");
        assert_eq!(r2, WindowResult::Blocked("badword".to_string()));
    }

    #[test]
    fn carry_round_trips_through_metadata() {
        let mut m = SlidingWindowMatcher::new(std::sync::Arc::new(rules()));
        let _ = m.feed("hello there");
        let carry = m.carry();
        // a fresh matcher restored from the carry sees the same window
        let mut restored = SlidingWindowMatcher::with_carry(std::sync::Arc::new(rules()), &carry);
        assert_eq!(
            restored.feed(" world"),
            WindowResult::Emit(" world".to_string())
        );
    }
}
