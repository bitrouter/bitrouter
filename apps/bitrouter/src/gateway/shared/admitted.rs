//! The locked admission snapshot travels with the request, not in caller headers.

use std::sync::Arc;

use crate::auth::events::{ApiPrincipalEstablished, Authenticated};
use crate::gateway::caller::CallerContext;
use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::pipeline::{context::PipelineContext, types::PipelineRequest};

use super::engine::{Engine, Rejection};
use super::ownership::Custody;

/// Created only after shared admission commits. The original policy and tenant
/// cannot be replaced by a later API-key lookup while preparing the pipeline.
pub(crate) struct SharedAdmission {
    pub(super) engine: Engine,
    pub(super) key_hash: String,
    pub(super) execution_id: String,
    operation: bitrouter_ai::types::ModelOperation,
    principal: Authenticated,
    pub(super) dispatch: Arc<super::dispatched::Tracker>,
    custody: Arc<Custody>,
}

impl SharedAdmission {
    #[cfg(test)]
    pub(super) fn new(
        engine: Engine,
        key_hash: String,
        execution: bitrouter_ai::gateway::execution::GatewayExecution,
        principal: Authenticated,
    ) -> anyhow::Result<Arc<Self>> {
        let custody = Custody::fixture(engine, execution.execution_id.clone())?;
        Ok(Self::with_custody(custody, key_hash, execution, principal))
    }

    pub(super) fn with_custody(
        custody: Arc<Custody>,
        key_hash: String,
        execution: bitrouter_ai::gateway::execution::GatewayExecution,
        principal: Authenticated,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine: custody.engine.clone(),
            key_hash,
            execution_id: execution.execution_id,
            operation: execution.operation,
            principal,
            dispatch: custody.dispatch.clone(),
            custody,
        })
    }

    pub(crate) fn record_pipeline_outcome(&self, successful_terminal: bool) {
        self.custody.record_pipeline_outcome(successful_terminal);
    }

    /// Required continuation activation precedes this authoritative commit;
    /// HTTP success may only become visible after it has acknowledged.
    pub(crate) async fn finish_delivery(&self, successful_terminal: bool) -> Result<()> {
        use bitrouter_ai::gateway::execution::TerminalOutcome;

        self.record_pipeline_outcome(successful_terminal);
        if self.blocks_fallback() {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        let execution = self
            .engine
            .owned_execution(&self.execution_id)
            .await
            .map_err(public_error)?;
        let outcome = self.custody.terminal_outcome(&execution);
        if successful_terminal && !matches!(outcome, TerminalOutcome::Succeeded) {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        self.engine
            .finish(&self.execution_id, &outcome)
            .await
            .map_err(public_error)?;
        Ok(())
    }

    pub fn attach(self: &Arc<Self>, request: &mut PipelineRequest) {
        request.request_id.clone_from(&self.execution_id);
        request.caller = CallerContext::new(&self.principal.api_key_id, &self.principal.user_id);
        request.extensions.insert(self.clone());
    }

    pub async fn establish(&self, ctx: &mut PipelineContext) -> Result<()> {
        self.validate(ctx).await?;
        ctx.set_metadata(
            &crate::auth::hook::plugin_id(),
            serde_json::json!({
                "api_key_id": self.principal.api_key_id,
                "user_id": self.principal.user_id,
                "policy_id": self.principal.policy_id,
            }),
        );
        ctx.emit(self.principal.clone());
        ctx.emit(ApiPrincipalEstablished {
            route_scope_id: self.key_hash.clone(),
        });
        Ok(())
    }

    pub async fn revalidate(&self, ctx: &PipelineContext) -> Result<()> {
        let unchanged = ctx.get_event::<Authenticated>().is_some_and(|frozen| {
            frozen.api_key_id == self.principal.api_key_id
                && frozen.user_id == self.principal.user_id
                && frozen.policy_id == self.principal.policy_id
        }) && ctx
            .get_event::<ApiPrincipalEstablished>()
            .is_some_and(|frozen| frozen.route_scope_id == self.key_hash);
        if !unchanged {
            return Err(BitrouterError::Unauthorized(
                "shared authentication snapshot changed".into(),
            ));
        }
        self.validate(ctx).await
    }

    async fn validate(&self, ctx: &PipelineContext) -> Result<()> {
        let key_matches = crate::auth::hook::credential_from_headers(ctx.headers())
            .is_some_and(|key| crate::auth::keys::hash_key(&key) == self.key_hash);
        if !key_matches
            || ctx.request_id() != self.execution_id
            || ctx.operation() != self.operation
            || ctx.caller().is_local()
            || ctx.caller().api_key_id() != self.principal.api_key_id
            || ctx.caller().user_id() != self.principal.user_id
        {
            return Err(BitrouterError::Unauthorized(
                "shared request differs from its admission".into(),
            ));
        }
        self.engine
            .revalidate(&self.key_hash, &self.execution_id)
            .await
            .map_err(public_error)
    }
}

/// Never forward SQL, credential or connection diagnostics into HTTP/SSE.
pub(super) fn public_error(error: anyhow::Error) -> BitrouterError {
    match error.downcast_ref::<Rejection>() {
        Some(Rejection::Unauthorized) => {
            BitrouterError::Unauthorized("shared authentication failed".into())
        }
        Some(Rejection::Forbidden) => {
            BitrouterError::Forbidden("shared resource scope denied".into())
        }
        Some(Rejection::Quota) => BitrouterError::RateLimited {
            retry_after: Some(1),
        },
        Some(Rejection::NotFound) => BitrouterError::NotFound("shared execution not found".into()),
        _ => BitrouterError::UpstreamUnavailable,
    }
}
