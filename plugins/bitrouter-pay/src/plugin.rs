//! [`PayPlugin`] — registers the Arc payment gate on the `language_model` pipeline.

use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_sdk::error::Result;
use bitrouter_sdk::language_model::{HookDecision, PipelineContext, PreRequestHook};
use bitrouter_sdk::{AppBuilder, Plugin, PluginId};

use crate::PayError;
use crate::gate::{ArcPaymentGate, ArcPaymentGateConfig};

/// Extension key for retrieving the active payment gate from a request context.
pub struct PaymentGateExtension(pub Arc<ArcPaymentGate>);

/// A [`Plugin`] that deposits [`ArcPaymentGate`] into every request's extensions.
pub struct PayPlugin {
    id: PluginId,
    gate: Arc<ArcPaymentGate>,
}

impl PayPlugin {
    /// Build a payment plugin from gate configuration.
    pub fn new(config: ArcPaymentGateConfig) -> std::result::Result<Self, PayError> {
        let gate = Arc::new(ArcPaymentGate::new(config)?);
        Ok(Self {
            id: PluginId::new("bitrouter-pay"),
            gate,
        })
    }

    /// Access the underlying gate (for host wiring outside the pipeline).
    pub fn gate(&self) -> Arc<ArcPaymentGate> {
        self.gate.clone()
    }
}

impl Plugin for PayPlugin {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn install(&self, app: &mut AppBuilder) {
        app.language_model_builder()
            .pre_request_hook(DepositPaymentGateHook::new(self.gate.clone()));
    }
}

/// Deposits the shared payment gate into request extensions for downstream hooks.
pub struct DepositPaymentGateHook {
    gate: Arc<ArcPaymentGate>,
}

impl DepositPaymentGateHook {
    pub fn new(gate: Arc<ArcPaymentGate>) -> Self {
        Self { gate }
    }
}

#[async_trait]
impl PreRequestHook for DepositPaymentGateHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        ctx.insert_extension(self.gate.clone());
        Ok(HookDecision::Allow)
    }
}
