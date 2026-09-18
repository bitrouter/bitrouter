//! Fixed-rule input-block checker callback.

use std::sync::Arc;

use bitrouter_checker_protocol::v1;
use bitrouter_guardrails::rules::{RuleSet, flatten_fragments};

use crate::adapter::{CheckCallback, CheckDecision};

const INPUT_BLOCKED_REASON: &str = "guardrail.input_blocked";

/// Build an input-only checker callback over an immutable compiled rule set.
///
/// Fragment order and legacy newline boundaries are preserved. Rule names and
/// matched request text are never included in the wire decision.
pub fn callback(rules: RuleSet) -> Arc<CheckCallback> {
    let rules = Arc::new(rules);
    Arc::new(move |request: &v1::Request| {
        let text = flatten_fragments(
            request
                .content
                .iter()
                .filter_map(|fragment| fragment.text.as_deref()),
        );
        if rules.first_block(&text).is_some() {
            CheckDecision::Deny {
                reason_code: INPUT_BLOCKED_REASON.to_owned(),
            }
        } else {
            CheckDecision::Allow
        }
    })
}
