//! The guardrail hooks:
//! - [`DepositRulesHook`] — a `language_model::PreRequestHook` that inserts a
//!   shared [`RuleSet`] into the request's typed extensions, so the guardrail
//!   hooks downstream see a fixed, process-global rule set;
//! - [`GuardrailPreHook`] — a `language_model::PreRequestHook` that scans the
//!   **request** content and denies on a `Block` rule (upstream /);
//! - [`GuardrailStreamHook`] — a `language_model::StreamHook` that scans the
//!   **response** stream, redacting `Redact` matches and aborting on `Block`
//!   (downstream /).
//!
//! Both guardrail hooks read the active [`RuleSet`] from the pipeline's typed
//! extensions rather than capturing one at construction — so a multi-tenant
//! host can resolve a per-account rule set in an earlier pre-request stage and
//! [`PipelineContext::insert_extension`] it, and the same hooks enforce it. The
//! OSS path uses [`DepositRulesHook`] to install one global set. With no rule
//! set present, both hooks no-op (allow / pass).

use std::sync::Arc;

use async_trait::async_trait;

use bitrouter_sdk::PluginId;
use bitrouter_sdk::Result;
use bitrouter_sdk::error::BitrouterError;
use bitrouter_sdk::language_model::{
    Content, DenyReason, HookDecision, PipelineContext, PreRequestHook, StreamAction,
    StreamContext, StreamHook, StreamInterest, StreamOutcome, StreamPart,
};

use crate::rules::{RuleSet, SlidingWindowMatcher, WindowResult};

fn plugin_id() -> PluginId {
    PluginId::new("bitrouter-guardrails")
}

/// Collect every text-bearing fragment of a request prompt into one string for
/// scanning (system instruction + each message's text / reasoning content).
fn request_text(ctx: &PipelineContext) -> String {
    let prompt = ctx.prompt();
    let mut buf = String::new();
    if let Some(system) = &prompt.system {
        buf.push_str(system);
        buf.push('\n');
    }
    for message in &prompt.messages {
        for content in &message.content {
            match content {
                Content::Text { text, .. } | Content::Reasoning { text, .. } => {
                    buf.push_str(text);
                    buf.push('\n');
                }
                Content::ToolResult { output, .. } => {
                    // Scan the flattened text of the tool result (text parts and
                    // stringified JSON) so guardrail rules still see tool output.
                    buf.push_str(&output.to_provider_string());
                    buf.push('\n');
                }
                Content::ToolCall { arguments, .. } => {
                    buf.push_str(arguments);
                    buf.push('\n');
                }
                Content::File { .. } => {
                    // File parts (image / audio / document) carry no scannable
                    // text, so they contribute nothing to the guardrail buffer.
                }
                Content::Source { .. } => {
                    // Citation sources are response-side metadata (a url/title),
                    // not user-authored prompt text, so they are not scanned by
                    // the request-side guardrail.
                }
                Content::ToolApprovalResponse { reason, .. } => {
                    // A tool-approval response carries no model/user content other
                    // than an optional human-authored denial reason; scan that
                    // when present so guardrail rules still see it.
                    if let Some(reason) = reason {
                        buf.push_str(reason);
                        buf.push('\n');
                    }
                }
                Content::ToolApprovalRequest { .. } => {
                    // A tool-approval request is a provider-emitted handshake
                    // marker (approval/tool-call ids); it carries no user-authored
                    // prompt text to scan.
                }
            }
        }
    }
    buf
}

/// Upstream hook that deposits a shared [`RuleSet`] into the request's typed
/// extensions, so [`GuardrailPreHook`] / [`GuardrailStreamHook`] (which read
/// the rule set from the extensions) enforce a fixed, process-global set.
///
/// Used by [`crate::GuardrailsPlugin::with_static`]. A multi-tenant host
/// instead resolves a per-account rule set in its own pre-request stage and
/// calls [`PipelineContext::insert_extension`] directly — no deposit hook.
pub struct DepositRulesHook {
    rules: Arc<RuleSet>,
}

impl DepositRulesHook {
    /// Build a deposit hook over a shared rule set.
    pub fn new(rules: Arc<RuleSet>) -> Self {
        Self { rules }
    }
}

#[async_trait]
impl PreRequestHook for DepositRulesHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        ctx.insert_extension(self.rules.clone());
        Ok(HookDecision::Allow)
    }
}

/// Upstream guardrail: scans request content, denies on a `Block` match. Reads
/// the active [`RuleSet`] from the request's typed extensions; allows the
/// request when none is present.
#[derive(Default)]
pub struct GuardrailPreHook;

impl GuardrailPreHook {
    /// Build an upstream guardrail hook.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PreRequestHook for GuardrailPreHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        let Some(rules) = ctx.extension::<RuleSet>() else {
            return Ok(HookDecision::Allow);
        };
        if rules.is_empty() {
            return Ok(HookDecision::Allow);
        }
        let text = request_text(ctx);
        if let Some(rule_name) = rules.first_block(&text) {
            return Ok(HookDecision::Deny(DenyReason::GuardrailViolation(format!(
                "request blocked by guardrail rule '{rule_name}'"
            ))));
        }
        Ok(HookDecision::Allow)
    }
}

/// Downstream guardrail: scans the response stream, redacting `Redact` matches
/// and aborting the stream on a `Block` match. Reads the active [`RuleSet`]
/// from the stream context's typed extensions (propagated from the pre-request
/// stage); passes the stream through untouched when none is present.
///
/// The per-request sliding-window carry is persisted in `StreamContext`
/// metadata between `on_part` calls — the hook itself is stateless and shared.
#[derive(Default)]
pub struct GuardrailStreamHook;

impl GuardrailStreamHook {
    /// Build a downstream guardrail hook.
    pub fn new() -> Self {
        Self
    }

    fn load_carry(ctx: &StreamContext) -> String {
        ctx.get_metadata(&plugin_id())
            .and_then(|m| m.get("carry"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn store_carry(ctx: &mut StreamContext, carry: &str) {
        ctx.set_metadata(&plugin_id(), serde_json::json!({ "carry": carry }));
    }
}

#[async_trait]
impl StreamHook for GuardrailStreamHook {
    fn interest(&self) -> StreamInterest {
        // Only text-bearing deltas need scanning — the per-token hot path skips
        // this hook on usage / finish parts.
        StreamInterest::none()
            .with_text_delta()
            .with_reasoning_delta()
    }

    async fn on_part(&self, ctx: &mut StreamContext, part: StreamPart) -> Result<StreamAction> {
        let Some(rules) = ctx.extension::<RuleSet>() else {
            return Ok(StreamAction::Pass);
        };
        if rules.is_empty() {
            return Ok(StreamAction::Pass);
        }
        let (text, rebuild): (&str, fn(String) -> StreamPart) = match &part {
            StreamPart::TextDelta { text } => {
                (text.as_str(), |t| StreamPart::TextDelta { text: t })
            }
            StreamPart::ReasoningDelta { text } => {
                (text.as_str(), |t| StreamPart::ReasoningDelta { text: t })
            }
            // not a text-bearing part — interest() should have filtered it out
            _ => return Ok(StreamAction::Pass),
        };

        let carry = Self::load_carry(ctx);
        // `rules` is a per-delta map lookup returning a cloned `Arc` (refcount
        // bump, no realloc); the matcher moves it in and reuses the same
        // compiled regex set without re-allocating per delta.
        let mut matcher = SlidingWindowMatcher::with_carry(rules, &carry);
        let verdict = matcher.feed(text);
        Self::store_carry(ctx, &matcher.carry());

        match verdict {
            WindowResult::Blocked(rule_name) => {
                Ok(StreamAction::Abort(BitrouterError::bad_request(format!(
                    "response blocked by guardrail rule '{rule_name}'"
                ))))
            }
            WindowResult::Emit(emitted) => {
                if emitted == text {
                    Ok(StreamAction::Pass)
                } else {
                    Ok(StreamAction::Replace(vec![rebuild(emitted)]))
                }
            }
        }
    }

    async fn on_stream_end(
        &self,
        _ctx: &mut StreamContext,
        _outcome: &StreamOutcome,
    ) -> Result<()> {
        // Every delta is emitted in its own turn — there is no buffered tail to
        // flush at stream end.
        Ok(())
    }
}
