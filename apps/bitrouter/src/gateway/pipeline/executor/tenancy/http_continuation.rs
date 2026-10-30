//! Only the authenticated executor may create an HTTP terminal proof. Public
//! caller/continuation/finalization fields are comparisons, never constructors.
use super::BoundAuthentication;
use crate::{
    continuation::tenant::{runtime::Runtime, state::Ticket},
    gateway::{
        model::protocol::responses::{self, CausalPrefixPlan, ContinuationSeed},
        pipeline::{
            context::PipelineContext, settlement::RequiredFinalizationContext, types::RoutingTarget,
        },
        providers::auth::ContinuationAuthority,
    },
    tenancy::continuation::http::codec::{Binding, Live, Owner, Payload},
};
use bitrouter_ai::{
    tenancy::operation::ErrorCode,
    types::{ApiProtocol, Prompt},
};
use std::sync::Arc;

pub(crate) struct Proof {
    owner: Owner,
    public_id: String,
    live: Live,
    delivered: responses::AssistantTurnCommitment,
}
impl Proof {
    pub(crate) fn payload(&self, ticket: &Ticket) -> Result<Payload, ErrorCode> {
        if self.owner != ticket.owner
            || self.public_id != ticket.public_id
            || self.live.execution != ticket.execution
        {
            return Err(ErrorCode::InvalidRequest);
        }
        Ok(Payload {
            format: 1,
            owner: self.owner.clone(),
            public_id: self.public_id.clone(),
            live: Some(self.live.clone()),
        })
    }
    pub(crate) fn same_source(&self, payload: &Payload) -> bool {
        payload.owner == self.owner
            && payload.public_id == self.public_id
            && payload.live.as_ref() == Some(&self.live)
    }
    pub(crate) fn matches(&self, runtime: &Runtime, ctx: &RequiredFinalizationContext) -> bool {
        let Some(target) = &ctx.target else {
            return false;
        };
        ctx.successful_terminal
            && ctx.native_response_completed
            && ctx.inbound_protocol == Some(ApiProtocol::Responses)
            && ctx.response_id.as_deref() == Some(self.live.provider_response_id.as_str())
            && ctx.caller.user_id() == self.owner.principal().as_str()
            && ctx.effective_model == self.live.effective_model
            && ctx.effective_effort == self.live.effective_effort
            && runtime.matches_delivered(&self.delivered)
            && self.live.causal_prefix.as_deref().is_none_or(|root| {
                ctx.causal_prefix_commitment.as_ref().map(|p| p.as_str()) == Some(root)
            })
            && responses::encode_gateway_continuation_id(&ctx.request_id)
                .is_ok_and(|id| id == self.public_id)
            && runtime
                .matches_binding(target, &self.live.binding)
                .is_ok_and(|v| v)
            && ctx.credential_authority.as_ref().is_some_and(|authority| {
                runtime
                    .authority_matches(authority, &self.live.binding)
                    .is_ok_and(|v| v)
            })
    }
}

/// The actual decoded output and complete raw terminal have separate proofs.
/// Only the latter can authorize visible-history detachment; neither an image
/// input nor a lossy raw-output projection invalidates exact provider resume.
pub(in crate::gateway::pipeline::executor) struct Output {
    pub delivered: Option<responses::AssistantTurnCommitment>,
    pub reconstructible: Option<responses::AssistantTurnCommitment>,
    pub stored: bool,
}

/// Fixed-size input commitment plus bounded IDs. It never retains a prompt,
/// transcript or provider terminal JSON. The attempt shares its existing runtime.
pub(in crate::gateway::pipeline::executor) struct Input {
    runtime: Arc<Runtime>,
    owner: Owner,
    public_id: String,
    effective_model: String,
    effective_effort: Option<bitrouter_ai::types::ReasoningEffort>,
    seed: Option<ContinuationSeed>,
}
impl Input {
    pub(in crate::gateway::pipeline::executor) fn new(
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> Option<Self> {
        let runtime = ctx.extension::<Runtime>()?;
        if !runtime.active(ctx) {
            return None;
        }
        let plan = ctx.extension::<CausalPrefixPlan>();
        Some(Self {
            owner: runtime.owner(),
            public_id: responses::encode_gateway_continuation_id(ctx.request_id()).ok()?,
            effective_model: ctx.model().to_owned(),
            effective_effort: prompt.params.reasoning_effort,
            seed: ContinuationSeed::new(&prompt.messages, plan.as_deref()),
            runtime,
        })
    }
}
impl BoundAuthentication {
    pub(in crate::gateway::pipeline::executor) fn record_http_source(
        &self,
        input: Input,
        attempt: bitrouter_ai::tenancy::identity::AttemptId,
        authority: &ContinuationAuthority,
        response_id: Option<&str>,
        output: Output,
    ) {
        let outcome = (|| {
            if !output.stored {
                return Ok(None);
            }
            // A post-hook output comparison is mandatory even without a causal
            // input seed. Unsupported delivered semantics cannot mint a proof.
            let Some(delivered) = output.delivered else {
                return Ok(None);
            };
            let root = input
                .seed
                .as_ref()
                .zip(
                    output
                        .reconstructible
                        .as_ref()
                        .filter(|turn| **turn == delivered),
                )
                .map(|(seed, turn)| seed.finish(turn));
            let response_id = response_id.ok_or(ErrorCode::InvalidRequest)?;
            if response_id.is_empty()
                || response_id.len() > 512
                || response_id.bytes().any(|b| b.is_ascii_control())
                || input.effective_model.len() > 512
            {
                return Err(ErrorCode::CapacityExceeded);
            }
            if input.owner != self.execution.http_owner() {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let crypto = self.execution.http_port().state.crypto()?;
            let live = Live {
                binding: Binding {
                    candidate: crypto
                        .candidate_tag(&self.candidate)
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?,
                    credential: self.credential.clone(),
                    credential_tag: crypto
                        .credential_tag(&self.credential)
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?,
                    authority: crypto
                        .authority_tag(authority)
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?,
                },
                revision: self.execution.http_revision().clone(),
                execution: self.execution.id().clone(),
                attempt,
                provider_response_id: response_id.into(),
                effective_model: input.effective_model,
                effective_effort: input.effective_effort,
                causal_prefix: root.map(|root| root.as_str().to_owned()),
            };
            Ok(Some(Arc::new(Proof {
                owner: input.owner,
                public_id: input.public_id,
                live,
                delivered,
            })))
        })();
        input.runtime.record(outcome);
    }
}

pub(in crate::gateway::pipeline::executor) fn bind_headers(
    authority: Option<ContinuationAuthority>,
    request: &reqwest::Request,
    target: &RoutingTarget,
    ctx: &PipelineContext,
) -> Option<ContinuationAuthority> {
    if !ctx
        .extension::<Runtime>()
        .is_some_and(|runtime| runtime.active(ctx))
        || target.headers.is_empty()
    {
        return authority;
    }
    let execution = super::execution(ctx)?;
    authority.map(|authority| {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(b"bitrouter.tenant.http.request-headers.v1");
        digest.update(authority.credential().proof_bytes());
        for rule in &target.headers {
            let name = rule.name().as_str().as_bytes();
            digest.update((name.len() as u64).to_be_bytes());
            digest.update(name);
            let values = request.headers().get_all(rule.name());
            digest.update((values.iter().count() as u64).to_be_bytes());
            for value in values.iter() {
                digest.update((value.as_bytes().len() as u64).to_be_bytes());
                digest.update(value.as_bytes());
            }
        }
        ContinuationAuthority::new(
            crate::gateway::providers::auth::CredentialAuthority::derive_scoped(
                "tenant-http-header-scope-v1",
                execution.installation_id().as_str(),
                &hex::encode(digest.finalize()),
            ),
            authority.effective_scheme(),
        )
    })
}
