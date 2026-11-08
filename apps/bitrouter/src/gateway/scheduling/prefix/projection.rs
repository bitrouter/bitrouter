//! Adapter-owned semantic UTF-8 framing, not a provider tokenization claim.
//! Read at most the configured prefix's string bytes; count discarded tails by
//! length, with a separate structural-visit budget equal to that byte cap.
use bitrouter_ai::types::*;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;

pub(super) type Digest = [u8; 32];
pub(crate) struct Projection {
    pub(super) blocks: Vec<Digest>,
    pub(super) bytes: u64,
    pub(super) protocol: usize,
}
impl Projection {
    pub(super) fn retained_bytes(&self) -> u64 {
        (std::mem::size_of::<Self>() + 32 + self.blocks.capacity() * 32) as u64
    }
}
pub(super) fn mac(key: &Digest) -> Hmac<Sha256> {
    let mut padded = [0u8; 64];
    padded[..32].copy_from_slice(key);
    <Hmac<Sha256> as hmac::digest::KeyInit>::new((&padded).into())
}
pub(super) fn digest(key: &Digest, parts: &[&[u8]]) -> Digest {
    // HMAC accepts a key of every length; use the infallible KeyInit API.
    let mut mac = mac(key);
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}
struct Frames<'a> {
    key: &'a Digest,
    previous: Digest,
    buffer: [u8; 256],
    buffered: usize,
    blocks: Vec<Digest>,
    cap: usize,
    bytes: u64,
    visits: usize,
}
type Result<T> = std::result::Result<T, ()>;
impl Frames<'_> {
    fn visit(&mut self) -> Result<()> {
        self.visits += 1;
        if self.visits > self.cap {
            return Err(());
        }
        Ok(())
    }
    fn raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes = self.bytes.checked_add(bytes.len() as u64).ok_or(())?;
        let mut bytes = bytes;
        while !bytes.is_empty() && self.blocks.len() < self.cap / 256 {
            let take = bytes.len().min(256 - self.buffered);
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&bytes[..take]);
            self.buffered += take;
            bytes = &bytes[take..];
            if self.buffered == 256 {
                self.previous = digest(
                    self.key,
                    &[b"bitrouter.prefix.block.v1", &self.previous, &self.buffer],
                );
                self.blocks.push(self.previous);
                self.buffered = 0;
            }
        }
        Ok(())
    }
    fn text(&mut self, value: &str) -> Result<()> {
        self.visit()?;
        self.raw(b"s")?;
        self.raw(value.len().to_string().as_bytes())?;
        self.raw(b":")?;
        self.raw(value.as_bytes())
    }
    fn scalar<T: serde::Serialize>(&mut self, value: T) -> Result<()> {
        self.text(&serde_json::to_string(&value).map_err(|_| ())?)
    }
    fn optional(&mut self, value: &Option<String>) -> Result<()> {
        self.scalar(value.is_some())?;
        if let Some(value) = value {
            self.text(value)?;
        }
        Ok(())
    }
    fn json(&mut self, value: &serde_json::Value, depth: usize) -> Result<()> {
        self.visit()?;
        if depth > 64 {
            return Err(());
        }
        match value {
            serde_json::Value::Null => self.raw(b"n"),
            serde_json::Value::Bool(v) => self.raw(if *v { b"t" } else { b"f" }),
            serde_json::Value::Number(v) => {
                self.raw(b"d")?;
                self.text(&v.to_string())
            }
            serde_json::Value::String(v) => self.text(v),
            serde_json::Value::Array(v) => {
                self.raw(b"[")?;
                for item in v {
                    self.json(item, depth + 1)?;
                }
                self.raw(b"]")
            }
            serde_json::Value::Object(v) => {
                self.raw(b"{")?;
                for (key, value) in v {
                    self.text(key)?;
                    self.json(value, depth + 1)?;
                }
                self.raw(b"}")
            }
        }
    }
    fn metadata(&mut self, value: &ProviderMetadata) -> Result<()> {
        // The only v1 provider metadata projection is the typed Messages cache
        // breakpoint. Other replay/signature/provider extensions disable affinity.
        if !value.is_empty() {
            if value.len() != 1 {
                return Err(());
            }
            let fields = value
                .get("anthropic")
                .and_then(serde_json::Value::as_object)
                .ok_or(())?;
            if fields.len() != 1 {
                return Err(());
            }
            let control = fields
                .get("cacheControl")
                .and_then(serde_json::Value::as_object)
                .ok_or(())?;
            if control.len() > 2
                || control.get("type").and_then(serde_json::Value::as_str) != Some("ephemeral")
                || control.keys().any(|k| k != "type" && k != "ttl")
                || control
                    .get("ttl")
                    .is_some_and(|v| !matches!(v.as_str(), Some("5m" | "1h")))
            {
                return Err(());
            }
        }
        self.raw(b"{")?;
        for (key, value) in value {
            self.text(key)?;
            self.json(value, 0)?;
        }
        self.raw(b"}")
    }
    fn prompt(&mut self, prompt: &Prompt) -> Result<()> {
        let Prompt {
            model: _,
            system,
            system_provider_metadata,
            messages,
            tools,
            params,
            response_format,
            tool_choice,
            stream: _,
        } = prompt;
        let GenerationParams {
            temperature,
            top_p,
            max_tokens,
            chat_token_limit_field,
            reasoning_effort,
            reasoning_effort_source: _,
            response_modalities,
            top_k,
            seed,
            stop,
            presence_penalty,
            frequency_penalty,
            store: _,
            parallel_tool_calls,
            chat_stream_options: _,
            extra_protocol: _,
            supplemental_extra,
            extra,
        } = params;
        if !extra.is_empty()
            || !supplemental_extra.is_empty()
            || response_modalities.len() > self.cap
            || response_modalities.iter().any(|v| *v != Modality::Text)
        {
            return Err(());
        }
        if [temperature, top_p, presence_penalty, frequency_penalty]
            .into_iter()
            .any(|v| v.is_some_and(|v| !v.is_finite()))
        {
            return Err(());
        }
        // Conservative input-affecting controls precede any reusable content.
        self.text("controls")?;
        self.scalar((
            temperature,
            top_p,
            max_tokens,
            chat_token_limit_field,
            reasoning_effort,
            top_k,
            seed,
            presence_penalty,
            frequency_penalty,
            parallel_tool_calls,
        ))?;
        self.scalar(response_modalities.len())?;
        for modality in response_modalities {
            self.scalar(modality)?;
        }
        self.scalar(stop.len())?;
        for value in stop {
            self.text(value)?;
        }
        self.scalar(response_format.is_some())?;
        if let Some(format) = response_format {
            match format {
                ResponseFormat::JsonSchema {
                    name,
                    description,
                    strict,
                    schema,
                } => {
                    self.text("json_schema")?;
                    self.optional(name)?;
                    self.optional(description)?;
                    self.scalar(strict)?;
                    self.json(schema, 0)?;
                }
            }
        }
        match tool_choice {
            None => self.text("unset")?,
            Some(ToolChoice::Tool { name }) => {
                self.text("named")?;
                self.text(name)?;
            }
            Some(value) => self.scalar(value)?,
        }
        self.text("tools")?;
        self.scalar(tools.len())?;
        for tool in tools {
            self.visit()?;
            let Tool::Function {
                name,
                description,
                parameters,
                strict,
                provider_metadata,
            } = tool
            else {
                return Err(());
            };
            self.text(name)?;
            self.optional(description)?;
            self.scalar(strict)?;
            self.json(parameters, 0)?;
            self.metadata(provider_metadata)?;
        }
        self.text("system")?;
        self.optional(system)?;
        self.metadata(system_provider_metadata)?;
        self.text("messages")?;
        // Do not prefix the list with its final length: appending a turn must
        // preserve earlier blocks. Typed delimiters still disambiguate boundaries.
        self.raw(b"[")?;
        for message in messages {
            let Message { role, content } = message;
            self.scalar(role)?;
            self.raw(b"[")?;
            for part in content {
                self.visit()?;
                let Content::Text {
                    text,
                    provider_metadata,
                } = part
                else {
                    return Err(());
                };
                self.text(text)?;
                self.metadata(provider_metadata)?;
            }
            self.raw(b"]")?;
        }
        self.raw(b"]")
    }
}

pub(crate) fn build(
    prompt: &Prompt,
    key: &Digest,
    tenant: &Digest,
    cap: usize,
    version: &[u8],
    protocol: usize,
) -> Option<Arc<Projection>> {
    if protocol >= 3 || !(256..=1_048_576).contains(&cap) {
        return None;
    }
    let mut frames = Frames {
        key,
        previous: digest(key, &[b"bitrouter.prefix.seed.v1", tenant, version]),
        buffer: [0; 256],
        buffered: 0,
        blocks: Vec::with_capacity(cap / 256),
        cap,
        bytes: 0,
        visits: 0,
    };
    let valid = frames.prompt(prompt).is_ok();
    frames.buffer.fill(0);
    if !valid || frames.blocks.is_empty() {
        return None;
    }
    Some(Arc::new(Projection {
        blocks: frames.blocks,
        bytes: frames.bytes,
        protocol,
    }))
}
