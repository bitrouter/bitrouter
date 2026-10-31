use bitrouter_ai::protocol::responses::ResponsesRequest;
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};

use super::SubmissionOptions;
use crate::error::{Error, Result};

// The gateway-request-v1 semantic header contract, independent of configuration.
// Keep this in sync with docs/specs/gateway-v1/shared-execution.md; additions require
// an explicit compatibility decision, not a client-local expansion.
const SEMANTIC_HEADERS: &[&str] = &[
    "anthropic-beta",
    "anthropic-workspace-id",
    "chatgpt-account-id",
    "openai-organization",
    "openai-project",
    "session-id",
    "thread-id",
    "user-agent",
    "x-bitrouter-acp-session-id",
    "x-bitrouter-benchmark-run-id",
    "x-bitrouter-controller-id",
    "x-bitrouter-harness",
    "x-bitrouter-parent-session-id",
    "x-bitrouter-trial-id",
    "x-bitrouter-workflow-session",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
    "x-claude-code-session-id",
    "x-codex-turn-metadata",
    "x-goog-user-project",
    "x-session-id",
    "x-smithers-node-id",
    "x-smithers-workflow-id",
];

pub(super) fn headers(options: &SubmissionOptions) -> Result<HeaderMap> {
    if options
        .headers
        .keys()
        .any(|name| !SEMANTIC_HEADERS.contains(&name.as_str()))
    {
        return Err(Error::Request(
            "submission header is outside the v1 semantic header contract",
        ));
    }
    let mut headers = options.headers.clone();
    if !headers.contains_key(USER_AGENT) {
        // Versioned user agents change the fingerprint after an SDK upgrade.
        // Explicit caller values remain part of the caller's request identity.
        headers.insert(USER_AGENT, HeaderValue::from_static("bitrouter-sdk"));
    }
    for (name, value) in [
        ("x-bitrouter-request-id", &options.correlation_id),
        ("idempotency-key", &options.idempotency_key),
    ] {
        if let Some(value) = value {
            let value = HeaderValue::from_str(value)
                .map_err(|_| Error::Request("submission identity is not a valid HTTP header"))?;
            let normalized = value
                .to_str()
                .map_err(|_| Error::Request("submission identity must be textual"))?
                .trim();
            if !valid_identity(normalized) {
                return Err(Error::Request(
                    "submission identity must contain 1..256 bytes without controls",
                ));
            }
            let mut value = HeaderValue::from_str(normalized)
                .map_err(|_| Error::Request("submission identity is not a valid HTTP header"))?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
    }
    Ok(headers)
}

pub(super) fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

pub(super) fn normalized_identity(value: &str) -> bool {
    value.is_ascii() && valid_identity(value) && value.trim() == value
}

pub(super) fn stream(
    actual: bool,
    required: bool,
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<()> {
    if extra.contains_key("stream") {
        return Err(Error::Request(
            "submission extensions must not override the typed stream field",
        ));
    }
    super::super::require_stream(actual, required)
}

pub(super) fn ordinary_responses(request: &ResponsesRequest) -> Result<()> {
    if request.extra.contains_key("bitrouter")
        || request
            .extra
            .get("multi_agent")
            .and_then(|value| value.get("enabled"))
            .is_some_and(|value| value == true)
    {
        return Err(Error::Request(
            "shared submissions do not support managed Responses profiles",
        ));
    }
    Ok(())
}
