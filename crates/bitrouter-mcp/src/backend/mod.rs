//! `Backend` — *which deployment* an origin server is serving, plus the wire
//! types both backends share. Implementations are thin reqwest clients — no
//! routing logic lives here.
//!
//! It answers no action itself. `status` and `list_models` are
//! [actions](crate::actions), answered through
//! [`crate::actions::status::StatusQuery`] and
//! [`crate::actions::models::ModelsQuery`] by whichever side
//! actually knows. A backend that can answer one hands its port over via
//! [`Backend::status_port`] / [`Backend::models_port`].
//!
//! Running a completion is not among them: MCP is a control and introspection
//! surface, and inference is what the daemon's HTTP API (`/v1/messages`,
//! `/v1/chat/completions`) is for.

use std::sync::Arc;

use async_trait::async_trait;

use crate::actions::models::ModelsQuery;
use crate::actions::status::StatusQuery;

pub mod cloud;
pub mod local;

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

/// Which deployment an origin server is serving. Object-safe so the HTTP
/// profile can be assembled from an `Arc<dyn Backend>` alone.
///
/// Every method is a port handover: the trait does nothing itself, it only says
/// which of this crate's actions the backend can answer *for its own
/// deployment*. It exists in this shape because
/// [`serve_http_on`](crate::server::serve_http_on) takes an `Arc<dyn Backend>`
/// and nothing else, so a backend that cannot hand a port over cannot keep its
/// tool.
#[async_trait]
pub trait Backend: Send + Sync {
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
