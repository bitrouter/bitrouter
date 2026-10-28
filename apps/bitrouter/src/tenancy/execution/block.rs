//! Original capture custody supplies the only HTTP Block producer capability.
pub(in crate::tenancy) mod store;

use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_ai::{routing::ModelInfo, tenancy::operation::ErrorCode};

use super::TenantExecution;
use crate::{
    gateway::{
        caller::CallerContext,
        error::{BitrouterError, Result as GatewayResult},
        pipeline::{
            context::PipelineContext,
            routing::{ModelResolution, PromptOverrides, RoutingPrefs, RoutingTable},
            types::RoutingTarget,
        },
    },
    storage::execution::DecisionContext,
    tenancy::{
        capture::{
            Binding,
            queue::{Reply, Request},
        },
        transaction::Disposition,
    },
};

pub(in crate::tenancy) struct SelectionRequest {
    pub execution: Arc<TenantExecution>,
    pub context: DecisionContext,
    pub token_pricing_eligible: bool,
}

pub(in crate::tenancy) struct Witness {
    binding: Arc<Binding>,
    selection: store::Selection,
}

impl TenantExecution {
    pub(in crate::tenancy) async fn select_block(
        self: &Arc<Self>,
        ctx: &PipelineContext,
    ) -> GatewayResult<()> {
        let Some(profile) = self.route.block_profile() else {
            return Ok(());
        };
        let binding = self
            .submission
            .block
            .as_ref()
            .ok_or_else(|| self.reject(ErrorCode::Forbidden))?;
        // The first retained profile has no override/continuation precedence.
        // Refuse before H rather than mint an intent for a different request.
        let prompt = ctx.require_generation_prompt()?;
        if ["previous_response_id", "conversation"].iter().any(|key| {
            prompt
                .params
                .extra
                .get(*key)
                .is_some_and(|value| !value.is_null())
        }) {
            return Err(self.reject(ErrorCode::CapabilityMismatch));
        }
        let rule = profile
            .definition
            .rules
            .first()
            .ok_or_else(|| self.reject(ErrorCode::AuthorityUnavailable))?;
        let request = Arc::new(SelectionRequest {
            execution: self.clone(),
            token_pricing_eligible: self.route.block_token_pricing_input(prompt),
            context: DecisionContext {
                selector: rule.selector.clone(),
                fingerprint: crate::policy_table_router::PolicyTable::fingerprint(prompt),
            },
        });
        self.revalidate().map_err(|code| self.reject(code))?;
        let deadline = self
            .submission
            .deadline
            .min(tokio::time::Instant::now() + std::time::Duration::from_secs(5));
        // This awaited work is already inside the original registered pipeline.
        // H retains admitted work independently of loss of this reply waiter.
        let receive = binding
            .enqueue_before(Request::Block(request), deadline)
            .await
            .map_err(|failure| self.reject(failure.0.code))?;
        let outcome = tokio::time::timeout_at(deadline, receive)
            .await
            .map_err(|_| self.reject(ErrorCode::AuthorityUnavailable))?
            .map_err(|_| self.reject(ErrorCode::AuthorityUnavailable))?;
        let selection = match (outcome.result, outcome.disposition) {
            (Ok(Reply::Block(selection)), Disposition::Committed) => *selection,
            (Err(failure), _) => return Err(self.reject(failure.0.code)),
            _ => return Err(self.reject(ErrorCode::AuthorityUnavailable)),
        };
        if binding.current_head() != selection.capture_head {
            return Err(self.reject(ErrorCode::Forbidden));
        }
        self.revalidate().map_err(|code| self.reject(code))?;
        self.block
            .set(Arc::new(Witness {
                binding: binding.clone(),
                selection,
            }))
            .map_err(|_| self.reject(ErrorCode::Forbidden))?;
        #[cfg(all(test, unix))]
        if let Some(gate) = self
            .service
            .take_host_gate(crate::tenancy::hosts::HostBoundary::BlockIntentAck)
            .map_err(|_| self.reject(ErrorCode::AuthorityUnavailable))?
        {
            gate.reached().await;
        }
        self.revalidate().map_err(|code| self.reject(code))?;
        Ok(())
    }

    pub(super) fn block_send(&self, target: &RoutingTarget) -> Result<(), ErrorCode> {
        if self.route.block_profile().is_none() {
            return Ok(());
        }
        let witness = self.block.get().ok_or(ErrorCode::Forbidden)?;
        if !self
            .submission
            .block
            .as_ref()
            .is_some_and(|binding| Arc::ptr_eq(binding, &witness.binding))
            || witness.binding.current_head() != witness.selection.capture_head
            || witness.binding.block_generation() != witness.selection.intent.control_generation
        {
            return Err(ErrorCode::Forbidden);
        }
        self.route
            .block_candidate(&witness.selection.intent.selected_route, target)
    }

    pub(super) fn block_table(self: &Arc<Self>) -> Arc<dyn RoutingTable> {
        Arc::new(Table(self.clone()))
    }
}

struct Table(Arc<TenantExecution>);
impl Table {
    fn selector(&self) -> GatewayResult<&str> {
        let witness = self
            .0
            .block
            .get()
            .ok_or_else(|| self.0.reject(ErrorCode::Forbidden))?;
        self.0.revalidate().map_err(|code| self.0.reject(code))?;
        if witness.binding.current_head() != witness.selection.capture_head
            || witness.binding.block_generation() != witness.selection.intent.control_generation
        {
            return Err(self.0.reject(ErrorCode::Forbidden));
        }
        Ok(&witness.selection.intent.selected_route)
    }
}

#[async_trait]
impl RoutingTable for Table {
    async fn resolve_model(&self, model: &str) -> GatewayResult<ModelResolution> {
        let mut resolution = self.0.route.resolve_model(model).await?;
        resolution.clean_model = self.selector()?.to_owned();
        Ok(resolution)
    }
    async fn route_chain(
        &self,
        _model: &str,
        prefs: &RoutingPrefs,
        _caller: &CallerContext,
    ) -> GatewayResult<Vec<RoutingTarget>> {
        self.0.route.block_chain(self.selector()?, prefs)
    }
    async fn route_resolved(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        caller: &CallerContext,
    ) -> GatewayResult<Vec<RoutingTarget>> {
        self.route_chain(model, prefs, caller).await
    }
    fn canonical_model_id(&self, model: &str, target: &RoutingTarget) -> Option<String> {
        self.0.block_send(target).ok()?;
        self.0.route.canonical_model_id(model, target)
    }
    fn list_models(&self) -> Vec<ModelInfo> {
        self.0.route.list_models()
    }
    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        self.0.route.model_info(model)
    }
    async fn reload(&self) -> GatewayResult<()> {
        Err(BitrouterError::bad_request(
            "retained Block routing cannot reload",
        ))
    }
    async fn preset_overrides(&self, model: &str) -> GatewayResult<PromptOverrides> {
        self.0.route.preset_overrides(model).await
    }
}
