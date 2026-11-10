//! Conservative owned-payload accounting. No encoded prompt copy is retained.
//! JSON object storage is compacted before the frozen handoff because serde_json
//! does not expose map capacity. String/Vec/HashMap spare capacity is charged.

use crate::gateway::error::{BitrouterError, Result};
use bitrouter_ai::types::*;
use std::{
    collections::BTreeMap,
    mem::{size_of, size_of_val},
};

pub(crate) struct Weight {
    bytes: u64,
    visits: usize,
}
impl Weight {
    pub(crate) fn new() -> Self {
        Self {
            bytes: 0,
            visits: 0,
        }
    }
    pub(crate) fn add(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes as u64)
            .ok_or_else(super::saturated)?;
        Ok(())
    }
    fn visit(&mut self) -> Result<()> {
        self.visits += 1;
        if self.visits > 1_000_000 {
            return Err(super::saturated());
        }
        Ok(())
    }
    pub(crate) fn total(&self) -> u64 {
        self.bytes
    }
    pub(crate) fn string(&mut self, value: &String) -> Result<()> {
        self.visit()?;
        self.add(value.capacity())
    }
    fn optional(&mut self, value: &Option<String>) -> Result<()> {
        if let Some(s) = value {
            self.string(s)?;
        }
        Ok(())
    }
    fn array<T>(&mut self, value: &Vec<T>) -> Result<()> {
        self.visit()?;
        self.add(
            value
                .capacity()
                .checked_mul(size_of::<T>())
                .ok_or_else(super::saturated)?,
        )
    }
    pub(crate) fn json(&mut self, value: &mut serde_json::Value) -> Result<()> {
        self.json_at(value, 0)
    }
    fn json_at(&mut self, value: &mut serde_json::Value, depth: usize) -> Result<()> {
        self.visit()?;
        if depth > 64 {
            return Err(super::saturated());
        }
        match value {
            serde_json::Value::String(value) => self.string(value)?,
            serde_json::Value::Array(values) => {
                self.array(values)?;
                for value in values {
                    self.json_at(value, depth + 1)?;
                }
            }
            serde_json::Value::Object(values) => {
                // Moving entries preserves order and values, and discards opaque
                // grow/remove capacity. Fresh map storage has bounded slack.
                let old = std::mem::take(values);
                for (key, mut value) in old {
                    self.add(256)?;
                    self.string(&key)?;
                    self.json_at(&mut value, depth + 1)?;
                    values.insert(key, value);
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn metadata(&mut self, values: &mut BTreeMap<String, serde_json::Value>) -> Result<()> {
        for (key, value) in values {
            self.add(256)?;
            self.string(key)?;
            self.json(value)?;
        }
        Ok(())
    }
    fn data(&mut self, value: &DataContent) -> Result<()> {
        match value {
            DataContent::Base64 { data } => self.string(data),
            DataContent::Url { url } => self.string(url),
        }
    }
    fn content(&mut self, value: &mut Content) -> Result<()> {
        self.visit()?;
        match value {
            Content::Text {
                text,
                provider_metadata,
            } => {
                self.string(text)?;
                self.metadata(provider_metadata)?;
            }
            Content::Reasoning {
                text,
                provider_metadata,
                native,
            } => {
                self.string(text)?;
                self.metadata(provider_metadata)?;
                if let Some(NativeReasoning::Responses(value)) = native {
                    self.json(value)?;
                }
            }
            Content::File {
                media_type,
                data,
                filename,
                provider_metadata,
            } => {
                self.string(media_type)?;
                self.data(data)?;
                self.optional(filename)?;
                self.metadata(provider_metadata)?;
            }
            Content::ToolCall {
                id,
                name,
                arguments,
                provider_executed: _,
                dynamic: _,
                provider_metadata,
            } => {
                self.string(id)?;
                self.string(name)?;
                self.string(arguments)?;
                self.metadata(provider_metadata)?;
            }
            Content::ToolResult {
                call_id,
                tool_name,
                output,
                dynamic: _,
                provider_metadata,
            } => {
                self.string(call_id)?;
                self.optional(tool_name)?;
                self.metadata(provider_metadata)?;
                match output {
                    ToolResultOutput::Text { value } | ToolResultOutput::ErrorText { value } => {
                        self.string(value)?
                    }
                    ToolResultOutput::Json { value } | ToolResultOutput::ErrorJson { value } => {
                        self.json(value)?
                    }
                    ToolResultOutput::ExecutionDenied { reason } => self.optional(reason)?,
                    ToolResultOutput::Content { value } => {
                        self.array(value)?;
                        for part in value {
                            match part {
                                ToolResultContentPart::Text { text } => self.string(text)?,
                                ToolResultContentPart::Media { media_type, data } => {
                                    self.string(media_type)?;
                                    self.data(data)?;
                                }
                                ToolResultContentPart::FileId { media_type, id } => {
                                    self.optional(media_type)?;
                                    self.string(id)?;
                                }
                            }
                        }
                    }
                }
            }
            Content::Source {
                source,
                provider_metadata,
            } => {
                match source {
                    Source::Url { id, url, title } => {
                        self.string(id)?;
                        self.string(url)?;
                        self.optional(title)?;
                    }
                    Source::Document {
                        id,
                        media_type,
                        title,
                        filename,
                    } => {
                        self.string(id)?;
                        self.string(media_type)?;
                        self.string(title)?;
                        self.optional(filename)?;
                    }
                }
                self.metadata(provider_metadata)?;
            }
            Content::ToolApprovalRequest {
                approval_id,
                tool_call_id,
                provider_metadata,
            } => {
                self.string(approval_id)?;
                self.string(tool_call_id)?;
                self.metadata(provider_metadata)?;
            }
            Content::ToolApprovalResponse {
                approval_id,
                approved: _,
                reason,
                provider_metadata,
            } => {
                self.string(approval_id)?;
                self.optional(reason)?;
                self.metadata(provider_metadata)?;
            }
        }
        Ok(())
    }
    pub(crate) fn prompt(&mut self, value: &mut Prompt) -> Result<()> {
        let Prompt {
            model,
            system,
            system_provider_metadata,
            messages,
            tools,
            params,
            response_format,
            tool_choice,
            stream: _,
        } = value;
        self.add(size_of::<Prompt>())?;
        self.string(model)?;
        self.optional(system)?;
        self.metadata(system_provider_metadata)?;
        self.array(messages)?;
        for Message { role: _, content } in messages {
            self.array(content)?;
            for item in content {
                self.content(item)?;
            }
        }
        self.array(tools)?;
        for tool in tools {
            match tool {
                Tool::Function {
                    name,
                    description,
                    parameters,
                    strict: _,
                    provider_metadata,
                } => {
                    self.string(name)?;
                    self.optional(description)?;
                    self.json(parameters)?;
                    self.metadata(provider_metadata)?;
                }
                Tool::ProviderDefined {
                    id,
                    name,
                    args,
                    provider_metadata,
                } => {
                    self.string(id)?;
                    self.string(name)?;
                    self.json(args)?;
                    self.metadata(provider_metadata)?;
                }
            }
        }
        let GenerationParams {
            temperature: _,
            top_p: _,
            max_tokens: _,
            chat_token_limit_field: _,
            reasoning_effort: _,
            reasoning_effort_source: _,
            response_modalities,
            top_k: _,
            seed: _,
            stop,
            presence_penalty: _,
            frequency_penalty: _,
            store: _,
            parallel_tool_calls: _,
            chat_stream_options: _,
            extra_protocol,
            supplemental_extra,
            extra,
        } = params;
        self.array(response_modalities)?;
        self.array(stop)?;
        for value in stop {
            self.string(value)?;
        }
        if let Some(ApiProtocol::Custom(name)) = extra_protocol {
            self.string(name)?;
        }
        for map in [supplemental_extra, extra] {
            // Normalize maps before any subsequent Prompt clone can copy spare buckets.
            let old = std::mem::take(map);
            for (key, mut value) in old {
                self.string(&key)?;
                self.json(&mut value)?;
                map.insert(key, value);
            }
            self.add(
                map.capacity()
                    .checked_mul(size_of::<(String, serde_json::Value)>() * 2 + 32)
                    .ok_or_else(super::saturated)?,
            )?;
        }
        if let Some(ResponseFormat::JsonSchema {
            name,
            description,
            strict: _,
            schema,
        }) = response_format
        {
            self.optional(name)?;
            self.optional(description)?;
            self.json(schema)?;
        }
        if let Some(ToolChoice::Tool { name }) = tool_choice {
            self.string(name)?;
        }
        Ok(())
    }
}

pub(super) fn errors(errors: &mut [BitrouterError]) -> Result<u64> {
    let mut weight = Weight::new();
    weight.add(size_of_val(errors))?;
    for error in errors {
        match error {
            BitrouterError::BadRequest { message }
            | BitrouterError::Upstream { message, .. }
            | BitrouterError::UpstreamPolicyViolation { message }
            | BitrouterError::Unauthorized(message)
            | BitrouterError::PaymentRequired(message)
            | BitrouterError::Forbidden(message)
            | BitrouterError::NotFound(message)
            | BitrouterError::Internal(message) => weight.string(message)?,
            BitrouterError::UpstreamPaymentRequired { detail }
            | BitrouterError::UpstreamRateLimited { detail, .. } => weight.optional(detail)?,
            BitrouterError::UpstreamAuth {
                www_authenticate,
                required_scope,
                ..
            } => {
                weight.optional(www_authenticate)?;
                weight.optional(required_scope)?;
            }
            BitrouterError::RateLimited { .. }
            | BitrouterError::UpstreamTimeout
            | BitrouterError::UpstreamUnavailable
            | BitrouterError::Cancelled => {}
            // These terminal/incompatibility roots cannot legitimately enter a
            // scheduled fallback queue. Do not estimate opaque retained evidence.
            BitrouterError::UpstreamInvalidResponse { .. }
            | BitrouterError::Incompatible { .. } => return Err(super::unavailable()),
            BitrouterError::UpstreamBadRequest { error } => {
                weight.json(error)?;
            }
        }
    }
    Ok(weight.total())
}

pub(crate) type Tracked = std::sync::Arc<dyn Fn() -> Result<u64> + Send + Sync>;

/// Only host-owned producers can attest an opaque event/extension's retained
/// bound. Public plugin insertion remains unaccounted and cannot enter the queue.
pub(crate) fn tracked<T: Send + Sync + 'static>(
    value: std::sync::Arc<T>,
    measure: impl Fn(&T) -> Result<u64> + Send + Sync + 'static,
) -> Tracked {
    std::sync::Arc::new(move || measure(&value))
}

/// Conservative logical bound for compact, freshly constructed host DTOs.
/// Producers must separately charge skipped fields and spare owned capacity.
mod compact;
pub(crate) fn compact_dto<T: serde::Serialize>(value: &T) -> Result<u64> {
    compact::measure(value)
}

pub(crate) fn headers(headers: &mut http::HeaderMap) -> Result<u64> {
    let mut weight = Weight::new();
    let mut compact = http::HeaderMap::new();
    for (name, value) in headers.iter() {
        let name = http::HeaderName::from_bytes(name.as_str().as_bytes())
            .map_err(|_| super::unavailable())?;
        let mut owned =
            http::HeaderValue::from_bytes(value.as_bytes()).map_err(|_| super::unavailable())?;
        owned.set_sensitive(value.is_sensitive());
        weight.add(name.as_str().len() + owned.as_bytes().len() + 96)?;
        compact.append(name, owned);
    }
    *headers = compact;
    weight.add(headers.capacity() * 64)?;
    Ok(weight.total())
}

pub(crate) fn targets(chain: &Vec<crate::gateway::pipeline::types::RoutingTarget>) -> Result<u64> {
    use crate::gateway::pipeline::types::RoutingTarget;
    let mut weight = Weight::new();
    weight.array(chain)?;
    for RoutingTarget {
        registry: _,
        provider_name,
        service_id,
        api_base,
        api_key,
        api_protocol,
        chat_token_limit_field: _,
        chat_supports_store: _,
        chat_supports_stream_options: _,
        chat_google_extensions: _,
        reasoning_effort,
        model_constraints,
        account_label,
        api_key_override,
        api_base_override,
        auth_scheme: _,
        headers,
    } in chain
    {
        for value in [provider_name, service_id, api_base, api_key] {
            weight.string(value)?;
        }
        if let ApiProtocol::Custom(name) = api_protocol {
            weight.string(name)?;
        }
        for value in [account_label, api_key_override, api_base_override] {
            weight.optional(value)?;
        }
        if let Some(ReasoningEffortConfig { levels, default: _ }) = reasoning_effort {
            weight.array(levels)?;
        }
        let bitrouter_ai::core::evidence::NativeRouteConstraints {
            capabilities,
            token_limits: _,
            input_token_counting: _,
            source,
        } = model_constraints;
        weight.array(capabilities)?;
        weight.optional(source)?;
        weight.array(headers)?;
        for header in headers {
            weight.add(header.name().as_str().len() + 40)?;
            if let Some(value) = header.default() {
                weight.add(value.as_bytes().len() + 40)?;
            }
        }
    }
    Ok(weight.total())
}
