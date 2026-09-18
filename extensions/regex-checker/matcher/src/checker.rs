//! Fixed-rule input-block checker callback.

use std::sync::Arc;

use crate::rules::{RuleSet, flatten_fragments};
use bitrouter_sdk::extension::request_check::{Callback, Decision, Input};

const INPUT_BLOCKED_REASON: &str = "guardrail.input_blocked";

/// Build an input-only checker callback over an immutable compiled rule set.
///
/// Fragment order and legacy newline boundaries are preserved. Rule names and
/// matched request text are never included in the decision.
pub fn callback(rules: RuleSet) -> Arc<Callback> {
    let rules = Arc::new(rules);
    Arc::new(move |request: &Input| {
        let text = flatten_fragments(
            request
                .content
                .iter()
                .filter_map(|fragment| fragment.text.as_deref()),
        );
        if rules.first_block(&text).is_some() {
            Decision::Deny {
                reason_code: INPUT_BLOCKED_REASON.to_owned(),
            }
        } else {
            Decision::Allow
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::extension::request_check::{
        ContentFragment, ContentFragmentKind, ContentRole, RequestCheckCoverage,
        RequestCheckCoverageScope, RequestCheckCoverageStatus,
    };

    use crate::config::{InputAction, InputGuardrailConfig, InputRuleSpec, InputScope};

    fn input(fragments: &[&str]) -> Input {
        Input {
            content: fragments
                .iter()
                .map(|text| ContentFragment {
                    role: ContentRole::User,
                    kind: ContentFragmentKind::Text,
                    text: Some((*text).to_owned()),
                })
                .collect(),
            coverage: RequestCheckCoverage {
                scope: RequestCheckCoverageScope::EntryRequestText,
                text_bytes: fragments.iter().map(|text| text.len() as u64).sum(),
                text_fragments: fragments.len() as u64,
                excluded_media_fragments: 0,
                status: RequestCheckCoverageStatus::CompleteWithinScope,
            },
        }
    }

    #[test]
    fn callback_preserves_fragment_boundaries_and_sanitizes_denials()
    -> Result<(), Box<dyn std::error::Error>> {
        let rules = InputGuardrailConfig {
            scope: InputScope::Input,
            rules: vec![InputRuleSpec {
                name: "private-rule-name".to_owned(),
                pattern: r"for\nbidden\n".to_owned(),
                action: InputAction::Block,
            }],
        }
        .compile()?;
        let check = callback(rules);
        let denied = check(&input(&["for", "bidden"]));
        assert_eq!(
            denied,
            Decision::Deny {
                reason_code: "guardrail.input_blocked".to_owned(),
            }
        );
        denied.validate()?;
        assert_eq!(check(&input(&["forbidden"])), Decision::Allow);
        assert_eq!(check(&input(&["clean"])), Decision::Allow);
        Ok(())
    }
}
