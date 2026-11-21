//! Authenticated, bounded capture of model request facts before prompt mutation.

use std::{io, sync::Arc, time::Duration};

use async_trait::async_trait;
use bitrouter_ai::types::ApiProtocol;
use serde::Serialize;

use crate::gateway::{
    error::{BitrouterError, Result},
    observability::event::PipelineEvent,
    pipeline::{
        context::PipelineContext,
        hooks::{HookDecision, PreRequestHook},
    },
};

use super::correlation::TrajectoryRuntime;

const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;

/// In-process binding between authenticated ingress and its durable request.
#[derive(Clone, Serialize)]
pub(crate) struct CapturedRequest {
    #[serde(skip)]
    external_id: String,
    #[serde(skip)]
    owner: String,
    #[serde(skip)]
    api_key_id: String,
    #[serde(skip)]
    delivery_attempt_id: u64,
    #[serde(skip)]
    protocol: ApiProtocol,
    pub(super) request_id: String,
    pub(super) episode_id: String,
}

impl PipelineEvent for CapturedRequest {
    fn event_name(&self) -> &'static str {
        "trajectory.request_captured"
    }
}

impl CapturedRequest {
    pub(super) fn matches_settlement(
        &self,
        context: &crate::gateway::pipeline::settlement::SettlementContext,
    ) -> bool {
        self.external_id == context.request_id
            && self.owner == context.caller.user_id()
            && self.api_key_id == context.caller.api_key_id()
    }

    fn matches_context(&self, context: &PipelineContext) -> bool {
        self.external_id == context.request_id()
            && self.owner == context.caller().user_id()
            && self.api_key_id == context.caller().api_key_id()
            && self.delivery_attempt_id == context.delivery_attempt_id()
            && context.inbound_protocol().as_ref() == Some(&self.protocol)
    }
}

pub(crate) struct CaptureIngress {
    runtime: TrajectoryRuntime,
}

impl CaptureIngress {
    pub(crate) fn new(runtime: TrajectoryRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl PreRequestHook for CaptureIngress {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        if ctx.extension::<CapturedRequest>().is_some() {
            return self.revalidate_context(ctx).await;
        }
        // Native callers without a declared wire protocol have no protocol fact
        // to record. HTTP always supplies the parsed inbound protocol.
        let Some(protocol) = ctx.inbound_protocol() else {
            return Ok(HookDecision::Allow);
        };
        let prompt = ctx.require_generation_prompt()?;
        serde_json::to_writer(&mut CaptureBudget(MAX_CAPTURE_BYTES), prompt).map_err(|_| {
            BitrouterError::bad_request("trajectory input exceeds capture capacity")
        })?;
        // Settlement duration covers the whole pipeline, including authentication.
        // Anchor its start to that observed elapsed time, not to this later hook.
        let elapsed = chrono::TimeDelta::from_std(Duration::from_millis(ctx.request_duration_ms()))
            .map_err(|_| {
                BitrouterError::internal("trajectory request duration exceeds time range")
            })?;
        let captured_at = chrono::Utc::now()
            .checked_sub_signed(elapsed)
            .ok_or_else(|| BitrouterError::internal("trajectory request start exceeds time range"))?
            .to_rfc3339();
        // The owning pipeline task retains this commit until its outcome is
        // known; cancellation must not strand a committed request without the
        // binding needed by settlement.
        let captured = self
            .runtime
            .begin_request(
                ctx.caller().user_id(),
                ctx.request_id(),
                protocol.clone(),
                prompt,
                &captured_at,
            )
            .await
            .map_err(|_| BitrouterError::internal("trajectory capture failed"))?;
        let binding = CapturedRequest {
            external_id: ctx.request_id().into(),
            owner: ctx.caller().user_id().into(),
            api_key_id: ctx.caller().api_key_id().into(),
            delivery_attempt_id: ctx.delivery_attempt_id(),
            protocol,
            request_id: captured.request_id,
            episode_id: captured.episode_id,
        };
        ctx.insert_extension(Arc::new(binding.clone()));
        ctx.emit(binding);
        Ok(HookDecision::Allow)
    }

    async fn revalidate_context(&self, ctx: &PipelineContext) -> Result<HookDecision> {
        let valid = ctx
            .extension::<CapturedRequest>()
            .is_some_and(|binding| binding.matches_context(ctx));
        if !valid {
            return Err(BitrouterError::bad_request(
                "trajectory capture lost its request binding",
            ));
        }
        Ok(HookDecision::Allow)
    }
}

struct CaptureBudget(usize);

impl io::Write for CaptureBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("capture capacity exceeded"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lifecycle_tests;
