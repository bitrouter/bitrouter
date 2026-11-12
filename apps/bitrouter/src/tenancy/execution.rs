//! Request-local authority for one exact retained execution revision.
//!
//! Public caller metadata is never a proof. Only this module can construct an
//! execution, after authenticating its credential and acquiring bounded capacity.
//! Current authority is checked again at every provider boundary; finalization
//! retains ownership without granting permission to start more provider work.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use bitrouter_ai::{
    tenancy::{
        authority::{GrantScope, OwnerKind, Permission, ResourceKind},
        configuration::ResourceRef,
        identity::{
            AttemptId, CorrelationId, CredentialId, ExecutionId, InstallationId, InstanceId,
            PrincipalId, RevisionId, TenantId,
        },
        operation::{DispatchBinding, DispatchState, ErrorCode},
        ordered::{OrderedCandidateSelectionV1, OrderedDispatchBindingV2},
        validation::Validate,
    },
    types::ApiProtocol,
};
use chrono::Utc;
use http::HeaderMap;

use super::{
    Service,
    error::Failure,
    model::{Caller, Snapshot},
    resources::execution::{RetainedExecutionRevision, RetainedRoute},
};
use crate::gateway::{
    caller::CallerContext, pipeline::types::RoutingTarget, providers::auth::credentials::Credential,
};

#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy) mod block;
pub(super) mod cached;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod choice;
mod continuation;
mod count;
pub(crate) mod deadline;
#[cfg(all(feature = "core", feature = "harness"))]
mod experiment;
mod generation_defaults;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod host;
pub(crate) mod http_continuation;
pub(crate) mod lifecycle;
mod native;
mod native_controls;
mod oauth;
mod operational;
pub(crate) mod private_adapter;
mod profile;
pub(crate) const BLOCK_LINK_HEADER: &str = "bitrouter-capture-link";
pub(crate) const SELECTION_HEADER: &str = "bitrouter-candidate-selection";
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod session;
#[cfg(test)]
mod tests;

/// Authenticated ingress identity. The tenant and principal come only from the
/// current credential record, never from caller-controlled headers or JSON.
#[derive(Clone)]
pub(crate) struct Submission {
    caller: Caller,
    authority: super::authority::live::Lease,
    preparation: super::authority::live::DependencyIntent,
    tenant: TenantId,
    principal: PrincipalId,
    credential: CredentialId,
    binding: DispatchIdentity,
    configuration_defaults: bitrouter_ai::tenancy::generation_defaults::ConfigurationDefaults,
    generate_content: Option<crate::gateway::pipeline::types::generate_content::Input>,
    deadline: deadline::Deadline,
    cancelled: tokio_util::sync::CancellationToken,
    #[cfg(all(feature = "core", feature = "harness"))]
    session_closed: Option<Arc<AtomicBool>>,
    #[cfg(all(feature = "core", feature = "harness"))]
    block: Option<Arc<super::capture::Binding>>,
}

/// Validated version-independent identity, separated from frozen public DTOs.
#[derive(Clone)]
struct DispatchIdentity {
    installation_id: InstallationId,
    instance_id: InstanceId,
    capability_revision: RevisionId,
    execution_revision: RevisionId,
    correlation_id: CorrelationId,
    selection: Option<OrderedCandidateSelectionV1>,
    native_controls: bool,
}

impl Submission {
    pub(in crate::tenancy) fn has_ordered_selection(&self) -> bool {
        self.binding.selection.is_some()
    }

    pub(crate) fn validate_http_input(
        &self,
        input: &crate::gateway::pipeline::types::PipelineInput,
    ) -> Result<(), Failure> {
        if let crate::gateway::pipeline::types::PipelineInput::GenerateContent(input) = input {
            if !self
                .generate_content
                .as_ref()
                .is_some_and(|own| own.same_original(input))
                || !input.cache_allowed(self.binding.native_controls)
            {
                return Err(ErrorCode::CapabilityMismatch.into());
            }
            return Ok(());
        }
        if self.binding.selection.is_some() {
            native_controls::validate_input(
                input.generation_prompt(),
                self.binding.native_controls,
            )
            .map_err(Failure::from)?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub(in crate::tenancy) fn narrow_deadline_for_test(
        mut self,
        deadline: tokio::time::Instant,
    ) -> Self {
        self.deadline = deadline::Deadline::Execution(Some(self.deadline.min(deadline)));
        self
    }
    pub(super) fn deadline(&self) -> tokio::time::Instant {
        // Ingress only accepts a bounded submission. Accidentally recycling an
        // unlimited admitted execution into ingress must fail closed.
        self.deadline
            .instant()
            .unwrap_or_else(tokio::time::Instant::now)
    }
}

/// One invocation, shared with detached execution, stream draining and delivery.
/// No Clone implementation can duplicate the lease or attempt counter.
pub(crate) struct TenantExecution {
    #[cfg(all(feature = "core", feature = "harness"))]
    experiment: std::sync::OnceLock<Arc<super::experiment::Witness>>,
    #[cfg(all(feature = "core", feature = "harness"))]
    assessment: std::sync::OnceLock<super::learning::source::Binding>,
    service: Service,
    submission: Submission,
    id: ExecutionId,
    route: Arc<RetainedRoute>,
    #[cfg(all(feature = "core", feature = "harness"))]
    block: std::sync::OnceLock<Arc<block::Witness>>,
    #[cfg(feature = "core")]
    learning: bitrouter_ai::tenancy::configuration::Learning,
    journal: std::sync::OnceLock<Arc<super::receipts::journal::Journal>>,
    rejection: Mutex<Option<ErrorCode>>,
    hop: Mutex<Option<Arc<super::operational::Hop>>>,
    preparation_claimed: AtomicBool,
    entered: AtomicBool,
    #[cfg(feature = "core")]
    observation: std::sync::OnceLock<Arc<super::eval::observation::RequiredObservation>>,
    continuation: Option<Arc<continuation::Policy>>,
    receipt: std::sync::OnceLock<Arc<super::receipts::owner::Owner>>,
    receipt_ticket: std::sync::OnceLock<lifecycle::AdmissionTicket>,
    #[cfg(all(feature = "core", feature = "harness"))]
    choice: Option<std::sync::OnceLock<Arc<crate::core::session::choice::SendGate>>>,
}

pub(crate) struct OwnedCredential {
    pub(crate) reference: ResourceRef,
    pub(crate) credential: Credential,
}

impl Service {
    fn submission_deadline(&self) -> deadline::Deadline {
        deadline::Deadline::Submission(deadline::after(std::time::Duration::from_secs(
            self.execution_settings.submission_timeout_secs,
        )))
    }

    pub(super) fn dispatch_ready(&self, state: &Snapshot) -> bool {
        // Production construction consumes the exclusive owned database and
        // completes bootstrap before publication. Unix eligibility still requires
        // every live authority, storage and admission fence below.
        #[cfg(test)]
        let qualified = self.dispatch_test_ready.load(Ordering::Acquire);
        #[cfg(not(test))]
        let qualified = cfg!(unix);
        qualified
            && !state.saturated
            && !self.stop.is_cancelled()
            && !self.caches.is_closed()
            && self.admission.receipts_ready()
    }

    pub(super) fn capability_revision(&self, state: &Snapshot) -> Result<RevisionId, ErrorCode> {
        format!(
            "{}-{}-{}",
            self.instance,
            u8::from(state.saturated),
            u8::from(self.dispatch_ready(state))
        )
        .try_into()
        .map_err(|_| ErrorCode::AuthorityUnavailable)
    }

    /// The publication gate spans every sequential owner check and final send.
    /// Individual view locks are released between checks, including aliases.
    pub(in crate::tenancy) fn with_dispatch_guard<T>(
        &self,
        submission: &Submission,
        check: impl FnOnce(&mut super::authority::live::ReadGuard<'_>) -> Result<T, ErrorCode>,
    ) -> Result<T, ErrorCode> {
        let mut guard = self.authority.read()?;
        if !self.live.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        #[cfg(all(feature = "core", feature = "harness"))]
        if submission
            .session_closed
            .as_ref()
            .is_some_and(|closed| closed.load(Ordering::Acquire))
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        if submission.deadline.expired() || submission.cancelled.is_cancelled() {
            return Err(ErrorCode::CapacityExceeded);
        }
        guard.check(&submission.authority, |view| {
            let state = view.facts();
            if !self.dispatch_ready(state)
                || (submission.binding.native_controls
                    && !self.execution_settings.ordered_native_controls)
                || (submission.binding.selection.is_some() && !self.ordered_dispatch_ready(state))
                || submission.binding.installation_id != state.installation
                || submission.binding.instance_id != self.instance
                || submission.binding.capability_revision != self.capability_revision(state)?
            {
                return Err(ErrorCode::CapabilityMismatch);
            }
            let effective = state.effective(&submission.caller, &submission.tenant, Utc::now())?;
            if effective.credential.principal != submission.principal
                || effective.credential.id != submission.credential
                || !effective.permits(Permission::ExecutionSubmit)
            {
                return Err(ErrorCode::Forbidden);
            }
            Ok(())
        })?;
        #[cfg(all(feature = "core", feature = "harness"))]
        if let Some(binding) = &submission.block {
            binding.block_guard(&mut guard)?;
        }
        check(&mut guard)
    }

    fn with_dispatch_view<T>(
        &self,
        submission: &Submission,
        check: impl FnOnce(&super::authority::live::View) -> Result<T, ErrorCode>,
    ) -> Result<T, ErrorCode> {
        self.with_dispatch_guard(submission, |guard| {
            guard.check(&submission.authority, check)
        })
    }

    fn with_dispatch_authority<T>(
        &self,
        submission: &Submission,
        check: impl FnOnce(&Snapshot) -> Result<T, ErrorCode>,
    ) -> Result<T, ErrorCode> {
        self.with_dispatch_view(submission, |view| check(view.facts()))
    }

    #[cfg(all(feature = "core", feature = "harness"))]
    pub(in crate::tenancy) async fn extend_submission(
        &self,
        submission: &Submission,
        dependencies: &[ResourceRef],
    ) -> Result<(), Failure> {
        self.prepare_submission(
            submission,
            dependencies,
            super::authority::live::DependencyIntent::Owner,
        )
        .await
    }

    async fn prepare_submission(
        &self,
        submission: &Submission,
        dependencies: &[ResourceRef],
        intent: super::authority::live::DependencyIntent,
    ) -> Result<(), Failure> {
        if self
            .with_dispatch_view(submission, |view| view.covers(dependencies, intent))
            .map_err(Failure::from)?
        {
            return Ok(());
        }
        self.admitted_authority_read(|| {
            // All queued copies together fit one third of the working share.
            // The read permit is acquired before reserving or cloning payloads.
            let bytes = self.authority.load_budget()
                / self.execution_settings.capacity.management.reads
                / 3;
            let required = dependencies
                .len()
                .checked_mul(512 + std::mem::size_of::<ResourceRef>())
                .ok_or(ErrorCode::CapacityExceeded)?;
            if required > bytes {
                return Err(ErrorCode::CapacityExceeded.into());
            }
            let mut copied = Vec::new();
            copied
                .try_reserve_exact(dependencies.len())
                .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
            copied.extend_from_slice(dependencies);
            Ok(super::actor::AuthoritySubject::Extend {
                caller: submission.caller.clone(),
                lease: submission.authority.clone(),
                dependencies: copied,
                intent,
            })
        })
        .await?;
        self.with_dispatch_authority(submission, |_| Ok(()))
            .map_err(Failure::from)
    }

    pub(crate) async fn submission(&self, headers: &HeaderMap) -> Result<Submission, Failure> {
        self.submission_with_intent(headers, super::authority::live::DependencyIntent::Fresh)
            .await
    }

    async fn submission_with_intent(
        &self,
        headers: &HeaderMap,
        preparation: super::authority::live::DependencyIntent,
    ) -> Result<Submission, Failure> {
        let binding = dispatch_binding(headers)?;
        if let Some(selection) = &binding.selection {
            if selection.candidate_indices.len()
                > usize::from(self.execution_settings.ordered_attempt_limit)
            {
                return Err(ErrorCode::CapacityExceeded.into());
            }
            if headers.contains_key(BLOCK_LINK_HEADER) {
                return Err(ErrorCode::CapabilityMismatch.into());
            }
        }
        if headers.contains_key("x-api-key")
            || headers.contains_key("bitrouter-tenant-id")
            || headers.contains_key("bitrouter-principal-id")
        {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let authorization = one_header(headers, "authorization")?
            .ok_or_else(|| Failure::from(ErrorCode::Unauthorized))?;
        let (scheme, token) = authorization
            .split_once(' ')
            .ok_or_else(|| Failure::from(ErrorCode::Unauthorized))?;
        if !scheme.eq_ignore_ascii_case("bearer")
            || token.is_empty()
            || token.bytes().any(|b| b.is_ascii_whitespace())
        {
            return Err(ErrorCode::Unauthorized.into());
        }
        let authenticated = self
            .authenticate_with_reuse(
                token,
                preparation == super::authority::live::DependencyIntent::Fresh,
            )
            .await?;
        let (tenant, principal, credential) = self
            .authority
            .read()
            .map_err(Failure::from)?
            .check(&authenticated.lease, |view| {
                let credential = view.facts().credential(&authenticated.caller, Utc::now())?;
                Ok((
                    credential.tenant.clone(),
                    credential.principal.clone(),
                    credential.id.clone(),
                ))
            })
            .map_err(Failure::from)?;
        #[cfg(not(all(feature = "core", feature = "harness")))]
        if headers.contains_key(BLOCK_LINK_HEADER) {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        let submission = Submission {
            caller: authenticated.caller,
            authority: authenticated.lease,
            preparation,
            tenant,
            principal,
            credential,
            binding,
            configuration_defaults:
                bitrouter_ai::tenancy::generation_defaults::ConfigurationDefaults::Apply,
            generate_content: None,
            deadline: self.submission_deadline(),
            cancelled: tokio_util::sync::CancellationToken::new(),
            #[cfg(all(feature = "core", feature = "harness"))]
            session_closed: None,
            #[cfg(all(feature = "core", feature = "harness"))]
            block: one_header(headers, BLOCK_LINK_HEADER)?
                .map(|link| self.linked_capture(link))
                .transpose()?,
        };
        self.with_dispatch_authority(&submission, |_| Ok(()))
            .map_err(Failure::from)?;
        #[cfg(all(feature = "core", feature = "harness"))]
        if submission
            .block
            .as_ref()
            .is_some_and(|binding| !binding.authority().matches_submission(&submission))
        {
            return Err(ErrorCode::Forbidden.into());
        }
        Ok(submission)
    }

    pub(crate) async fn admit_execution(
        &self,
        submission: Submission,
        raw_model: &str,
        inbound: &ApiProtocol,
    ) -> Result<Arc<TenantExecution>, Failure> {
        let selection = submission.binding.selection.clone();
        self.admit_selected_execution(submission, false, |revision| {
            if let Some(selection) = selection {
                if raw_model != selection.route.resource_id.as_str() {
                    return Err(ErrorCode::InvalidRequest);
                }
                revision.select_ordered(
                    &selection.route,
                    inbound,
                    &selection.candidate_indices,
                    selection.admission_mode,
                )
            } else {
                revision.select(raw_model, inbound)
            }
        })
        .await
    }

    async fn admit_native_execution(
        &self,
        submission: Submission,
        raw_model: &str,
    ) -> Result<Arc<TenantExecution>, Failure> {
        if submission.binding.selection.is_some() {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        self.admit_selected_execution(submission, false, |revision| {
            revision.select_native(raw_model, bitrouter_ai::types::ModelOperation::Generation)
        })
        .await
    }

    async fn admit_selected_execution(
        &self,
        mut submission: Submission,
        fail_fast: bool,
        select: impl FnOnce(
            &super::cache::Pin<RetainedExecutionRevision>,
        ) -> Result<Arc<RetainedRoute>, ErrorCode>,
    ) -> Result<Arc<TenantExecution>, Failure> {
        self.with_dispatch_authority(&submission, |_| Ok(()))
            .map_err(Failure::from)?;
        let revision = self.retained_execution(&submission).await?;
        let route = select(&revision).map_err(Failure::from)?;
        self.prepare_submission(&submission, route.dependencies(), submission.preparation)
            .await?;
        submission.preparation = super::authority::live::DependencyIntent::Owner;
        self.with_dispatch_view(&submission, |view| {
            authorize_effect(view.facts(), &submission, &route)?;
            self.require_static_ordered_credentials(view, &route)
        })
        .map_err(Failure::from)?;
        let mut limits = revision.document().limits.clone();
        if fail_fast {
            limits.queued_work = 0;
        }
        route.require_capabilities().map_err(Failure::from)?;
        #[cfg(all(feature = "core", feature = "harness"))]
        if route.block_profile().is_some() != submission.block.is_some()
            || submission
                .block
                .as_ref()
                .is_some_and(|binding| binding.block_route() != Some(route.reference()))
        {
            return Err(ErrorCode::Forbidden.into());
        }
        #[cfg(feature = "core")]
        let learning = revision.document().learning.clone();
        drop(revision);
        // Bound volumes are refreshed under accounting before count pins/grants.
        // Preserve the existing refusal/test behavior for an unbound volume.
        if self.receipt_volume.is_none() {
            let free = super::receipts::capacity::free_space(self.receipt_volume.as_deref())
                .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
            self.admission.receipt_probe(free);
        }
        let _count = self
            .pin_unknown_count(&submission.tenant, submission.deadline())
            .await?;
        let lease = self
            .admission
            .acquire_until(&submission.tenant, &limits, submission.deadline())
            .await
            .map_err(Failure::from)?;
        let intent_slot = route
            .is_ordered()
            .then(|| self.reserve_ordered_intent())
            .transpose()?;
        let execution = self.finish_admission(
            submission,
            route,
            lease,
            #[cfg(feature = "core")]
            learning,
            #[cfg(all(feature = "core", feature = "harness"))]
            false,
        )?;
        self.prepare_ordered_intent(&execution, intent_slot).await?;
        execution.revalidate().map_err(Failure::from)?;
        Ok(execution)
    }

    fn require_static_ordered_credentials(
        &self,
        view: &super::authority::live::View,
        route: &RetainedRoute,
    ) -> Result<(), ErrorCode> {
        if route.is_ordered() {
            for reference in route
                .dependencies()
                .iter()
                .filter(|reference| reference.resource_kind == ResourceKind::Credential)
            {
                if view.oauth(reference)?.is_some() {
                    return Err(ErrorCode::CapabilityMismatch);
                }
            }
        }
        Ok(())
    }

    fn finish_admission(
        &self,
        mut submission: Submission,
        route: Arc<RetainedRoute>,
        lease: super::admission::Lease,
        #[cfg(feature = "core")] learning: bitrouter_ai::tenancy::configuration::Learning,
        #[cfg(all(feature = "core", feature = "harness"))] choice: bool,
    ) -> Result<Arc<TenantExecution>, Failure> {
        // Waiting for capacity never extends a token/binding lifetime. The
        // exact submitted revision remains pinned even after activation changes.
        self.with_dispatch_view(&submission, |view| {
            authorize_effect(view.facts(), &submission, &route)?;
            self.require_static_ordered_credentials(view, &route)
        })
        .map_err(Failure::from)?;
        submission.deadline = submission
            .deadline
            .admit(self.execution_settings.execution_timeout_secs);
        let id = uuid::Uuid::new_v4()
            .to_string()
            .try_into()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        lease.bind_execution(&id).map_err(Failure::from)?;
        let continuation = self
            .native_continuation
            .clone()
            .filter(|_| !route.is_ordered())
            .map(|crypto| {
                Arc::new(continuation::Policy::new(
                    crypto,
                    &submission,
                    &id,
                    route.clone(),
                ))
            });
        let execution = Arc::new(TenantExecution {
            #[cfg(all(feature = "core", feature = "harness"))]
            experiment: std::sync::OnceLock::new(),
            #[cfg(all(feature = "core", feature = "harness"))]
            assessment: std::sync::OnceLock::new(),
            service: self.clone(),
            submission,
            id,
            continuation,
            route,
            #[cfg(all(feature = "core", feature = "harness"))]
            block: std::sync::OnceLock::new(),
            #[cfg(feature = "core")]
            learning,
            journal: std::sync::OnceLock::new(),
            rejection: Mutex::new(None),
            hop: Mutex::new(None),
            preparation_claimed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            #[cfg(feature = "core")]
            observation: std::sync::OnceLock::new(),
            receipt: std::sync::OnceLock::new(),
            receipt_ticket: std::sync::OnceLock::new(),
            #[cfg(all(feature = "core", feature = "harness"))]
            choice: choice.then(std::sync::OnceLock::new),
        });
        execution
            .journal
            .set(
                super::receipts::journal::Journal::new(&execution)
                    .map_err(|_| Failure::from(ErrorCode::CapabilityMismatch))?,
            )
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        let receipt = super::receipts::owner::Owner::new(
            super::receipts::owner::Identity {
                installation: execution.submission.binding.installation_id.clone(),
                tenant: execution.submission.tenant.clone(),
                principal: execution.submission.principal.clone(),
                execution: execution.id.clone(),
                correlation: execution.submission.binding.correlation_id.clone(),
                revision: execution.submission.binding.execution_revision.clone(),
                ordered: execution
                    .route
                    .accepted_selection(
                        execution
                            .submission
                            .binding
                            .selection
                            .as_ref()
                            .and_then(|selection| selection.generation_defaults_digest.as_ref()),
                    )
                    .map_err(Failure::from)?
                    .map(|selection| (selection, self.execution_settings.ordered_attempt_limit)),
            },
            execution.journal().map_err(Failure::from)?.clone(),
            self.completions.clone(),
            Arc::new(lease),
            self.admission.clone(),
            self.cleanup.clone(),
            (
                execution.submission.deadline,
                execution.submission.cancelled.clone(),
                &self.execution_settings,
                self.receipt_observer
                    .timing(&execution.submission.binding.correlation_id),
            ),
        );
        if receipt.identity.ordered.is_some() {
            let stamp = self
                .startup
                .as_ref()
                .ok_or_else(|| Failure::from(ErrorCode::CapabilityMismatch))?
                .bootstrap_stamp(&self.cleanup)
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            let intent = super::receipts::ordered::intent::Intent::new(&receipt.identity, stamp)
                .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
            receipt
                .intent
                .set(Arc::new(intent))
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        }
        let ticket = self
            .pipelines
            .admit(receipt.clone())
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        execution
            .receipt
            .set(receipt.clone())
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        execution
            .receipt_ticket
            .set(ticket)
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        receipt.observation.admitted();
        Ok(execution)
    }
}

impl TenantExecution {
    pub(crate) fn native_generate_content_encoder(
        &self,
    ) -> Option<Box<dyn crate::gateway::model::protocol::StreamEncoder>> {
        self.submission
            .generate_content
            .as_ref()?
            .native_stream_encoder()
    }
    pub(crate) fn native_generate_content_custody(
        &self,
    ) -> Option<Arc<crate::gateway::model::protocol::generate_content::native::stream::Custody>>
    {
        self.submission
            .generate_content
            .as_ref()?
            .native_stream_custody()
    }
    pub(crate) fn validate_generate_content(
        &self,
        input: &crate::gateway::pipeline::types::generate_content::Input,
    ) -> crate::gateway::error::Result<()> {
        if !self.route.is_ordered()
            || !self.profiled()
            || !self
                .submission
                .generate_content
                .as_ref()
                .is_some_and(|own| own.same_original(input))
            || !input.cache_allowed(self.submission.binding.native_controls)
        {
            return Err(self.reject(ErrorCode::CapabilityMismatch));
        }
        self.revalidate().map_err(|code| self.reject(code))
    }
    pub(crate) fn receipt_owner(
        &self,
    ) -> crate::gateway::error::Result<&Arc<super::receipts::owner::Owner>> {
        self.receipt.get().ok_or_else(|| {
            crate::gateway::error::BitrouterError::internal("admitted receipt owner missing")
        })
    }
    pub(crate) async fn terminal_receipt(
        &self,
        outcome: bitrouter_ai::tenancy::execution::Outcome,
    ) -> crate::gateway::error::Result<()> {
        self.receipt_owner()?
            .finish(outcome)
            .await
            .map_err(|code| {
                self.reject(code);
                crate::gateway::error::BitrouterError::UpstreamUnavailable
            })?;
        // Persisted provider work belongs to the admitted execution. Revoking
        // or rotating its caller blocks another send, not this completion.
        // The receipt await may still cross the execution's original lifetime
        // ceiling; continuation publication has its own tenant-state fence.
        if outcome == bitrouter_ai::tenancy::execution::Outcome::Completed
            && (self.submission.deadline.expired() || self.submission.cancelled.is_cancelled())
        {
            return Err(self.reject(ErrorCode::CapacityExceeded));
        }
        // Test-only caller scheduling AFTER the original durable receipt and
        // lifetime checks; no production ACK, actor, SQL or optional-write gate.
        #[cfg(all(test, unix))]
        self.service.technical_io.after_receipt_ack(&self.id).await;
        Ok(())
    }
    pub(crate) fn record_native_source(
        &self,
        proof: crate::gateway::pipeline::executor::tenancy::native::Proof,
    ) {
        if let Some(policy) = &self.continuation {
            policy.record(proof);
        }
    }

    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn send_trajectory(
        &self,
        permit: tokio::sync::mpsc::OwnedPermit<super::actor::Job>,
        job: super::actor::Job,
        deadline: tokio::time::Instant,
    ) -> Result<(), ErrorCode> {
        let _gate = self
            .service
            .accepting
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !self.trajectory_producer_ready() || tokio::time::Instant::now() >= deadline {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        drop(permit.send(job));
        Ok(())
    }

    #[cfg(feature = "core")]
    pub(crate) fn storage_database(&self) -> &sea_orm::DatabaseConnection {
        &self.service.db
    }
    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn trajectory_identity(
        &self,
    ) -> (
        &bitrouter_ai::tenancy::identity::RevisionId,
        &bitrouter_ai::tenancy::configuration::Learning,
    ) {
        (&self.submission.binding.execution_revision, &self.learning)
    }
    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn trajectory_resources(&self) -> &super::resources::Resources {
        &self.service.resources
    }
    #[cfg(all(test, feature = "core"))]
    pub(in crate::tenancy) fn trajectory_observation(
        &self,
    ) -> Option<&Arc<super::eval::observation::RequiredObservation>> {
        self.observation.get()
    }
    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn trajectory_sender(
        &self,
    ) -> tokio::sync::mpsc::Sender<super::actor::Job> {
        self.service.commands.clone()
    }
    #[cfg(feature = "core")]
    pub(in crate::tenancy) fn trajectory_producer_ready(&self) -> bool {
        self.service.eval.producers_open.load(Ordering::Acquire)
            && self.service.eval.healthy()
            && self.service.live.load(Ordering::Acquire)
    }

    #[cfg(feature = "core")]
    pub(crate) fn storage_identity(&self) -> (&InstallationId, &TenantId, &PrincipalId) {
        (
            &self.submission.binding.installation_id,
            &self.submission.tenant,
            &self.submission.principal,
        )
    }

    #[cfg(feature = "core")]
    pub(crate) fn eval_store(&self) -> crate::core::eval::store::EvalStore {
        crate::core::eval::store::EvalStore::for_execution(self.service.db.clone(), self)
    }

    #[cfg(feature = "core")]
    pub(super) fn eval_origin(&self, origin: &Arc<super::transaction::Proof>) -> bool {
        Arc::ptr_eq(origin, &self.service.cleanup)
    }

    #[cfg(feature = "core")]
    pub(super) fn observation_matches(
        &self,
        observation: &Arc<super::eval::observation::RequiredObservation>,
    ) -> bool {
        self.observation
            .get()
            .is_some_and(|own| Arc::ptr_eq(own, observation))
    }

    #[cfg(feature = "core")]
    pub(crate) async fn persist_observation(
        self: &Arc<Self>,
        observation: Arc<super::eval::observation::RequiredObservation>,
        subject: Arc<bitrouter_ai::eval::EvalSubject>,
        deadline: tokio::time::Instant,
    ) -> crate::gateway::error::Result<()> {
        self.service
            .persist_observation(self.clone(), observation, subject, deadline)
            .await
    }

    pub(crate) fn installation_id(&self) -> &InstallationId {
        &self.submission.binding.installation_id
    }

    pub(crate) fn id(&self) -> &ExecutionId {
        &self.id
    }

    pub(crate) fn remaining(&self) -> Option<std::time::Duration> {
        self.submission
            .deadline
            .instant()
            .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
    }

    pub(crate) fn caller(&self) -> CallerContext {
        CallerContext::new(
            self.submission.credential.as_str(),
            self.submission.principal.as_str(),
        )
    }

    pub(crate) fn route(&self) -> &Arc<RetainedRoute> {
        &self.route
    }

    /// Paid completion keeps its admitted caller authority. Output still obeys
    /// the execution lifetime and explicit revocation of its resource graph.
    pub(crate) fn check_delivery(&self) -> Result<(), ErrorCode> {
        if self.submission.deadline.expired() || self.submission.cancelled.is_cancelled() {
            return Err(ErrorCode::CapacityExceeded);
        }
        self.service
            .authority
            .read()?
            .check(&self.submission.authority, |view| {
                if self
                    .route
                    .dependencies()
                    .iter()
                    .any(|reference| view.facts().resource_lifecycle.revoked(reference))
                {
                    return Err(ErrorCode::Forbidden);
                }
                Ok(())
            })
    }

    /// All model, refresh and auxiliary sends share the same owner checks.
    fn with_send_authority<T>(
        &self,
        check: impl FnOnce(&super::authority::live::View) -> Result<T, ErrorCode>,
    ) -> Result<T, ErrorCode> {
        self.service.with_dispatch_guard(&self.submission, |guard| {
            #[cfg(all(feature = "core", feature = "harness"))]
            if let Some(binding) = self.assessment.get() {
                binding.assessment_guard(guard)?;
            }
            guard.check(&self.submission.authority, |view| {
                let rejection = self
                    .rejection
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                if let Some(code) = *rejection {
                    return Err(code);
                }
                authorize_effect(view.facts(), &self.submission, &self.route)?;
                check(view)
            })
        })
    }

    pub(crate) fn revalidate(&self) -> Result<(), ErrorCode> {
        self.with_send_authority(|_| Ok(()))
    }

    pub(crate) async fn credential(
        &self,
        target: &RoutingTarget,
    ) -> Result<OwnedCredential, ErrorCode> {
        self.revalidate()?;
        let reference = self.route.match_candidate(target)?;
        let credential = self
            .service
            .retained_credential(&self.submission, &reference)
            .await
            .map_err(|failure| failure.0.code)?;
        #[cfg(test)]
        self.service
            .receipt_credential_read_for_test(&self.id)
            .await;
        self.revalidate()?;
        Ok(OwnedCredential {
            reference,
            credential,
        })
    }

    pub(crate) fn journal(&self) -> Result<&Arc<super::receipts::journal::Journal>, ErrorCode> {
        self.journal.get().ok_or(ErrorCode::AuthorityUnavailable)
    }

    /// Exact static identity is validated and a slot is reserved before auth.
    pub(crate) fn reserve_attempt(
        &self,
        target: &RoutingTarget,
    ) -> Result<super::receipts::journal::AttemptPermit, ErrorCode> {
        self.revalidate()?;
        let stamp = super::receipts::journal::TargetStamp::for_execution(self, target)
            .map_err(super::receipts::journal_error)?;
        let mut permit = self
            .journal()?
            .reserve(stamp)
            .map_err(super::receipts::journal_error)?;
        permit.bind_operational(self.bound_hop(target)?);
        Ok(permit)
    }

    /// The last synchronous current-grant check and arming share one authority lock.
    #[cfg(test)]
    pub(crate) fn arm_attempt(
        &self,
        target: &RoutingTarget,
        permit: &mut super::receipts::journal::AttemptPermit,
        authenticated: &super::receipts::journal::TargetStamp,
    ) -> Result<AttemptId, ErrorCode> {
        self.arm_attempt_with_oauth(target, permit, authenticated, None, None)
    }

    pub(crate) fn arm_attempt_with_oauth(
        &self,
        target: &RoutingTarget,
        permit: &mut super::receipts::journal::AttemptPermit,
        authenticated: &super::receipts::journal::TargetStamp,
        oauth: Option<&super::oauth::ModelStamp>,
        counted: Option<&str>,
    ) -> Result<AttemptId, ErrorCode> {
        if self.route.is_ordered()
            && !self
                .receipt
                .get()
                .is_some_and(|owner| owner.intent_ready.load(Ordering::Acquire))
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let actual = super::receipts::journal::TargetStamp::for_execution(self, target)
            .map_err(super::receipts::journal_error)?;
        if !actual.matches(authenticated) {
            return Err(ErrorCode::Forbidden);
        }
        self.with_send_authority(|view| {
            #[cfg(all(feature = "core", feature = "harness"))]
            self.block_send(target)?;
            self.validate_oauth_model(view, target, oauth)?;
            self.verify_count_send(target, counted)?;
            let hop = self.bound_hop(target)?;
            if self.profiled()
                && let Some(hop) = &hop
            {
                hop.consume()?;
            }
            let id = permit
                .arm(authenticated)
                .map_err(super::receipts::journal_error)?;
            if let Some(hop) = hop {
                hop.armed(id.clone());
            }
            Ok(id)
        })
    }

    #[cfg(test)]
    pub(crate) fn admit_send(&self, target: &RoutingTarget) -> Result<AttemptId, ErrorCode> {
        let mut permit = self.reserve_attempt(target)?;
        let stamp = permit.stamp().clone();
        self.arm_attempt(target, &mut permit, &stamp)
    }

    /// Authority failures stop fallback and remain available to the HTTP layer
    /// as typed tenancy failures without exposing secret-bearing diagnostics.
    pub(crate) fn reject(&self, code: ErrorCode) -> crate::gateway::error::BitrouterError {
        if let Ok(mut rejection) = self.rejection.lock() {
            rejection.get_or_insert(code);
        }
        crate::gateway::error::BitrouterError::Forbidden(
            "tenant execution is no longer eligible".into(),
        )
    }

    /// Keep internal eligibility failures non-retryable, while exposing their
    /// actual tenancy class at HTTP ingress. A reserved send cannot prove that
    /// no provider work began; full per-attempt evidence is a separate stage.
    pub(crate) fn failure(&self) -> Option<Failure> {
        let code = self.receipt.get().and_then(|owner| owner.failure()).or(
            match self.rejection.lock() {
                Ok(rejection) => *rejection,
                Err(_) => Some(ErrorCode::AuthorityUnavailable),
            },
        )?;
        let mut failure = Failure::new(code, None);
        failure.0.execution_id = Some(self.id.clone());
        failure.0.dispatch_state = self
            .journal()
            .and_then(|journal| journal.dispatch().map_err(super::receipts::journal_error))
            .unwrap_or(DispatchState::Unknown);
        Some(failure)
    }
}

fn authorize_effect(
    state: &Snapshot,
    submission: &Submission,
    route: &RetainedRoute,
) -> Result<(), ErrorCode> {
    use bitrouter_ai::tenancy::admission::AdmissionMode;
    let mode = route.admission_mode();
    if state.resource_admission.protected(route.reference()) && mode != AdmissionMode::ProfiledV1 {
        return Err(ErrorCode::CapabilityMismatch);
    }
    authorize_route(state, submission, route.dependencies())?;
    if mode == AdmissionMode::ProfiledV1 {
        if !route.is_ordered()
            || !submission
                .binding
                .selection
                .as_ref()
                .is_some_and(|selection| {
                    selection.admission_mode == mode && &selection.route == route.reference()
                })
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        for pinned in route
            .dependencies()
            .iter()
            .filter(|reference| reference.resource_kind == ResourceKind::Policy)
        {
            state
                .resource_admission
                .current_profile(pinned)
                .ok_or(ErrorCode::Forbidden)?;
        }
        // Runtime admission is consumed separately at the actual arm boundary.
        // Authority-only reads and queue admission never spend rate capacity.
    }
    Ok(())
}

fn authorize_route(
    state: &Snapshot,
    submission: &Submission,
    dependencies: &[ResourceRef],
) -> Result<(), ErrorCode> {
    let effective = state.effective(&submission.caller, &submission.tenant, Utc::now())?;
    if dependencies.is_empty() {
        return Err(ErrorCode::Forbidden);
    }
    for reference in dependencies {
        if state.resource_lifecycle.revoked(reference) {
            return Err(ErrorCode::Forbidden);
        }
        if reference.owner_kind == OwnerKind::Tenant
            && reference.owner_id.as_str() != submission.tenant.as_str()
        {
            return Err(ErrorCode::Forbidden);
        }
        let permission = if reference.resource_kind == ResourceKind::Credential {
            Permission::CredentialUse
        } else {
            Permission::ExecutionSubmit
        };
        if !effective.permits(permission)
            || !effective.covers(&GrantScope::Resources {
                resources: vec![reference.key()],
            })
        {
            return Err(ErrorCode::Forbidden);
        }
    }
    Ok(())
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, Failure> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .map(|v| v.to_str())
        .transpose()
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    if values.next().is_some() {
        return Err(ErrorCode::InvalidRequest.into());
    }
    Ok(value)
}

fn dispatch_binding(headers: &HeaderMap) -> Result<DispatchIdentity, Failure> {
    use bitrouter_ai::tenancy::native_controls::{HEADER, PROVIDER_NATIVE_V1};
    let native_controls = match one_header(headers, HEADER)? {
        None => false,
        Some(PROVIDER_NATIVE_V1) => true,
        Some(_) => return Err(ErrorCode::InvalidRequest.into()),
    };
    let mut document = serde_json::Map::new();
    for name in [
        "Bitrouter-Tenancy-Version",
        "Bitrouter-Installation-Id",
        "Bitrouter-Instance-Id",
        "Bitrouter-Capability-Revision",
        "Bitrouter-Execution-Revision",
        "Bitrouter-Correlation-Id",
        "Bitrouter-Receipt-Mode",
    ] {
        let value =
            one_header(headers, name)?.ok_or_else(|| Failure::from(ErrorCode::InvalidRequest))?;
        document.insert(name.into(), value.into());
    }
    match document
        .get("Bitrouter-Tenancy-Version")
        .and_then(serde_json::Value::as_str)
    {
        Some("1") => {
            if headers.contains_key(SELECTION_HEADER) || native_controls {
                return Err(ErrorCode::InvalidRequest.into());
            }
            let binding: DispatchBinding = serde_json::from_value(document.into())
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
            binding
                .validate()
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
            Ok(DispatchIdentity {
                installation_id: binding.installation_id,
                instance_id: binding.instance_id,
                capability_revision: binding.capability_revision,
                execution_revision: binding.execution_revision,
                correlation_id: binding.correlation_id,
                selection: None,
                native_controls: false,
            })
        }
        Some("2") => {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            // RFC 4648 sections 3.5 and 5: canonical URL-safe alphabet, with
            // padding explicitly omitted by this header's wire contract.
            // https://www.rfc-editor.org/rfc/rfc4648.html#section-5
            let encoded = one_header(headers, SELECTION_HEADER)?
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= bitrouter_ai::tenancy::ordered::MAX_SELECTION_HEADER_BYTES
                })
                .ok_or_else(|| Failure::from(ErrorCode::InvalidRequest))?;
            let bytes = URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
            if URL_SAFE_NO_PAD.encode(&bytes) != encoded {
                return Err(ErrorCode::InvalidRequest.into());
            }
            let selection: OrderedCandidateSelectionV1 = serde_json::from_slice(&bytes)
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
            selection
                .validate()
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
            document.insert(
                "selection".into(),
                serde_json::to_value(selection)
                    .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?,
            );
            let binding: OrderedDispatchBindingV2 = serde_json::from_value(document.into())
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
            binding
                .validate()
                .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
            Ok(DispatchIdentity {
                installation_id: binding.installation_id,
                instance_id: binding.instance_id,
                capability_revision: binding.capability_revision,
                execution_revision: binding.execution_revision,
                correlation_id: binding.correlation_id,
                selection: Some(binding.selection),
                native_controls,
            })
        }
        _ => Err(ErrorCode::InvalidRequest.into()),
    }
}

fn ordered_prompt(prompt: Option<&bitrouter_ai::types::Prompt>) -> Result<(), ErrorCode> {
    let prompt = prompt.ok_or(ErrorCode::CapabilityMismatch)?;
    if [
        "previous_response_id",
        "conversation",
        "context_management",
        "cachedContent",
    ]
    .iter()
    .any(|field| {
        [&prompt.params.extra, &prompt.params.supplemental_extra]
            .iter()
            .any(|values| values.get(*field).is_some_and(|value| !value.is_null()))
    }) {
        return Err(ErrorCode::CapabilityMismatch);
    }
    Ok(())
}
