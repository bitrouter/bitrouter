//! The `Backend` abstraction over *where* tool calls route, plus the wire
//! types the tools and both backends share. Implementations are thin reqwest
//! clients — no routing logic lives here.

use async_trait::async_trait;

pub mod cloud;
pub mod local;

/// A normalized completion request, independent of the upstream wire shape.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CompleteRequest {
    /// Routable model name (e.g. `openai/gpt-4o`), from `list_models`.
    pub model: String,
    /// Chat messages, passed through to the OpenAI-shaped upstream verbatim.
    pub messages: Vec<serde_json::Value>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub system: Option<String>,
}

/// Token accounting for a completion.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// A full (non-streaming) completion result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CompleteResponse {
    pub content: String,
    pub model: String,
    pub usage: Usage,
    pub finish_reason: String,
}

/// One routable model.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
}

/// Backend-specific status payload.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum StatusInfo {
    Local {
        listen: String,
        models: usize,
        providers: Vec<ProviderStatus>,
    },
    Cloud {
        available_micro_usd: i64,
        balance_micro_usd: i64,
        pending_micro_usd: i64,
    },
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ProviderStatus {
    pub id: String,
}

/// Envelope returned by `/v1/models` on both backends.
#[derive(serde::Deserialize)]
pub(super) struct ModelsEnvelope {
    pub(super) data: Vec<ModelEntry>,
}

/// One entry in the models list envelope.
#[derive(serde::Deserialize)]
pub(super) struct ModelEntry {
    pub(super) id: String,
    #[serde(default)]
    pub(super) providers: Vec<String>,
}

/// The caller's bearer to forward upstream, if the inbound request carried one.
/// Empty for stdio (the cloud backend's configured credential applies instead).
#[derive(Debug, Default, Clone)]
pub struct CallerAuth {
    pub bearer: Option<String>,
}

/// Errors surfaced to the MCP client as tool failures.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("daemon not reachable at {0} — run `bitrouter start`")]
    DaemonUnreachable(String),
    #[error("upstream returned {status}: {body}")]
    Upstream { status: u16, body: String },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("malformed upstream response: {0}")]
    Decode(String),
    #[error("no bearer token: set Authorization on the MCP client")]
    MissingCredential,
}

/// Where tool calls route. Object-safe so tools hold `Arc<dyn Backend>`.
#[async_trait]
pub trait Backend: Send + Sync {
    async fn complete(
        &self,
        caller: &CallerAuth,
        req: CompleteRequest,
    ) -> Result<CompleteResponse, BackendError>;
    async fn list_models(&self, caller: &CallerAuth) -> Result<Vec<ModelInfo>, BackendError>;
    async fn status(&self, caller: &CallerAuth) -> Result<StatusInfo, BackendError>;
}
