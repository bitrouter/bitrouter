//! Direct original-clock/actual-arm wiring, independent of ObserveHooks/OTel.
use super::{context::PipelineContext, types::RoutingTarget};
use crate::tenancy::receipts::provider_technical::Operation;
use bitrouter_ai::tenancy::identity::AttemptId;
use std::{sync::Arc, time::Instant};

#[derive(Default)]
pub(super) struct CurrentState {
    operation: Option<Arc<Operation>>,
    hop: Option<Arc<crate::tenancy::receipts::provider_technical::scope::Hop>>,
}
pub(super) type Current = Arc<std::sync::Mutex<CurrentState>>;
fn capture(
    ctx: &PipelineContext,
) -> Option<Arc<crate::tenancy::receipts::provider_technical::Capture>> {
    ctx.extension::<crate::tenancy::execution::TenantExecution>()
        .and_then(|execution| execution.provider_technical_capture())
}
pub(super) fn register_context(ctx: &PipelineContext) {
    if let Some(capture) = capture(ctx) {
        capture.register_context(ctx.request_id());
    }
}

pub(super) struct HopGuard {
    hop: Option<Arc<crate::tenancy::receipts::provider_technical::scope::Hop>>,
    started: Instant,
    profile: bitrouter_ai::tenancy::provider_technical_facts_v2::HopProfile,
    finished: bool,
}
impl HopGuard {
    pub(super) fn finish(&mut self, success: bool) {
        use bitrouter_ai::tenancy::provider_technical_facts_v2::HopEnd;
        if let Some(hop) = &self.hop {
            hop.finish(
                self.profile,
                if success {
                    HopEnd::Completed
                } else {
                    HopEnd::Failed
                },
                super::timing::elapsed_millis(self.started),
                false,
            );
        }
        self.finished = true;
    }
    pub(super) fn opened(
        mut self,
    ) -> Option<Arc<crate::tenancy::receipts::provider_technical::scope::Hop>> {
        self.finished = true;
        if let Some(hop) = &self.hop {
            hop.selected(true);
        }
        self.hop.take()
    }
}
impl Drop for HopGuard {
    fn drop(&mut self) {
        if !self.finished
            && let Some(hop) = &self.hop
        {
            hop.finish(
                self.profile,
                bitrouter_ai::tenancy::provider_technical_facts_v2::HopEnd::Dropped,
                super::timing::elapsed_millis(self.started),
                true,
            );
        }
    }
}
pub(super) fn begin_hop(ctx: &PipelineContext, streamed: bool) -> HopGuard {
    use bitrouter_ai::tenancy::provider_technical_facts_v2::HopProfile;
    let profile = if streamed {
        HopProfile::CoreStreamOpenCallbacksV1
    } else {
        HopProfile::CoreJsonHopCallbacksV1
    };
    let owner = capture(ctx);
    let hop = owner
        .as_ref()
        .and_then(|capture| capture.begin_hop(profile));
    match ctx.provider_technical.try_lock() {
        Ok(mut current) => {
            current.hop = hop.clone();
            current.operation = None;
        }
        Err(_) => {
            if let Some(owner) = owner {
                owner.invalidate();
            }
        }
    }
    HopGuard {
        hop,
        started: Instant::now(),
        profile,
        finished: false,
    }
}

pub(super) fn finalize_scope(
    ctx: &PipelineContext,
    streamed: bool,
    partial: bool,
    upstream: Option<u64>,
    opened: bool,
) {
    use bitrouter_ai::tenancy::{
        provider_technical_facts::{FirstSemantic, FirstSemanticKind},
        provider_technical_facts_v2::{
            PipelineProfile, PipelineScope, PipelineStream, StreamProfile,
        },
    };
    if let Some(capture) = capture(ctx) {
        if !capture.healthy() {
            return;
        }
        let stream = if streamed && opened {
            Some(PipelineStream {
                profile: StreamProfile::CorePipelineMergedStreamV1,
                source_attempt_id: None,
                terminal_attempt_id: None,
                upstream_duration_ms: upstream,
                first_semantic: ctx.first_token_timing().map(|first| FirstSemantic {
                    ttft_ms: first.ttft_ms,
                    kind: match first.kind {
                        super::timing::FirstTokenKind::Reasoning => FirstSemanticKind::Reasoning,
                        super::timing::FirstTokenKind::Text => FirstSemanticKind::Text,
                        super::timing::FirstTokenKind::Tool => FirstSemanticKind::Tool,
                    },
                }),
                generation_duration_ms: ctx.generation_duration_ms(),
                partial: partial || upstream.is_none(),
            })
        } else {
            None
        };
        capture.finalize_scope(PipelineScope {
            profile: PipelineProfile::CorePipelineFinalizedV1,
            context_request_id: ctx.request_id().to_owned(),
            request_duration_ms: ctx.request_duration_ms(),
            streamed,
            stream,
        });
    }
}

pub(super) struct Guard {
    operation: Option<Arc<Operation>>,
    detached: bool,
}
impl Guard {
    pub(super) fn completed(&self) {
        if let Some(operation) = &self.operation {
            operation.finish(false);
        }
    }
    pub(super) fn detach(mut self) {
        self.detached = true;
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if !self.detached
            && let Some(operation) = &self.operation
        {
            operation.finish(true);
        }
    }
}

pub(super) fn begin(
    ctx: &PipelineContext,
    target: &RoutingTarget,
    started: Instant,
    streamed: bool,
) -> Guard {
    // This is the Core-owned sender clock. The paired bridge is not Google;
    // its separate Cloud-owned timing/projection must never consume this value.
    let bridge = matches!(&target.api_protocol,bitrouter_ai::types::ApiProtocol::Custom(name)
        if name==bitrouter_ai::tenancy::generate_content::BRIDGE_PROTOCOL);
    let capture = ctx
        .extension::<crate::tenancy::execution::TenantExecution>()
        .and_then(|execution| execution.provider_technical_capture());
    let operation = if bridge {
        None
    } else {
        capture
            .as_ref()
            .map(|capture| capture.begin(started, streamed))
    };
    match ctx.provider_technical.try_lock() {
        Ok(mut current) => current.operation = operation.clone(),
        Err(_) => {
            if let Some(capture) = capture {
                capture.invalidate();
            }
        }
    }
    Guard {
        operation,
        detached: false,
    }
}

pub(super) fn current(ctx: &PipelineContext) -> Option<Arc<Operation>> {
    let capture = ctx
        .extension::<crate::tenancy::execution::TenantExecution>()
        .and_then(|execution| execution.provider_technical_capture())?;
    match ctx.provider_technical.try_lock() {
        Ok(slot) => slot
            .operation
            .clone()
            .filter(|operation| operation.belongs_to(&capture) && operation.healthy()),
        Err(_) => {
            capture.invalidate();
            None
        }
    }
}
pub(super) fn armed(ctx: &PipelineContext, id: &AttemptId) {
    if let Some(capture) = capture(ctx) {
        match ctx.provider_technical.try_lock() {
            Ok(slot) => {
                if let Some(hop) = &slot.hop {
                    hop.armed(id);
                }
            }
            Err(_) => capture.invalidate(),
        }
    }
    if let Some(operation) = current(ctx) {
        operation.armed(id);
    }
}
pub(super) fn selected(ctx: &PipelineContext) {
    if let Some(capture) = capture(ctx) {
        match ctx.provider_technical.try_lock() {
            Ok(slot) => {
                if let Some(hop) = &slot.hop {
                    hop.selected(false);
                }
            }
            Err(_) => capture.invalidate(),
        }
    }
    if let Some(operation) = current(ctx) {
        operation.selected();
    } else if let Some(capture) = ctx
        .extension::<crate::tenancy::execution::TenantExecution>()
        .and_then(|execution| execution.provider_technical_capture())
    {
        capture.clear_selected();
    }
}

pub(super) fn finish_selected(ctx: &PipelineContext) {
    if let Some(capture) = ctx
        .extension::<crate::tenancy::execution::TenantExecution>()
        .and_then(|execution| execution.provider_technical_capture())
    {
        capture.finish_selected();
    }
}
