//! Request-local tenant branch of ContinuationRuntime. All mutable durable work
//! uses the existing registry port; this value owns only its request markers.
use super::*;
use crate::{
    gateway::{
        pipeline::{executor::tenancy::http_continuation::Proof, native::NativeManagedRequest},
        providers::auth::ContinuationAuthority,
    },
    tenancy::{
        continuation::http::{
            Compensated,
            codec::{Binding, Owner, Payload},
            store::Resolution,
        },
        execution::TenantExecution,
    },
};
use bitrouter_ai::{
    tenancy::operation::ErrorCode,
    types::{ModelOperation, Role},
};
use state::{Reservation, Ticket};

struct Request {
    reservation: Reservation,
    previous: Option<Arc<Payload>>,
    detach: bool,
}
struct RequestSnapshot {
    ticket: Ticket,
    previous: Option<Arc<Payload>>,
    detach: bool,
}
struct Finalization {
    ticket: Ticket,
    receipt: RequiredFinalizationReceipt,
    compensation: Option<Box<Compensated>>,
}
pub(crate) struct Runtime {
    registry: ContinuationRegistry,
    execution: Arc<TenantExecution>,
    request: Mutex<Option<Request>>,
    finalization: Mutex<Option<Finalization>>,
    proof: Mutex<std::result::Result<Option<Arc<Proof>>, ErrorCode>>,
    delivered: Mutex<Option<crate::gateway::model::protocol::responses::AssistantTurnCommitment>>,
}
impl Runtime {
    fn error(&self, code: ErrorCode) -> BitrouterError {
        self.execution.reject(code)
    }
    fn deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(30)
    }
    fn port(&self) -> &Arc<Port> {
        self.execution.http_port()
    }
    pub(crate) fn owner(&self) -> Owner {
        self.execution.http_owner()
    }
    pub(crate) fn active(&self, ctx: &PipelineContext) -> bool {
        ctx.operation() == ModelOperation::Generation
            && ctx.inbound_protocol() == Some(ApiProtocol::Responses)
            && ctx.extension::<NativeManagedRequest>().is_none()
    }
    pub(crate) fn record(&self, proof: std::result::Result<Option<Arc<Proof>>, ErrorCode>) {
        if let Ok(mut current) = self.proof.lock() {
            *current = proof;
        }
    }
    /// Pipeline-only post-hook output observation. This cannot create an
    /// executor proof; it must match that attempt's independently recorded output.
    pub(crate) fn observe_delivered(
        &self,
        value: Option<crate::gateway::model::protocol::responses::AssistantTurnCommitment>,
    ) {
        if let Ok(mut delivered) = self.delivered.lock() {
            *delivered = value;
        }
    }
    pub(crate) fn matches_delivered(
        &self,
        value: &crate::gateway::model::protocol::responses::AssistantTurnCommitment,
    ) -> bool {
        self.delivered
            .lock()
            .is_ok_and(|delivered| delivered.as_ref() == Some(value))
    }
    fn current(&self) -> PipelineResult<Option<RequestSnapshot>> {
        let guard = self
            .request
            .lock()
            .map_err(|_| self.error(ErrorCode::AuthorityUnavailable))?;
        Ok(guard.as_ref().map(|request| RequestSnapshot {
            ticket: request.reservation.ticket.clone(),
            previous: request.previous.clone(),
            detach: request.detach,
        }))
    }
    pub(crate) fn matches_binding(
        &self,
        target: &RoutingTarget,
        binding: &Binding,
    ) -> std::result::Result<bool, ErrorCode> {
        if target.api_protocol != ApiProtocol::Responses {
            return Ok(false);
        }
        let crypto = self.port().state.crypto()?;
        let candidate = self.execution.route().candidate_key(target)?;
        Ok(
            self.execution.route().match_candidate(target)? == binding.credential
                && crypto
                    .candidate_tag(&candidate)
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?
                    == binding.candidate,
        )
    }
    pub(crate) fn authority_matches(
        &self,
        authority: &ContinuationAuthority,
        binding: &Binding,
    ) -> std::result::Result<bool, ErrorCode> {
        Ok(self
            .port()
            .state
            .crypto()?
            .authority_tag(authority)
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            == binding.authority)
    }
    fn validate(&self, ctx: &PipelineContext) -> PipelineResult<()> {
        if !self.active(ctx) {
            // Another inbound wire can still select a Responses provider. Its
            // opaque extras must not smuggle an unowned provider continuation
            // through cross-protocol rendering. Native owns its separate031
            // proof path; Decisions never sends a Generation request.
            if ctx.operation() == ModelOperation::Generation
                && ctx.extension::<NativeManagedRequest>().is_none()
                && ["previous_response_id", "conversation"].iter().any(|name| {
                    ctx.generation_prompt().is_some_and(|prompt| {
                        prompt.params.extra.get(*name).is_some_and(|v| !v.is_null())
                    })
                })
            {
                return Err(self.error(ErrorCode::InvalidRequest));
            }
            return Ok(());
        }
        if !ctx
            .extension::<TenantExecution>()
            .is_some_and(|e| Arc::ptr_eq(&e, &self.execution))
            || ctx.extension::<ProviderContinuation>().is_some()
            || ctx.extension::<SuppressProviderContinuation>().is_some()
        {
            return Err(self.error(ErrorCode::Forbidden));
        }
        let prompt = ctx.require_generation_prompt()?;
        let current = self.current()?;
        // Routing may replace the canonical model with an approved selector.
        // The immutable ingress identity stays literal; only first entry also
        // requires the unprepared prompt to carry that same resource name.
        let literal = self.owner();
        let literal = literal.route().resource_id.as_str();
        if ctx.original_model() != literal
            || (current.is_none() && prompt.model != literal)
            || prompt
                .params
                .extra
                .get("conversation")
                .is_some_and(|v| !v.is_null())
        {
            return Err(self.error(ErrorCode::InvalidRequest));
        }
        if let Some(RequestSnapshot { previous, .. }) = current {
            let current = prompt
                .params
                .extra
                .get("previous_response_id")
                .filter(|v| !v.is_null());
            if current.and_then(serde_json::Value::as_str)
                != previous.as_ref().map(|p| p.public_id.as_str())
                || current.is_some_and(|v| !v.is_string())
            {
                return Err(self.error(ErrorCode::InvalidRequest));
            }
            if let Some(previous) = previous {
                let live = previous
                    .live
                    .as_ref()
                    .ok_or_else(|| self.error(ErrorCode::InvalidRequest))?;
                if prompt.params.reasoning_effort != live.effective_effort {
                    return Err(self.error(ErrorCode::InvalidRequest));
                }
            }
        }
        Ok(())
    }
    pub(in crate::continuation) async fn check(
        self: &Arc<Self>,
        ctx: &mut PipelineContext,
    ) -> PipelineResult<HookDecision> {
        self.validate(ctx)?;
        if !self.active(ctx) {
            if ctx.operation() == ModelOperation::Generation
                && ctx.extension::<NativeManagedRequest>().is_none()
            {
                // Cross-wire rendering still needs the no-parent egress check.
                // There is no continuation reservation or lookup on this branch.
                ctx.insert_extension(self.clone());
            }
            return Ok(HookDecision::Allow);
        }
        self.execution.revalidate().map_err(|e| self.error(e))?;
        if self.current()?.is_some() {
            return Err(self.error(ErrorCode::InvalidRequest));
        }
        let public = encode_gateway_continuation_id(ctx.request_id())?;
        let ticket = self
            .port()
            .state
            .reserve(
                self.owner(),
                self.execution.id().clone(),
                public,
                ctx.delivery_attempt_id(),
            )
            .map_err(|e| self.error(e))?;
        let request = Request {
            reservation: Reservation {
                state: self.port().state.clone(),
                ticket,
            },
            previous: None,
            detach: false,
        };
        *self
            .request
            .lock()
            .map_err(|_| self.error(ErrorCode::AuthorityUnavailable))? = Some(request);
        ctx.insert_extension(self.clone());
        let previous = ctx
            .require_generation_prompt()?
            .params
            .extra
            .get("previous_response_id")
            .filter(|v| !v.is_null())
            .cloned();
        if let Some(previous) = previous {
            let public = previous
                .as_str()
                .ok_or_else(|| self.error(ErrorCode::InvalidRequest))?;
            validate_public_id(public).map_err(|_| self.error(ErrorCode::InvalidRequest))?;
            let access = self.execution.http_access().map_err(|e| self.error(e))?;
            let response = self
                .port()
                .resolve(access, public.to_owned())
                .await
                .map_err(|e| self.error(e))?;
            self.execution.revalidate().map_err(|e| self.error(e))?;
            let active = match response {
                Resolution::Active(active) => active,
                Resolution::Missing | Resolution::Expired => {
                    return Err(self.error(ErrorCode::NotFound));
                }
            };
            let live = active
                .live
                .as_ref()
                .ok_or_else(|| self.error(ErrorCode::InvalidRequest))?;
            if ctx
                .require_generation_prompt()?
                .params
                .reasoning_effort
                .is_some_and(|effort| Some(effort) != live.effective_effort)
            {
                return Err(self.error(ErrorCode::InvalidRequest));
            }
            ctx.set_tenant_continuation_effort(live.effective_effort)?;
            let prefix = live
                .causal_prefix
                .as_deref()
                .and_then(CausalPrefixCommitment::parse);
            let exact = prefix.as_ref().and_then(|p| {
                authenticated_visible_causal_prefix(ctx.require_generation_prompt().ok()?, p)
            });
            let detach = exact.is_some();
            let causal = match (prefix, exact) {
                (Some(prior), Some(start)) => CausalPrefixPlan::extend(prior, start),
                (Some(prior), None)
                    if !ctx
                        .require_generation_prompt()?
                        .messages
                        .iter()
                        .any(|m| m.role == Role::Assistant) =>
                {
                    CausalPrefixPlan::extend(prior, 0)
                }
                _ => CausalPrefixPlan::ineligible(),
            };
            ctx.insert_extension(Arc::new(causal));
            let mut guard = self
                .request
                .lock()
                .map_err(|_| self.error(ErrorCode::AuthorityUnavailable))?;
            let request = guard
                .as_mut()
                .ok_or_else(|| self.error(ErrorCode::AuthorityUnavailable))?;
            request.previous = Some(Arc::from(active));
            request.detach = detach;
        }
        Ok(HookDecision::Allow)
    }
    pub(in crate::continuation) fn revalidate(&self, ctx: &PipelineContext) -> PipelineResult<()> {
        self.validate(ctx)
    }
    pub(in crate::continuation) fn route(
        &self,
        chain: &mut Vec<RoutingTarget>,
        ctx: &PipelineContext,
    ) -> PipelineResult<()> {
        self.validate(ctx)?;
        if !self.active(ctx) {
            return Ok(());
        }
        if let Some(RequestSnapshot {
            previous: Some(previous),
            ..
        }) = self.current()?
        {
            let live = previous
                .live
                .as_ref()
                .ok_or_else(|| self.error(ErrorCode::InvalidRequest))?;
            let mut approved = Vec::new();
            for target in chain.iter() {
                if self
                    .matches_binding(target, &live.binding)
                    .map_err(|e| self.error(e))?
                {
                    approved.push(target.clone());
                }
            }
            if approved.is_empty() {
                return Err(self.error(ErrorCode::InvalidRequest));
            }
            *chain = approved;
        }
        Ok(())
    }
    /// Substitution is private to this runtime and the exact selected candidate.
    pub(crate) fn substitute(
        &self,
        body: &mut serde_json::Value,
        target: &RoutingTarget,
        ctx: &PipelineContext,
    ) -> PipelineResult<Option<(String, String)>> {
        self.validate(ctx)?;
        if body.get("conversation").is_some_and(|v| !v.is_null()) {
            return Err(self.error(ErrorCode::InvalidRequest));
        }
        let Some(RequestSnapshot {
            previous: Some(previous),
            detach,
            ..
        }) = self.current()?
        else {
            if body
                .get("previous_response_id")
                .is_some_and(|v| !v.is_null())
            {
                return Err(self.error(ErrorCode::InvalidRequest));
            }
            return Ok(None);
        };
        let live = previous
            .live
            .as_ref()
            .ok_or_else(|| self.error(ErrorCode::InvalidRequest))?;
        if !self
            .matches_binding(target, &live.binding)
            .map_err(|e| self.error(e))?
        {
            return Err(self.error(ErrorCode::Forbidden));
        }
        let object = body
            .as_object_mut()
            .ok_or_else(|| self.error(ErrorCode::InvalidRequest))?;
        if object
            .get("previous_response_id")
            .and_then(serde_json::Value::as_str)
            != Some(&previous.public_id)
            || object
                .get("reasoning")
                .and_then(|r| r.get("effort"))
                .cloned()
                .unwrap_or(serde_json::Value::Null)
                != serde_json::to_value(live.effective_effort)
                    .map_err(|_| self.error(ErrorCode::InvalidRequest))?
        {
            return Err(self.error(ErrorCode::InvalidRequest));
        }
        if detach {
            object.remove("previous_response_id");
        } else {
            object.insert(
                "previous_response_id".into(),
                live.provider_response_id.clone().into(),
            );
        }
        Ok(Some((
            live.provider_response_id.clone(),
            previous.public_id.clone(),
        )))
    }
    pub(crate) fn authenticated(
        &self,
        target: &RoutingTarget,
        ctx: &PipelineContext,
        authority: Option<&ContinuationAuthority>,
    ) -> PipelineResult<()> {
        self.validate(ctx)?;
        if let Some(RequestSnapshot {
            previous: Some(previous),
            ..
        }) = self.current()?
        {
            let live = previous
                .live
                .as_ref()
                .ok_or_else(|| self.error(ErrorCode::InvalidRequest))?;
            let authority = authority.ok_or_else(|| self.error(ErrorCode::Forbidden))?;
            if !self
                .matches_binding(target, &live.binding)
                .map_err(|e| self.error(e))?
                || !self
                    .authority_matches(authority, &live.binding)
                    .map_err(|e| self.error(e))?
            {
                return Err(self.error(ErrorCode::Forbidden));
            }
        }
        Ok(())
    }
    pub(crate) fn confirmed_compensation(&self, receipt: &RequiredFinalizationReceipt) -> bool {
        self.finalization.lock().is_ok_and(|state| {
            state.as_ref().is_some_and(|state| {
                state.receipt.same_invocation(receipt)
                    && state
                        .compensation
                        .as_ref()
                        .is_some_and(|ack| ack.matches(self.port(), &state.ticket))
            })
        })
    }
    pub(in crate::continuation) async fn finalize(
        &self,
        ctx: &RequiredFinalizationContext,
        receipt: Option<&RequiredFinalizationReceipt>,
    ) -> PipelineResult<()> {
        let Some(RequestSnapshot { ticket, .. }) = self.current()? else {
            return Ok(());
        };
        if let Some(receipt) = receipt {
            let mut state = self
                .finalization
                .lock()
                .map_err(|_| self.error(ErrorCode::AuthorityUnavailable))?;
            if state.is_some() {
                return Err(self.error(ErrorCode::AuthorityUnavailable));
            }
            *state = Some(Finalization {
                ticket: ticket.clone(),
                receipt: receipt.clone(),
                compensation: None,
            });
        }
        let proof = self
            .proof
            .lock()
            .map_err(|_| self.error(ErrorCode::AuthorityUnavailable))?
            .clone()
            .map_err(|e| self.error(e))?;
        let Some(proof) = proof else {
            self.port().state.release(&ticket);
            return Ok(());
        };
        if !proof.matches(self, ctx) {
            return Err(self.error(ErrorCode::InvalidRequest));
        }
        let receipt = receipt.ok_or_else(|| self.error(ErrorCode::AuthorityUnavailable))?;
        let deadline = Self::deadline();
        let lock = self.registry.pending_identity_lock(&ticket.physical);
        let _guard = tokio::time::timeout_at(deadline, lock.lock())
            .await
            .map_err(|_| self.error(ErrorCode::CapacityExceeded))?;
        self.port()
            .prepare(ticket, proof, receipt, deadline)
            .await
            .map_err(|e| self.error(e))?;
        Ok(())
    }
    pub(in crate::continuation) async fn rollback(&self) -> PipelineResult<()> {
        self.rollback_with_receipt(None).await
    }
    pub(in crate::continuation) async fn rollback_with_receipt(
        &self,
        receipt: Option<&RequiredFinalizationReceipt>,
    ) -> PipelineResult<()> {
        let Some(RequestSnapshot { ticket, .. }) = self.current()? else {
            return Ok(());
        };
        self.port().state.abandon(&ticket);
        // Unqueued reservations were synchronously released. Queued work keeps
        // Reserved until the owning actor acknowledges a write or rollback.
        let deadline = Self::deadline();
        let lock = self.registry.pending_identity_lock(&ticket.physical);
        let _guard = tokio::time::timeout_at(deadline, lock.lock())
            .await
            .map_err(|_| self.error(ErrorCode::AuthorityUnavailable))?;
        let acknowledged = self
            .port()
            .compensate_confirmed(ticket, deadline)
            .await
            .map_err(|e| self.error(e))?;
        if let Some(receipt) = receipt {
            let mut state = self
                .finalization
                .lock()
                .map_err(|_| self.error(ErrorCode::AuthorityUnavailable))?;
            if let Some(state) = state.as_mut()
                && state.receipt.same_invocation(receipt)
                && acknowledged.matches(self.port(), &state.ticket)
            {
                state.compensation = Some(acknowledged);
            }
        }
        Ok(())
    }
    pub(in crate::continuation) async fn commit(
        &self,
        delivery: &RequiredDeliveryHandshake,
    ) -> PipelineResult<bool> {
        let Some(RequestSnapshot { ticket, .. }) = self.current()? else {
            return delivery.wait_for_delivery().await;
        };
        if !self.port().state.matches(&ticket) {
            return delivery.wait_for_delivery().await;
        }
        let deadline = Self::deadline();
        let result = async {
            self.transition(&ticket, Step::Delivering, deadline).await?;
            if !delivery.wait_for_delivery_acknowledgement().await? {
                self.rollback().await?;
                let _ = delivery.complete_activation(Ok(()));
                return Ok::<bool, BitrouterError>(false);
            }
            self.transition(&ticket, Step::Active, deadline).await?;
            if !delivery.complete_activation(Ok(())) || !delivery.wait_for_terminal_commit().await {
                self.rollback().await?;
                return Ok(false);
            }
            self.port().state.release(&ticket);
            Ok(true)
        };
        match tokio::time::timeout_at(deadline, result).await {
            Ok(Ok(delivered)) => Ok(delivered),
            Ok(Err(error)) => {
                self.port().state.abandon(&ticket);
                delivery.reject(error.clone());
                Err(error)
            }
            Err(_) => {
                self.port().state.abandon(&ticket);
                let error = self.error(ErrorCode::AuthorityUnavailable);
                delivery.reject(error.clone());
                Err(error)
            }
        }
    }
    async fn transition(
        &self,
        ticket: &Ticket,
        step: Step,
        deadline: tokio::time::Instant,
    ) -> PipelineResult<()> {
        let lock = self.registry.pending_identity_lock(&ticket.physical);
        let _guard = tokio::time::timeout_at(deadline, lock.lock())
            .await
            .map_err(|_| self.error(ErrorCode::AuthorityUnavailable))?;
        self.port()
            .advance(ticket.clone(), step, deadline)
            .await
            .map_err(|e| self.error(e))
    }
    pub(in crate::continuation) async fn drain(&self) -> PipelineResult<()> {
        self.rollback().await?;
        if self
            .port()
            .state
            .pending_for(self.execution.id())
            .map_err(|e| self.error(e))?
        {
            return Err(self.error(ErrorCode::AuthorityUnavailable));
        }
        Ok(())
    }
}
fn validate_public_id(value: &str) -> Result<()> {
    crate::tenancy::continuation::http::codec::validate_public(value)
}
impl ContinuationRuntime {
    pub(crate) fn for_tenant(
        registry: ContinuationRegistry,
        execution: Arc<TenantExecution>,
    ) -> Result<Self> {
        registry.tenant_backend()?;
        let runtime = Arc::new(Runtime {
            registry: registry.clone(),
            execution,
            request: Mutex::new(None),
            finalization: Mutex::new(None),
            proof: Mutex::new(Ok(None)),
            delivered: Mutex::new(None),
        });
        Ok(Self {
            registry,
            auth_appliers: AuthAppliers::new(),
            tenant: Some(runtime),
        })
    }
}
