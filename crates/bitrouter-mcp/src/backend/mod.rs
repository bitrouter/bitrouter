//! The `Backend` abstraction over *where* completions route, plus the wire
//! types the tools and both backends share. Implementations are thin reqwest
//! clients — no routing logic lives here.
//!
//! `status` and `list_models` are **not** here: they are
//! [actions](crate::actions), answered through
//! [`StatusQuery`](crate::actions::status::StatusQuery) and
//! [`ModelsQuery`](crate::actions::models::ModelsQuery) by whichever side
//! actually knows. A backend that can answer one hands its port over via
//! [`Backend::status_port`] / [`Backend::models_port`].

use std::sync::Arc;

use async_trait::async_trait;

use crate::actions::models::ModelsQuery;
use crate::actions::status::StatusQuery;

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

/// Envelope returned by `/v1/models` on both backends.
#[derive(serde::Deserialize)]
pub(super) struct ModelsEnvelope {
    pub(super) data: Vec<ModelEntry>,
}

impl ModelsEnvelope {
    /// The envelope as the shared action's element type, keeping **every**
    /// provider per model — the fallback chain the wire has always carried and
    /// this crate used to throw away.
    pub(super) fn into_models(self) -> Vec<bitrouter_sdk::language_model::routing::ModelInfo> {
        self.data
            .into_iter()
            .map(|m| bitrouter_sdk::language_model::routing::ModelInfo {
                id: m.id,
                providers: m.providers,
            })
            .collect()
    }
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

/// Where completions route. Object-safe so tools hold `Arc<dyn Backend>`.
#[async_trait]
pub trait Backend: Send + Sync {
    async fn complete(
        &self,
        caller: &CallerAuth,
        req: CompleteRequest,
    ) -> Result<CompleteResponse, BackendError>;

    /// The `status` port this backend can answer with, or `None`.
    ///
    /// This is wiring, not logic: it exists so the HTTP profile — which is
    /// assembled from an `Arc<dyn Backend>` and nothing else — can still serve
    /// `status` for the deployment whose status the backend genuinely knows
    /// (the cloud account's remaining credit). A local daemon's liveness is a
    /// control-socket question and its spend a metering-database one, so
    /// `LocalBackend` returns `None` and the embedding binary injects the real
    /// port instead.
    fn status_port(self: Arc<Self>) -> Option<Arc<dyn StatusQuery>>;

    /// The `list_models` port this backend can answer with, or `None`.
    ///
    /// Wiring on the same terms as [`Self::status_port`], and for the same
    /// reason: the HTTP profile is assembled from an `Arc<dyn Backend>` and
    /// nothing else, so a backend that can list a catalog has to hand its port
    /// over rather than have one injected. Both backends can: `GET /v1/models`
    /// is exactly this question, and the response has always carried every
    /// provider per model.
    ///
    /// The embedding binary injects a better port where it has one — on
    /// stdio + local it reads the daemon's live routing table over the control
    /// socket and falls back to static config, so `list_models` answers with no
    /// daemon running at all.
    fn models_port(self: Arc<Self>) -> Option<Arc<dyn ModelsQuery>>;
}
