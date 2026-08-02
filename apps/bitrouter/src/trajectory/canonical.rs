use anyhow::{Context, Result};
use bitrouter_sdk::language_model::{
    Content, Prompt, Role, ToolResultContentPart, ToolResultOutput,
};
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::Sha256;
use std::collections::BTreeMap;

use super::types::KeyedDigest;

const CANONICAL_PROMPT_VERSION: u32 = 1;
const CORRELATION_KEY_ID_DOMAIN: &[u8] = b"bitrouter.trajectory.correlation.key-id.v1";
const NATIVE_PARENT_DIGEST_DOMAIN: &[u8] = b"bitrouter.trajectory.correlation.native-parent.v1";
const REQUEST_IDENTITY_DOMAIN: &[u8] = b"bitrouter.trajectory.request-identity.v1";
pub(crate) const MAX_ANCESTOR_PREFIX_DIGESTS: usize = 256;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CanonicalWork {
    turn_serializations: usize,
    hmac_input_bytes: usize,
    digest_finalizations: usize,
}

#[cfg(test)]
thread_local! {
    static CANONICAL_WORK: std::cell::Cell<CanonicalWork> = const {
        std::cell::Cell::new(CanonicalWork {
            turn_serializations: 0,
            hmac_input_bytes: 0,
            digest_finalizations: 0,
        })
    };
}

#[cfg(test)]
fn reset_canonical_work() {
    CANONICAL_WORK.set(CanonicalWork::default());
}

#[cfg(test)]
fn canonical_work() -> CanonicalWork {
    CANONICAL_WORK.get()
}

#[cfg(test)]
fn record_turn_serializations(count: usize) {
    CANONICAL_WORK.with(|cell| {
        let mut work = cell.get();
        work.turn_serializations = work.turn_serializations.saturating_add(count);
        cell.set(work);
    });
}

#[cfg(test)]
fn record_hmac_input_work(input_bytes: usize) {
    CANONICAL_WORK.with(|cell| {
        let mut work = cell.get();
        work.hmac_input_bytes = work.hmac_input_bytes.saturating_add(input_bytes);
        cell.set(work);
    });
}

#[cfg(test)]
fn record_digest_finalization() {
    CANONICAL_WORK.with(|cell| {
        let mut work = cell.get();
        work.digest_finalizations = work.digest_finalizations.saturating_add(1);
        cell.set(work);
    });
}

#[derive(Clone)]
pub struct CorrelationKey {
    key_id: String,
    secret: [u8; 32],
}

impl CorrelationKey {
    pub fn from_bytes(secret: [u8; 32]) -> Result<Self> {
        let mut key_fingerprint = Hmac::<Sha256>::new_from_slice(&secret)
            .map_err(|_| anyhow::anyhow!("invalid correlation HMAC key"))?;
        key_fingerprint.update(CORRELATION_KEY_ID_DOMAIN);
        let id_digest = key_fingerprint.finalize().into_bytes();
        Ok(Self {
            key_id: format!("key-{}", hex::encode(&id_digest[..8])),
            secret,
        })
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub(crate) fn request_identity(
        &self,
        owner_user_id: &str,
        external_request_id: &str,
    ) -> Result<String> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .map_err(|_| anyhow::anyhow!("invalid correlation HMAC key"))?;
        mac.update(REQUEST_IDENTITY_DOMAIN);
        mac.update(&[0]);
        mac.update(owner_user_id.as_bytes());
        mac.update(&[0]);
        mac.update(external_request_id.as_bytes());
        Ok(format!(
            "trajectory-request-{}",
            hex::encode(mac.finalize().into_bytes())
        ))
    }
}

pub struct Canonicalizer {
    key: CorrelationKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPromptDigests {
    pub full_input_digest: KeyedDigest,
    pub ancestor_prefix_digests: Vec<KeyedDigest>,
    pub ancestor_prefixes_truncated: bool,
    pub starts_with_prior_turns: bool,
    pub canonical_input_bytes: u64,
}

#[derive(Serialize)]
struct CanonicalPrefix<'a> {
    version: u32,
    system: Option<&'a str>,
    turns: &'a [CanonicalTurn],
}

#[derive(Serialize)]
struct CanonicalTurn {
    role: Role,
    content: Vec<CanonicalContent>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProtocolArtifactKind {
    ToolCall,
    ToolResult,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CanonicalContent {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    File {
        media_type: String,
        data: serde_json::Value,
        filename: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: CanonicalJsonText,
        provider_executed: bool,
        dynamic: bool,
    },
    ToolResult {
        call_id: String,
        output: CanonicalToolResult,
        dynamic: bool,
    },
    Source {
        source: serde_json::Value,
    },
    ToolApprovalRequest {
        approval_id: String,
        tool_call_id: String,
    },
    ToolApprovalResponse {
        approval_id: String,
        approved: bool,
        reason: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum CanonicalJsonText {
    Json(CanonicalJson),
    Text(String),
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum CanonicalToolResult {
    Text(String),
    Json(CanonicalJson),
    ErrorText(String),
    ErrorJson(CanonicalJson),
    Content(Vec<CanonicalToolResultPart>),
    ExecutionDenied(Option<String>),
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CanonicalToolResultPart {
    Text {
        text: String,
    },
    Media {
        media_type: String,
        data: serde_json::Value,
    },
    FileId {
        media_type: Option<String>,
        id: String,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum CanonicalJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<CanonicalJson>),
    Object(BTreeMap<String, CanonicalJson>),
}

impl Canonicalizer {
    pub fn new(key: CorrelationKey) -> Self {
        Self { key }
    }

    pub fn key_id(&self) -> &str {
        self.key.key_id()
    }

    pub(crate) fn request_identity(
        &self,
        owner_user_id: &str,
        external_request_id: &str,
    ) -> Result<String> {
        self.key
            .request_identity(owner_user_id, external_request_id)
    }

    pub(crate) fn native_parent_digest(&self, native_parent_id: &str) -> Result<KeyedDigest> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key.secret)
            .map_err(|_| anyhow::anyhow!("invalid correlation HMAC key"))?;
        mac.update(NATIVE_PARENT_DIGEST_DOMAIN);
        mac.update(&[0]);
        mac.update(native_parent_id.as_bytes());
        let digest = mac.finalize().into_bytes();
        KeyedDigest::parse(format!(
            "hmac-sha256:{}:{}",
            self.key.key_id,
            hex::encode(digest)
        ))
    }

    pub fn canonicalize(&self, prompt: &Prompt) -> Result<CanonicalPromptDigests> {
        let turns = canonical_turns(prompt)?;
        let mut canonical_prefix = serde_json::to_vec(&CanonicalPrefix {
            version: CANONICAL_PROMPT_VERSION,
            system: prompt.system.as_deref(),
            turns: &[],
        })
        .context("serializing canonical prompt envelope")?;
        const CANONICAL_SUFFIX: &[u8] = b"]}";
        if !canonical_prefix.ends_with(CANONICAL_SUFFIX) {
            anyhow::bail!("canonical prompt envelope has an unsupported serialization shape")
        }
        canonical_prefix.truncate(canonical_prefix.len() - CANONICAL_SUFFIX.len());

        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key.secret)
            .map_err(|_| anyhow::anyhow!("invalid correlation HMAC key"))?;
        mac.update(&canonical_prefix);
        #[cfg(test)]
        record_hmac_input_work(canonical_prefix.len());
        let mut canonical_input_bytes = canonical_prefix.len();
        let prefix_capacity = turns
            .len()
            .saturating_sub(1)
            .min(MAX_ANCESTOR_PREFIX_DIGESTS);
        let ancestor_prefixes_truncated =
            turns.len().saturating_sub(1) > MAX_ANCESTOR_PREFIX_DIGESTS;
        let first_retained_end = turns.len().saturating_sub(MAX_ANCESTOR_PREFIX_DIGESTS);
        let mut prefix_digests = Vec::with_capacity(prefix_capacity);

        for (index, turn) in turns.iter().enumerate() {
            if index > 0 {
                mac.update(b",");
                #[cfg(test)]
                record_hmac_input_work(1);
                canonical_input_bytes = canonical_input_bytes
                    .checked_add(1)
                    .context("canonical prompt exceeds byte-count range")?;
            }
            let turn_bytes = serde_json::to_vec(turn).context("serializing canonical turn")?;
            #[cfg(test)]
            record_turn_serializations(1);
            mac.update(&turn_bytes);
            #[cfg(test)]
            record_hmac_input_work(turn_bytes.len());
            canonical_input_bytes = canonical_input_bytes
                .checked_add(turn_bytes.len())
                .context("canonical prompt exceeds byte-count range")?;

            let end = index + 1;
            if end < turns.len() && end >= first_retained_end {
                let mut prefix_mac = mac.clone();
                prefix_mac.update(CANONICAL_SUFFIX);
                #[cfg(test)]
                record_hmac_input_work(CANONICAL_SUFFIX.len());
                prefix_digests.push(self.finish_digest(prefix_mac)?);
            }
        }

        mac.update(CANONICAL_SUFFIX);
        #[cfg(test)]
        record_hmac_input_work(CANONICAL_SUFFIX.len());
        canonical_input_bytes = canonical_input_bytes
            .checked_add(CANONICAL_SUFFIX.len())
            .context("canonical prompt exceeds byte-count range")?;
        let full_input_digest = self.finish_digest(mac)?;
        let canonical_input_bytes = u64::try_from(canonical_input_bytes)
            .context("canonical prompt exceeds byte-count range")?;
        let starts_with_prior_turns = turns
            .iter()
            .any(|turn| matches!(turn.role, Role::Assistant | Role::Tool));
        Ok(CanonicalPromptDigests {
            full_input_digest,
            ancestor_prefix_digests: prefix_digests,
            ancestor_prefixes_truncated,
            starts_with_prior_turns,
            canonical_input_bytes,
        })
    }

    #[cfg(test)]
    fn digest_bytes(&self, bytes: &[u8]) -> Result<KeyedDigest> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key.secret)
            .map_err(|_| anyhow::anyhow!("invalid correlation HMAC key"))?;
        mac.update(bytes);
        record_hmac_input_work(bytes.len());
        self.finish_digest(mac)
    }

    fn finish_digest(&self, mac: Hmac<Sha256>) -> Result<KeyedDigest> {
        #[cfg(test)]
        record_digest_finalization();
        let digest = mac.finalize().into_bytes();
        KeyedDigest::parse(format!(
            "hmac-sha256:{}:{}",
            self.key.key_id,
            hex::encode(digest)
        ))
    }
}

fn canonical_turns(prompt: &Prompt) -> Result<Vec<CanonicalTurn>> {
    let mut turns: Vec<CanonicalTurn> = Vec::new();
    for message in &prompt.messages {
        let mut message_turns: Vec<CanonicalTurn> = Vec::new();
        let has_nonempty_sibling = message.content.len() > 1;
        for content in &message.content {
            if has_nonempty_sibling
                && matches!(content, Content::Text { text, .. } if text.is_empty())
            {
                continue;
            }
            let role = canonical_role(message.role, content);
            let content = canonical_content(content)?;
            if let Some(turn) = message_turns.last_mut()
                && turn.role == role
            {
                turn.content.push(content);
            } else {
                message_turns.push(CanonicalTurn {
                    role,
                    content: vec![content],
                });
            }
        }
        for turn in message_turns {
            let merge_artifact = turns.last().and_then(protocol_artifact_kind)
                == protocol_artifact_kind(&turn)
                && protocol_artifact_kind(&turn).is_some();
            if merge_artifact {
                if let Some(previous) = turns.last_mut() {
                    previous.content.extend(turn.content);
                } else {
                    turns.push(turn);
                }
            } else {
                turns.push(turn);
            }
        }
    }
    Ok(turns)
}

fn protocol_artifact_kind(turn: &CanonicalTurn) -> Option<ProtocolArtifactKind> {
    if turn
        .content
        .iter()
        .all(|content| matches!(content, CanonicalContent::ToolCall { .. }))
    {
        Some(ProtocolArtifactKind::ToolCall)
    } else if turn
        .content
        .iter()
        .all(|content| matches!(content, CanonicalContent::ToolResult { .. }))
    {
        Some(ProtocolArtifactKind::ToolResult)
    } else {
        None
    }
}

fn canonical_role(message_role: Role, content: &Content) -> Role {
    match content {
        Content::ToolCall { .. }
        | Content::Reasoning { .. }
        | Content::ToolApprovalRequest { .. } => Role::Assistant,
        Content::ToolResult { .. } | Content::ToolApprovalResponse { .. } => Role::Tool,
        _ => message_role,
    }
}

fn canonical_content(content: &Content) -> Result<CanonicalContent> {
    Ok(match content {
        Content::Text { text, .. } => CanonicalContent::Text { text: text.clone() },
        Content::Reasoning { text, .. } => CanonicalContent::Reasoning { text: text.clone() },
        Content::File {
            media_type,
            data,
            filename,
            ..
        } => CanonicalContent::File {
            media_type: media_type.clone(),
            data: serde_json::to_value(data).context("canonicalizing file data")?,
            filename: filename.clone(),
        },
        Content::ToolCall {
            id,
            name,
            arguments,
            provider_executed,
            dynamic,
            ..
        } => CanonicalContent::ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: canonical_json_text(arguments),
            provider_executed: *provider_executed,
            dynamic: *dynamic,
        },
        Content::ToolResult {
            call_id,
            output,
            dynamic,
            ..
        } => CanonicalContent::ToolResult {
            call_id: call_id.clone(),
            output: canonical_tool_result(output)?,
            dynamic: *dynamic,
        },
        Content::Source { source, .. } => CanonicalContent::Source {
            source: serde_json::to_value(source).context("canonicalizing source")?,
        },
        Content::ToolApprovalRequest {
            approval_id,
            tool_call_id,
            ..
        } => CanonicalContent::ToolApprovalRequest {
            approval_id: approval_id.clone(),
            tool_call_id: tool_call_id.clone(),
        },
        Content::ToolApprovalResponse {
            approval_id,
            approved,
            reason,
            ..
        } => CanonicalContent::ToolApprovalResponse {
            approval_id: approval_id.clone(),
            approved: *approved,
            reason: reason.clone(),
        },
    })
}

fn canonical_json_text(value: &str) -> CanonicalJsonText {
    serde_json::from_str::<serde_json::Value>(value)
        .map(CanonicalJson::from)
        .map(CanonicalJsonText::Json)
        .unwrap_or_else(|_| CanonicalJsonText::Text(value.to_owned()))
}

fn canonical_tool_result(output: &ToolResultOutput) -> Result<CanonicalToolResult> {
    Ok(match output {
        ToolResultOutput::Text { value } => match canonical_json_text(value) {
            CanonicalJsonText::Json(value) => CanonicalToolResult::Json(value),
            CanonicalJsonText::Text(value) => CanonicalToolResult::Text(value),
        },
        ToolResultOutput::Json { value } => CanonicalToolResult::Json(value.clone().into()),
        ToolResultOutput::ErrorText { value } => match canonical_json_text(value) {
            CanonicalJsonText::Json(value) => CanonicalToolResult::ErrorJson(value),
            CanonicalJsonText::Text(value) => CanonicalToolResult::ErrorText(value),
        },
        ToolResultOutput::ErrorJson { value } => {
            CanonicalToolResult::ErrorJson(value.clone().into())
        }
        ToolResultOutput::Content { value } => CanonicalToolResult::Content(
            value
                .iter()
                .map(|part| match part {
                    ToolResultContentPart::Text { text } => {
                        Ok(CanonicalToolResultPart::Text { text: text.clone() })
                    }
                    ToolResultContentPart::Media { media_type, data } => {
                        Ok(CanonicalToolResultPart::Media {
                            media_type: media_type.clone(),
                            data: serde_json::to_value(data)
                                .context("canonicalizing tool result media")?,
                        })
                    }
                    ToolResultContentPart::FileId { media_type, id } => {
                        Ok(CanonicalToolResultPart::FileId {
                            media_type: media_type.clone(),
                            id: id.clone(),
                        })
                    }
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        ToolResultOutput::ExecutionDenied { reason } => {
            CanonicalToolResult::ExecutionDenied(reason.clone())
        }
    })
}

impl From<serde_json::Value> for CanonicalJson {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(value) => Self::Bool(value),
            serde_json::Value::Number(value) => Self::Number(value),
            serde_json::Value::String(value) => Self::String(value),
            serde_json::Value::Array(values) => {
                Self::Array(values.into_iter().map(Self::from).collect())
            }
            serde_json::Value::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use bitrouter_sdk::language_model::{
        ApiProtocol, Content, Prompt, ToolResultOutput, inbound_adapter_for,
    };

    use super::{
        CANONICAL_PROMPT_VERSION, CanonicalPrefix, Canonicalizer, CorrelationKey,
        MAX_ANCESTOR_PREFIX_DIGESTS, canonical_turns, canonical_work, reset_canonical_work,
    };

    fn parse(protocol: ApiProtocol, body: serde_json::Value) -> anyhow::Result<Prompt> {
        inbound_adapter_for(&protocol)
            .ok_or_else(|| anyhow::anyhow!("missing inbound adapter"))?
            .parse_request(body)
            .map_err(anyhow::Error::from)
    }

    fn equivalent_prompts() -> anyhow::Result<[Prompt; 3]> {
        let chat = parse(
            ApiProtocol::ChatCompletions,
            serde_json::json!({
                "model": "chat-model",
                "messages": [
                    {"role": "system", "content": "Be concise."},
                    {"role": "user", "content": "Plan it."},
                    {"role": "assistant", "content": "First draft."},
                    {"role": "user", "content": "Refine it."}
                ],
                "metadata": {"workflow": "ignored"}
            }),
        )?;
        let messages = parse(
            ApiProtocol::Messages,
            serde_json::json!({
                "model": "messages-model",
                "system": "Be concise.",
                "messages": [
                    {"role": "user", "content": "Plan it."},
                    {"role": "assistant", "content": "First draft."},
                    {"role": "user", "content": "Refine it."}
                ],
                "max_tokens": 128,
                "metadata": {"user_id": "ignored"}
            }),
        )?;
        let responses = parse(
            ApiProtocol::Responses,
            serde_json::json!({
                "model": "responses-model",
                "instructions": "Be concise.",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Plan it."}]},
                    {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "First draft."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Refine it."}]}
                ],
                "metadata": {"task": "ignored"},
                "include": ["reasoning.encrypted_content"]
            }),
        )?;
        Ok([chat, messages, responses])
    }

    fn equivalent_tool_histories() -> anyhow::Result<[Prompt; 3]> {
        let chat = parse(
            ApiProtocol::ChatCompletions,
            serde_json::json!({
                "model": "chat-model",
                "messages": [
                    {"role": "user", "content": "Weather?"},
                    {"role": "assistant", "content": "", "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "weather", "arguments": "{ \"units\" : \"c\", \"city\":\"Paris\" }"}
                    }]},
                    {"role": "tool", "tool_call_id": "call-1", "content": "{\"provider_metadata\":{\"source\":\"sensor\"},\"temp\":21}"},
                    {"role": "user", "content": "And tomorrow?"}
                ]
            }),
        )?;
        let messages = parse(
            ApiProtocol::Messages,
            serde_json::json!({
                "model": "messages-model",
                "max_tokens": 128,
                "messages": [
                    {"role": "user", "content": "Weather?"},
                    {"role": "assistant", "content": [{
                        "type": "tool_use",
                        "id": "call-1",
                        "name": "weather",
                        "input": {"city": "Paris", "units": "c"}
                    }]},
                    {"role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": "call-1",
                        "content": {"temp": 21, "provider_metadata": {"source": "sensor"}}
                    }]},
                    {"role": "user", "content": "And tomorrow?"}
                ]
            }),
        )?;
        let responses = parse(
            ApiProtocol::Responses,
            serde_json::json!({
                "model": "responses-model",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Weather?"}]},
                    {"type": "function_call", "call_id": "call-1", "name": "weather", "arguments": "{\"city\":\"Paris\",\"units\":\"c\"}"},
                    {"type": "function_call_output", "call_id": "call-1", "output": "{ \"temp\" : 21, \"provider_metadata\" : { \"source\" : \"sensor\" } }"},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "And tomorrow?"}]}
                ]
            }),
        )?;
        Ok([chat, messages, responses])
    }

    fn equivalent_multi_tool_histories() -> anyhow::Result<[Prompt; 3]> {
        let chat = parse(
            ApiProtocol::ChatCompletions,
            serde_json::json!({
                "model": "chat-model",
                "messages": [
                    {"role": "user", "content": "Weather?"},
                    {"role": "assistant", "content": "", "tool_calls": [
                        {"id": "call-1", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}},
                        {"id": "call-2", "type": "function", "function": {"name": "weather", "arguments": "{\"city\":\"London\"}"}}
                    ]},
                    {"role": "tool", "tool_call_id": "call-1", "content": "{\"temp\":21}"},
                    {"role": "tool", "tool_call_id": "call-2", "content": "{\"temp\":17}"},
                    {"role": "user", "content": "Compare."}
                ]
            }),
        )?;
        let messages = parse(
            ApiProtocol::Messages,
            serde_json::json!({
                "model": "messages-model",
                "max_tokens": 128,
                "messages": [
                    {"role": "user", "content": "Weather?"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "call-1", "name": "weather", "input": {"city": "Paris"}},
                        {"type": "tool_use", "id": "call-2", "name": "weather", "input": {"city": "London"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "call-1", "content": {"temp": 21}},
                        {"type": "tool_result", "tool_use_id": "call-2", "content": {"temp": 17}}
                    ]},
                    {"role": "user", "content": "Compare."}
                ]
            }),
        )?;
        let responses = parse(
            ApiProtocol::Responses,
            serde_json::json!({
                "model": "responses-model",
                "input": [
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Weather?"}]},
                    {"type": "function_call", "call_id": "call-1", "name": "weather", "arguments": "{\"city\":\"Paris\"}"},
                    {"type": "function_call", "call_id": "call-2", "name": "weather", "arguments": "{\"city\":\"London\"}"},
                    {"type": "function_call_output", "call_id": "call-1", "output": "{\"temp\":21}"},
                    {"type": "function_call_output", "call_id": "call-2", "output": "{\"temp\":17}"},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Compare."}]}
                ]
            }),
        )?;
        Ok([chat, messages, responses])
    }

    #[test]
    fn equivalent_protocol_histories_have_identical_ordered_prefix_digests() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([7; 32])?);
        let [chat, messages, responses] = equivalent_prompts()?;

        let chat = canonicalizer.canonicalize(&chat)?;
        let messages = canonicalizer.canonicalize(&messages)?;
        let responses = canonicalizer.canonicalize(&responses)?;

        assert_eq!(chat.full_input_digest, messages.full_input_digest);
        assert_eq!(messages.full_input_digest, responses.full_input_digest);
        assert_eq!(
            chat.ancestor_prefix_digests,
            messages.ancestor_prefix_digests
        );
        assert_eq!(
            messages.ancestor_prefix_digests,
            responses.ancestor_prefix_digests
        );
        assert_eq!(chat.ancestor_prefix_digests.len(), 2);
        Ok(())
    }

    #[test]
    fn provider_and_workflow_metadata_do_not_change_digests_but_ancestry_does() -> anyhow::Result<()>
    {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([9; 32])?);
        let [baseline, _, _] = equivalent_prompts()?;
        let mut metadata_changed = baseline.clone();
        metadata_changed.model = "another-provider:model".into();
        metadata_changed.stream = true;
        metadata_changed.params.extra.insert(
            "x-bitrouter-task-id".into(),
            serde_json::json!("task-other"),
        );
        metadata_changed.params.extra.insert(
            "x-superpowers-workflow".into(),
            serde_json::json!("workflow-other"),
        );
        if let Some(first) = metadata_changed.messages.first_mut()
            && let Some(bitrouter_sdk::language_model::Content::Text {
                provider_metadata, ..
            }) = first.content.first_mut()
        {
            provider_metadata.insert(
                "anthropic".into(),
                serde_json::json!({"cacheControl": {"type": "ephemeral"}}),
            );
        }
        let mut ancestry_changed = baseline.clone();
        ancestry_changed.messages[1] = bitrouter_sdk::language_model::Message::text(
            bitrouter_sdk::language_model::Role::Assistant,
            "Different draft.",
        );

        let baseline = canonicalizer.canonicalize(&baseline)?;
        assert_eq!(baseline, canonicalizer.canonicalize(&metadata_changed)?);
        assert_ne!(
            baseline.full_input_digest,
            canonicalizer
                .canonicalize(&ancestry_changed)?
                .full_input_digest
        );
        Ok(())
    }

    #[test]
    fn keyed_digests_are_secret_scoped_and_stable_for_one_key() -> anyhow::Result<()> {
        let [prompt, _, _] = equivalent_prompts()?;
        let first = Canonicalizer::new(CorrelationKey::from_bytes([11; 32])?);
        let restarted = Canonicalizer::new(CorrelationKey::from_bytes([11; 32])?);
        let other = Canonicalizer::new(CorrelationKey::from_bytes([12; 32])?);

        assert_eq!(
            first.canonicalize(&prompt)?,
            restarted.canonicalize(&prompt)?
        );
        assert_ne!(
            first.canonicalize(&prompt)?.full_input_digest,
            other.canonicalize(&prompt)?.full_input_digest
        );
        Ok(())
    }

    #[test]
    fn native_parent_digest_is_truthfully_keyed_and_key_bound() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([19; 32])?);
        let digest = canonicalizer.native_parent_digest("response-parent")?;

        assert_eq!(digest.key_id(), canonicalizer.key_id());
        assert!(digest.as_str().starts_with("hmac-sha256:"));
        assert!(!digest.as_str().contains("response-parent"));
        Ok(())
    }

    #[test]
    fn request_identity_is_stable_install_keyed_owner_bound_and_opaque() -> anyhow::Result<()> {
        let raw = "SECRET-task-label-request-header";
        let key = CorrelationKey::from_bytes([27; 32])?;
        let stable = key.request_identity("owner-a", raw)?;
        assert_eq!(stable, key.request_identity("owner-a", raw)?);
        assert_ne!(stable, key.request_identity("owner-b", raw)?);
        assert_ne!(
            stable,
            CorrelationKey::from_bytes([28; 32])?.request_identity("owner-a", raw)?
        );
        assert!(stable.starts_with("trajectory-request-"));
        assert!(!stable.contains(raw));
        assert!(!stable.contains("SECRET"));
        assert!(!stable.contains("task-label"));
        Ok(())
    }

    #[test]
    fn equivalent_cross_protocol_tool_histories_share_digests() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([13; 32])?);
        let [chat, messages, responses] = equivalent_tool_histories()?;

        let chat = canonicalizer.canonicalize(&chat)?;
        assert_eq!(chat, canonicalizer.canonicalize(&messages)?);
        assert_eq!(chat, canonicalizer.canonicalize(&responses)?);
        Ok(())
    }

    #[test]
    fn semantic_message_boundaries_affect_digests_and_prefixes() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([14; 32])?);
        let one_multi_block = parse(
            ApiProtocol::Messages,
            serde_json::json!({
                "model": "messages-model",
                "max_tokens": 128,
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "A"},
                    {"type": "text", "text": "B"}
                ]}]
            }),
        )?;
        let two_messages = parse(
            ApiProtocol::Messages,
            serde_json::json!({
                "model": "messages-model",
                "max_tokens": 128,
                "messages": [
                    {"role": "user", "content": "A"},
                    {"role": "user", "content": "B"}
                ]
            }),
        )?;

        let one = canonicalizer.canonicalize(&one_multi_block)?;
        let two = canonicalizer.canonicalize(&two_messages)?;
        assert_ne!(one.full_input_digest, two.full_input_digest);
        assert_eq!(two.ancestor_prefix_digests.len(), 1);
        assert_eq!(
            two.ancestor_prefix_digests[0],
            canonicalizer
                .canonicalize(&parse(
                    ApiProtocol::Messages,
                    serde_json::json!({
                        "model": "messages-model",
                        "max_tokens": 128,
                        "messages": [{"role": "user", "content": "A"}]
                    }),
                )?)?
                .full_input_digest
        );
        Ok(())
    }

    #[test]
    fn adjacent_same_role_semantic_messages_remain_distinct() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([16; 32])?);
        for role in ["user", "assistant"] {
            let prompt = parse(
                ApiProtocol::ChatCompletions,
                serde_json::json!({
                    "model": "chat-model",
                    "messages": [
                        {"role": role, "content": "first"},
                        {"role": role, "content": "second"}
                    ]
                }),
            )?;
            assert_eq!(
                canonicalizer
                    .canonicalize(&prompt)?
                    .ancestor_prefix_digests
                    .len(),
                1,
                "{role} messages were flattened"
            );
        }
        Ok(())
    }

    #[test]
    fn equivalent_multi_tool_histories_merge_only_protocol_artifact_fragments() -> anyhow::Result<()>
    {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([18; 32])?);
        let [chat, messages, responses] = equivalent_multi_tool_histories()?;
        let chat = canonicalizer.canonicalize(&chat)?;
        assert_eq!(chat, canonicalizer.canonicalize(&messages)?);
        assert_eq!(chat, canonicalizer.canonicalize(&responses)?);
        Ok(())
    }

    #[test]
    fn parseable_tool_json_ignores_key_order_and_whitespace() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([15; 32])?);
        let [mut baseline, _, _] = equivalent_tool_histories()?;
        let mut reordered = baseline.clone();
        for content in &mut reordered.messages[1].content {
            if let Content::ToolCall { arguments, .. } = content {
                *arguments = "{\n  \"city\" : \"Paris\", \"units\": \"c\"\n}".into();
            }
        }
        if let Content::ToolResult { output, .. } = &mut baseline.messages[2].content[0] {
            *output = ToolResultOutput::Text {
                value: "{\"temp\":21,\"provider_metadata\":{\"source\":\"sensor\"}}".into(),
            };
        }
        if let Content::ToolResult { output, .. } = &mut reordered.messages[2].content[0] {
            *output = ToolResultOutput::Text {
                value: "{ \"provider_metadata\": { \"source\": \"sensor\" }, \"temp\" : 21 }"
                    .into(),
            };
        }

        assert_eq!(
            canonicalizer.canonicalize(&baseline)?,
            canonicalizer.canonicalize(&reordered)?
        );
        Ok(())
    }

    #[test]
    fn user_and_tool_json_provider_metadata_keys_are_semantic() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([17; 32])?);
        let [mut baseline, _, _] = equivalent_tool_histories()?;
        if let Content::ToolResult { output, .. } = &mut baseline.messages[2].content[0] {
            *output = ToolResultOutput::Json {
                value: serde_json::json!({
                    "temp": 21,
                    "provider_metadata": {"source": "sensor"}
                }),
            };
        }
        if let Content::Text { text, .. } = &mut baseline.messages[0].content[0] {
            *text = "{\"provider_metadata\":{\"user\":\"baseline\"}}".into();
        }
        let mut changed_tool_json = baseline.clone();
        if let Content::ToolResult { output, .. } = &mut changed_tool_json.messages[2].content[0] {
            *output = ToolResultOutput::Json {
                value: serde_json::json!({
                    "temp": 21,
                    "provider_metadata": {"source": "different"}
                }),
            };
        }
        let mut changed_user_json = baseline.clone();
        if let Content::Text { text, .. } = &mut changed_user_json.messages[0].content[0] {
            *text = "{\"provider_metadata\":{\"user\":\"changed\"}}".into();
        }

        let baseline = canonicalizer.canonicalize(&baseline)?;
        assert_ne!(baseline, canonicalizer.canonicalize(&changed_tool_json)?);
        assert_ne!(baseline, canonicalizer.canonicalize(&changed_user_json)?);
        Ok(())
    }

    #[test]
    fn canonicalizer_reports_exact_full_input_byte_count_without_content() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([29; 32])?);
        let prompt = parse(
            ApiProtocol::ChatCompletions,
            serde_json::json!({
                "model": "provider/model",
                "messages": [{"role": "user", "content": "hello"}]
            }),
        )?;

        let canonical = canonicalizer.canonicalize(&prompt)?;

        assert_eq!(canonical.canonical_input_bytes, 96);
        assert_eq!(canonical.full_input_digest.as_str().len(), 97);
        Ok(())
    }

    #[test]
    fn incremental_prefix_digests_remain_byte_compatible_with_v1_json() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([31; 32])?);
        let prompt = parse(
            ApiProtocol::ChatCompletions,
            serde_json::json!({
                "model": "provider/model",
                "messages": [
                    {"role": "user", "content": "first"},
                    {"role": "assistant", "content": "second"},
                    {"role": "user", "content": "third"}
                ]
            }),
        )?;
        let turns = canonical_turns(&prompt)?;
        let canonical = canonicalizer.canonicalize(&prompt)?;

        let expected_prefixes = (1..turns.len())
            .map(|end| {
                serde_json::to_vec(&CanonicalPrefix {
                    version: CANONICAL_PROMPT_VERSION,
                    system: prompt.system.as_deref(),
                    turns: &turns[..end],
                })
                .map_err(anyhow::Error::from)
                .and_then(|bytes| canonicalizer.digest_bytes(&bytes))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let full = serde_json::to_vec(&CanonicalPrefix {
            version: CANONICAL_PROMPT_VERSION,
            system: prompt.system.as_deref(),
            turns: &turns,
        })?;

        assert_eq!(canonical.ancestor_prefix_digests, expected_prefixes);
        assert!(!canonical.ancestor_prefixes_truncated);
        assert_eq!(
            canonical.full_input_digest,
            canonicalizer.digest_bytes(&full)?
        );
        assert_eq!(canonical.canonical_input_bytes, u64::try_from(full.len())?);
        Ok(())
    }

    #[test]
    fn canonical_prefix_work_is_linear_and_emits_at_most_256_newest_digests() -> anyhow::Result<()>
    {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([32; 32])?);
        for turn_count in [32_usize, 64, 128, 1024] {
            let messages = (0..turn_count)
                .map(|index| {
                    serde_json::json!({
                        "role": if index % 2 == 0 { "user" } else { "assistant" },
                        "content": format!("semantic-turn-{index:04}")
                    })
                })
                .collect::<Vec<_>>();
            let prompt = parse(
                ApiProtocol::ChatCompletions,
                serde_json::json!({"model": "provider/model", "messages": messages}),
            )?;

            reset_canonical_work();
            let canonical = canonicalizer.canonicalize(&prompt)?;
            let work = canonical_work();
            let expected_prefixes = turn_count.saturating_sub(1).min(256);

            assert_eq!(
                canonical.ancestor_prefix_digests.len(),
                expected_prefixes,
                "prefix bound changed for {turn_count} turns"
            );
            assert_eq!(
                canonical.ancestor_prefixes_truncated,
                turn_count.saturating_sub(1) > MAX_ANCESTOR_PREFIX_DIGESTS,
                "prefix truncation evidence changed for {turn_count} turns"
            );
            assert_eq!(
                work.turn_serializations, turn_count,
                "typed turns were serialized more than once for {turn_count} turns"
            );
            assert_eq!(work.digest_finalizations, expected_prefixes + 1);
            assert_eq!(
                work.hmac_input_bytes,
                usize::try_from(canonical.canonical_input_bytes)? + 2 * expected_prefixes,
                "HMAC work reprocessed a prior canonical prefix for {turn_count} turns"
            );
        }
        Ok(())
    }
}
