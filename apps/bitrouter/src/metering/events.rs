//! Events emitted by the metering recorder.

use bitrouter_sdk::PipelineEvent;
use serde::Serialize;

/// Emitted when pricing is missing for the resolved `(provider, model)`.
/// Downstream observers can surface this so operators notice unbilled
/// requests; the metering row is still written with `estimated_charge = 0`.
#[derive(Debug, Clone, Serialize)]
pub struct PricingUnavailable {
    /// The provider id the executor used.
    pub provider_id: String,
    /// The resolved service / model id.
    pub model_id: String,
}

impl PipelineEvent for PricingUnavailable {
    fn event_name(&self) -> &'static str {
        "metering.pricing_unavailable"
    }
}
