use anyhow::{Context, Result};
use bitrouter_sdk::language_model::{Content, Prompt, Role};
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::types::KeyedDigest;

const CANONICAL_PROMPT_VERSION: u32 = 1;

#[derive(Clone)]
pub struct CorrelationKey {
    key_id: String,
    secret: [u8; 32],
}

impl CorrelationKey {
    pub fn from_bytes(installation_id: &str, secret: [u8; 32]) -> Result<Self> {
        if installation_id.trim().is_empty() {
            anyhow::bail!("correlation key requires an installation id")
        }
        let mut key_fingerprint = Sha256::new();
        key_fingerprint.update(installation_id.as_bytes());
        key_fingerprint.update([0]);
        key_fingerprint.update(secret);
        let id_digest = key_fingerprint.finalize();
        Ok(Self {
            key_id: format!("install-{}", hex::encode(&id_digest[..8])),
            secret,
        })
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }
}

pub struct Canonicalizer {
    key: CorrelationKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPromptDigests {
    pub full_input_digest: KeyedDigest,
    pub ancestor_prefix_digests: Vec<KeyedDigest>,
    pub starts_with_prior_turns: bool,
}

#[derive(Serialize)]
struct CanonicalPrefix<'a> {
    version: u32,
    system: Option<&'a str>,
    messages: &'a [CanonicalMessage],
}

#[derive(Serialize)]
struct CanonicalMessage {
    role: Role,
    content: Vec<serde_json::Value>,
}

impl Canonicalizer {
    pub fn new(key: CorrelationKey) -> Self {
        Self { key }
    }

    pub fn key_id(&self) -> &str {
        self.key.key_id()
    }

    pub fn canonicalize(&self, prompt: &Prompt) -> Result<CanonicalPromptDigests> {
        let messages = prompt
            .messages
            .iter()
            .map(|message| {
                let content = message
                    .content
                    .iter()
                    .map(canonical_content)
                    .collect::<Result<Vec<_>>>()?;
                Ok(CanonicalMessage {
                    role: message.role,
                    content,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut prefix_digests = Vec::with_capacity(messages.len());
        if messages.is_empty() {
            prefix_digests.push(self.digest(&CanonicalPrefix {
                version: CANONICAL_PROMPT_VERSION,
                system: prompt.system.as_deref(),
                messages: &[],
            })?);
        } else {
            for end in 1..=messages.len() {
                prefix_digests.push(self.digest(&CanonicalPrefix {
                    version: CANONICAL_PROMPT_VERSION,
                    system: prompt.system.as_deref(),
                    messages: &messages[..end],
                })?);
            }
        }
        let full_input_digest = prefix_digests
            .pop()
            .ok_or_else(|| anyhow::anyhow!("canonical prompt produced no full digest"))?;
        let starts_with_prior_turns = prompt
            .messages
            .iter()
            .any(|message| matches!(message.role, Role::Assistant | Role::Tool));
        Ok(CanonicalPromptDigests {
            full_input_digest,
            ancestor_prefix_digests: prefix_digests,
            starts_with_prior_turns,
        })
    }

    fn digest<T: Serialize>(&self, value: &T) -> Result<KeyedDigest> {
        let bytes = serde_json::to_vec(value).context("serializing canonical prompt prefix")?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key.secret)
            .map_err(|_| anyhow::anyhow!("invalid correlation HMAC key"))?;
        mac.update(&bytes);
        let digest = mac.finalize().into_bytes();
        KeyedDigest::parse(format!(
            "hmac-sha256:{}:{}",
            self.key.key_id,
            hex::encode(digest)
        ))
    }
}

fn canonical_content(content: &Content) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(content).context("canonicalizing prompt content")?;
    remove_provider_metadata(&mut value);
    Ok(value)
}

fn remove_provider_metadata(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.remove("provider_metadata");
            for child in object.values_mut() {
                remove_provider_metadata(child);
            }
        }
        serde_json::Value::Array(array) => {
            for child in array {
                remove_provider_metadata(child);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use bitrouter_sdk::language_model::{ApiProtocol, Prompt, inbound_adapter_for};

    use super::{Canonicalizer, CorrelationKey};

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

    #[test]
    fn equivalent_protocol_histories_have_identical_ordered_prefix_digests() -> anyhow::Result<()> {
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes("install-a", [7; 32])?);
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
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes("install-a", [9; 32])?);
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
    fn keyed_digests_are_installation_scoped_and_stable_for_one_key() -> anyhow::Result<()> {
        let [prompt, _, _] = equivalent_prompts()?;
        let first = Canonicalizer::new(CorrelationKey::from_bytes("install-a", [11; 32])?);
        let restarted = Canonicalizer::new(CorrelationKey::from_bytes("install-a", [11; 32])?);
        let other = Canonicalizer::new(CorrelationKey::from_bytes("install-b", [12; 32])?);

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
}
