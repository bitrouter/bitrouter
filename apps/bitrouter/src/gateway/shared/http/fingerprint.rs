//! Versioned, configuration-independent identity of an ingress submission.

use std::io::Write;

use bitrouter_ai::types::ApiProtocol;
use http::HeaderMap;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::gateway::error::BitrouterError;

// These fields affect shipped prompt transforms, policy/experiment selectors,
// session identity or upstream account scope. Additions require a new review of
// the v1 contract and all passthrough consumers; never derive this list from the
// currently selected provider or mutable publication.
const HEADERS: &[&str] = &[
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

pub(in crate::gateway::shared) fn allows_header(name: &str) -> bool {
    HEADERS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
}

pub(super) fn request(
    inbound: &ApiProtocol,
    headers: &HeaderMap,
    body: &Value,
) -> Result<String, BitrouterError> {
    fingerprint(inbound, headers, body)
        .map_err(|_| BitrouterError::internal("cannot fingerprint shared request"))
}

fn fingerprint(inbound: &ApiProtocol, headers: &HeaderMap, body: &Value) -> anyhow::Result<String> {
    let endpoint = match inbound {
        ApiProtocol::Messages => "/v1/messages",
        ApiProtocol::ChatCompletions => "/v1/chat/completions",
        ApiProtocol::Responses => "/v1/responses",
        ApiProtocol::Decisions => "/v1/decisions",
        _ => anyhow::bail!("unsupported shared endpoint"),
    };
    let mut writer = HashWriter(Sha256::new());
    writer.write_all(b"[\"gateway-request-v1\",")?;
    serde_json::to_writer(&mut writer, endpoint)?;
    writer.write_all(b",[")?;
    for (index, name) in HEADERS.iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        writer.write_all(b"[")?;
        serde_json::to_writer(&mut writer, name)?;
        writer.write_all(b",[")?;
        // Preserve duplicate multiplicity, occurrence order and raw bytes.
        // Absence is [], and one empty value is [[]], so they cannot collide.
        // https://www.rfc-editor.org/rfc/rfc9110.html#section-5.2
        for (index, value) in headers.get_all(*name).iter().enumerate() {
            if index > 0 {
                writer.write_all(b",")?;
            }
            serde_json::to_writer(&mut writer, value.as_bytes())?;
        }
        writer.write_all(b"]]")?;
    }
    writer.write_all(b"],")?;
    canonical(&mut writer, body)?;
    writer.write_all(b"]")?;
    Ok(hex::encode(writer.0.finalize()))
}

/// Sort object keys recursively without cloning the complete body or allocating
/// its serialized representation. Array order and JSON scalar values are kept.
fn canonical(writer: &mut HashWriter, value: &Value) -> anyhow::Result<()> {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            writer.write_all(b"{")?;
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",")?;
                }
                serde_json::to_writer(&mut *writer, key)?;
                writer.write_all(b":")?;
                canonical(writer, value)?;
            }
            writer.write_all(b"}")?;
        }
        Value::Array(values) => {
            writer.write_all(b"[")?;
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",")?;
                }
                canonical(writer, value)?;
            }
            writer.write_all(b"]")?;
        }
        scalar => serde_json::to_writer(writer, scalar)?,
    }
    Ok(())
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn v1_published_encoding_vector() -> anyhow::Result<()> {
        let body: Value = serde_json::from_str(
            r#"{"seed":7,"model":"fixture","input":["雪",{"z":null,"a":false}]}"#,
        )?;
        let mut headers = HeaderMap::new();
        headers.append("openai-project", HeaderValue::from_static("p1"));
        headers.append("openai-project", HeaderValue::from_static("p2"));
        headers.append("session-id", HeaderValue::from_static(""));
        assert_eq!(
            request(&ApiProtocol::Responses, &headers, &body)?,
            "b8140196b302d60f648816e372820e1c133e96d73d6158df89b172b557972777"
        );
        Ok(())
    }

    #[test]
    fn canonical_json_and_ignored_headers_preserve_identity() -> anyhow::Result<()> {
        let first: Value = serde_json::from_str(
            r#"{"model":"fixture","messages":[{"role":"user","content":"hi"}],"temperature":0.5}"#,
        )?;
        let second: Value = serde_json::from_str(
            r#"{"temperature":0.5,"messages":[{"content":"hi","role":"user"}],"model":"fixture"}"#,
        )?;
        let mut headers = HeaderMap::new();
        let expected = request(&ApiProtocol::ChatCompletions, &headers, &first)?;
        for name in [
            "authorization",
            "x-api-key",
            "x-bitrouter-request-id",
            "idempotency-key",
            "traceparent",
            "tracestate",
            "baggage",
            "x-unlisted",
        ] {
            headers.insert(name, HeaderValue::from_static("changed"));
        }
        assert_eq!(
            expected,
            request(&ApiProtocol::ChatCompletions, &headers, &second)?
        );
        assert_ne!(
            expected,
            request(&ApiProtocol::Messages, &headers, &second)?
        );
        let mut changed = second;
        changed["temperature"] = serde_json::json!(0.6);
        assert_ne!(
            expected,
            request(&ApiProtocol::ChatCompletions, &headers, &changed)?
        );
        Ok(())
    }

    #[test]
    fn each_semantic_header_and_duplicate_order_is_bound() -> anyhow::Result<()> {
        let body = serde_json::json!({"model":"fixture"});
        let protocol = ApiProtocol::ChatCompletions;
        let absent = request(&protocol, &HeaderMap::new(), &body)?;
        for name in HEADERS {
            let mut headers = HeaderMap::new();
            headers.append(*name, HeaderValue::from_static(""));
            assert_ne!(absent, request(&protocol, &headers, &body)?, "{name}");
            headers.insert(*name, HeaderValue::from_static("first"));
            headers.append(*name, HeaderValue::from_static("second"));
            let ordered = request(&protocol, &headers, &body)?;
            headers.insert(*name, HeaderValue::from_static("second"));
            headers.append(*name, HeaderValue::from_static("first"));
            assert_ne!(ordered, request(&protocol, &headers, &body)?, "{name}");
            assert!(allows_header(&name.to_ascii_uppercase()));
        }
        assert!(!allows_header("authorization"));
        assert!(!allows_header("x-bitrouter-request-id"));
        assert!(HEADERS.windows(2).all(|pair| pair[0] < pair[1]));
        Ok(())
    }
}
