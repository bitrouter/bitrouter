//! Bounded local pilot requests. Caller input never supplies token counts or URLs.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::config::ConnectorConfig;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRequest {
    pub request_id: String,
    pub pool_id: String,
    pub pool_generation: String,
    /// Relative total duration, starting when the local service receives this call.
    pub deadline_ms: u64,
    pub request: ChatRequest,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl LocalRequest {
    pub fn validate(&self, config: &ConnectorConfig, generation: &str) -> Result<(), &'static str> {
        let execution = config
            .execution
            .as_ref()
            .ok_or("execution_not_configured")?;
        if self.request_id.is_empty()
            || matches!(self.request_id.as_str(), "." | "..")
            || self.request_id.len() > 128
            || !self
                .request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err("invalid_request_id");
        }
        if self.pool_id != config.pool.id || self.pool_generation != generation {
            return Err("pool_binding_mismatch");
        }
        if self.request.model != config.pool.model {
            return Err("model_binding_mismatch");
        }
        if self.deadline_ms < 10 || self.deadline_ms > execution.max_deadline_ms {
            return Err("invalid_deadline");
        }
        if self.request.max_tokens == 0 || self.request.max_tokens > execution.max_output_tokens {
            return Err("output_token_limit");
        }
        if self.request.messages.is_empty()
            || self.request.messages.len() > 128
            || !matches!(
                self.request.messages.last().map(|message| &message.role),
                Some(Role::User)
            )
            || self
                .request
                .messages
                .iter()
                .any(|message| message.content.is_empty())
        {
            return Err("unsupported_chat_shape");
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String, &'static str> {
        let bytes = serde_json::to_vec(self).map_err(|_| "request_encoding")?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    pub fn tokenize_body(&self) -> serde_json::Value {
        serde_json::json!({
            "model": self.request.model,
            "messages": self.request.messages,
            "add_generation_prompt": true,
            "continue_final_message": false,
            "add_special_tokens": false,
        })
    }

    pub fn inference_body(&self) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.request.model,
            "messages": self.request.messages,
            "max_tokens": self.request.max_tokens,
            "stream": self.request.stream,
            "n": 1,
            "add_generation_prompt": true,
            "continue_final_message": false,
            "add_special_tokens": false,
        });
        if self.request.stream {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }
        body
    }
}
