//! Trace / session attribution helpers shared across the OpenAI, Anthropic,
//! and Google chat handlers.
//!
//! Each protocol handler calls [`extract_trace_context`] before invoking
//! `to_call_options(...)` and assigns the returned [`TraceContext`] to
//! `LanguageModelCallOptions.trace_context`. The OTLP exporter (in
//! `bitrouter-observe`) then attaches the conversation/user identifiers as
//! GenAI semconv attributes (`gen_ai.conversation.id`, `user.id`).
//!
//! Header-side extraction is the v1 surface. Body-side extraction
//! (OpenRouter-compatible `session_id` and `trace.metadata.*` request body
//! fields) lands when the request body types are extended in a follow-up
//! commit.

use bitrouter_core::observe::TraceContext;
use warp::http::{HeaderMap, HeaderValue};

/// Header carrying the BitRouter conversation/session identifier. Maps to
/// `gen_ai.conversation.id` (and the OpenRouter-compatible duplicate
/// `session.id`) on the resulting span.
pub const HEADER_SESSION_ID: &str = "x-bitrouter-session-id";

/// Header carrying the end-user identifier. Maps to `user.id`.
pub const HEADER_USER_ID: &str = "x-bitrouter-user-id";

/// Builds a [`TraceContext`] from incoming request headers.
///
/// Returns `None` when no attribution headers are present so the caller can
/// leave `trace_context: None` on `LanguageModelCallOptions` — equivalent to
/// "let the exporter generate a fresh trace ID and emit an unparented span."
///
/// The `account_id` argument carries the authenticated caller context so the
/// resulting span attributes always include `bitrouter.account_id` even when
/// the client did not send any session/user headers, since auth and
/// telemetry attribution are independent concerns.
pub fn extract_trace_context(
    headers: &HeaderMap,
    account_id: Option<String>,
) -> Option<TraceContext> {
    let conversation_id = header_string(headers, HEADER_SESSION_ID);
    let user_id = header_string(headers, HEADER_USER_ID);

    // If absolutely nothing is attributable, return None so the exporter
    // takes the no-context path.
    if conversation_id.is_none() && user_id.is_none() && account_id.is_none() {
        return None;
    }

    Some(TraceContext {
        trace_id: None,
        parent_span_id: None,
        conversation_id,
        user_id,
        account_id,
    })
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    let value: &HeaderValue = headers.get(name)?;
    let s: &str = value.to_str().ok()?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hm(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn returns_none_when_no_attribution_signal_present() {
        let headers = hm(&[]);
        assert!(extract_trace_context(&headers, None).is_none());
    }

    #[test]
    fn returns_account_id_only_when_no_headers() {
        let headers = hm(&[]);
        let ctx = extract_trace_context(&headers, Some("acct-1".into())).expect("trace context");
        assert_eq!(ctx.account_id.as_deref(), Some("acct-1"));
        assert!(ctx.conversation_id.is_none());
        assert!(ctx.user_id.is_none());
    }

    #[test]
    fn extracts_session_id_from_header() {
        let headers = hm(&[("x-bitrouter-session-id", "sess-abc")]);
        let ctx = extract_trace_context(&headers, None).expect("trace context");
        assert_eq!(ctx.conversation_id.as_deref(), Some("sess-abc"));
        assert!(ctx.user_id.is_none());
    }

    #[test]
    fn extracts_user_id_from_header() {
        let headers = hm(&[("x-bitrouter-user-id", "user-xyz")]);
        let ctx = extract_trace_context(&headers, None).expect("trace context");
        assert_eq!(ctx.user_id.as_deref(), Some("user-xyz"));
    }

    #[test]
    fn extracts_all_three_fields() {
        let headers = hm(&[
            ("x-bitrouter-session-id", "sess-abc"),
            ("x-bitrouter-user-id", "user-xyz"),
        ]);
        let ctx = extract_trace_context(&headers, Some("acct-1".into())).expect("trace context");
        assert_eq!(ctx.conversation_id.as_deref(), Some("sess-abc"));
        assert_eq!(ctx.user_id.as_deref(), Some("user-xyz"));
        assert_eq!(ctx.account_id.as_deref(), Some("acct-1"));
    }

    #[test]
    fn empty_header_value_is_treated_as_absent() {
        let headers = hm(&[("x-bitrouter-session-id", "")]);
        // Account id absent too — should produce None.
        assert!(extract_trace_context(&headers, None).is_none());
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        // HeaderMap is case-insensitive by spec — verify our constants
        // interoperate with mixed-case headers received over the wire.
        let mut h = HeaderMap::new();
        h.insert("X-BITROUTER-SESSION-ID", HeaderValue::from_static("sess-Z"));
        let ctx = extract_trace_context(&h, None).expect("trace context");
        assert_eq!(ctx.conversation_id.as_deref(), Some("sess-Z"));
    }
}
