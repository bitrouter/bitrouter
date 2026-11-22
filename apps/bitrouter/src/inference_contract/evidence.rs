//! Provider/model facts shared by application components.
//! Verification and provider adaptation remain in the service.

use bitrouter_ai::types::Capability;
use serde::{Deserialize, Serialize};

use crate::config::model::{InputTokenCounting, ModelTokenLimits};

/// Credential-free facts about one concrete provider/model candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeRouteConstraints {
    /// Positive capability observations, not an exhaustive denylist. Omitted
    /// capabilities remain unknown even when this list is nonempty.
    pub capabilities: Vec<Capability>,
    /// Known limits for this exact route.
    pub token_limits: ModelTokenLimits,
    /// Opt-in counting contract for this exact provider/model.
    #[serde(default)]
    pub input_token_counting: Option<InputTokenCounting>,
    /// No source means that the routing table supplied no authoritative facts.
    pub source: Option<String>,
}
