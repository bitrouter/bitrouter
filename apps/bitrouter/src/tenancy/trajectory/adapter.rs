use super::super::transaction::Disposition;
use super::*;
use crate::{
    gateway::{error::Result as GatewayResult, pipeline::context::PipelineContext},
    trajectory::{
        canonical::Canonicalizer,
        correlation::{protocol_name, stable_id},
        store::GuardedRouteInput,
    },
};
use bitrouter_ai::{tenancy::configuration::LearningMode, types::ApiProtocol};

impl Adapter {
    pub(in crate::tenancy) fn new(
        execution: &Arc<TenantExecution>,
        observation: Arc<RequiredObservation>,
    ) -> Result<Arc<Self>> {
        let (installation, tenant, _) = execution.storage_identity();
        let (revision, learning) = execution.trajectory_identity();
        let route = execution.route();
        let policy = route
            .policy()
            .context("guarded route has no retained policy")?;
        let key = execution
            .trajectory_resources()
            .trajectory_crypto(installation)?
            .correlation_key(
                installation,
                tenant,
                revision,
                route.reference(),
                policy.reference(),
            )?;
        Ok(Arc::new(Self {
            execution: execution.clone(),
            observation,
            canonicalizer: Canonicalizer::new(key),
            observe: learning.mode != LearningMode::Off,
        }))
    }
    pub(crate) async fn begin(
        &self,
        ctx: &PipelineContext,
        protocol: ApiProtocol,
        captured_at: &str,
        mut guarded: GuardedRouteInput,
    ) -> GatewayResult<BeginReply> {
        self.observation.start_requested()?;
        // Everything before send is known not to have touched SQL. A rejected
        // input may skip only with this proof and a Failed pipeline outcome.
        let queued = async {
            let attached = ctx.extension::<TenantExecution>();
            if !attached
                .as_ref()
                .is_some_and(|e| Arc::ptr_eq(e, &self.execution))
                || ctx.request_id() != self.execution.id().as_str()
                || ctx.caller().user_id() != self.execution.caller().user_id()
                || ctx.caller().api_key_id() != self.execution.caller().api_key_id()
            {
                return Err(rejection(&self.execution, ErrorCode::Forbidden));
            }
            self.execution
                .revalidate()
                .map_err(|c| rejection(&self.execution, c))?;
            let reservation = super::reservation::prepare(&self.execution, ctx, self.observe)
                .map_err(|_| rejection(&self.execution, ErrorCode::CapacityExceeded))?;
            let prompt = ctx.require_generation_prompt()?;
            // A continuation requires its own retained key/profile proof. Never
            // reinterpret its ID as an ambient or caller-selected parent here.
            if prompt.params.extra.contains_key("previous_response_id") {
                return Err(rejection(&self.execution, ErrorCode::CapabilityMismatch));
            }
            let canonical = self
                .canonicalizer
                .canonicalize(prompt)
                .map_err(|_| rejection(&self.execution, ErrorCode::InvalidRequest))?;
            let owner = crate::trajectory::store::TrajectoryStore::for_execution(&self.execution)
                .owner()
                .map_err(|_| error())?;
            let request = self
                .canonicalizer
                .request_identity(&owner, ctx.request_id())
                .map_err(|_| error())?;
            guarded.route_event_id = stable_id("route-intent", &owner, &request);
            guarded.guard_event_id = stable_id("guard-activation", &owner, &request);
            let evidence = CorrelationEvidence {
                native_parent_id: None,
                full_input_digest: canonical.full_input_digest.as_str().into(),
                ancestor_prefix_digests: canonical
                    .ancestor_prefix_digests
                    .iter()
                    .map(|d| d.as_str().into())
                    .collect(),
                ancestor_prefixes_truncated: canonical.ancestor_prefixes_truncated,
                starts_with_prior_turns: canonical.starts_with_prior_turns,
                canonical_input_bytes: canonical.canonical_input_bytes,
            };
            let input = Arc::new(CorrelateAndBegin {
                new_episode_id: stable_id("episode", &owner, &request),
                event_id: stable_id("request-start", &owner, &request),
                request_id: request,
                correlation_key_id: self.canonicalizer.key_id().into(),
                native_parent_id: None,
                native_parent_digest: None,
                full_input_digest: canonical.full_input_digest,
                ancestor_prefix_digests: canonical.ancestor_prefix_digests,
                ancestor_prefixes_truncated: canonical.ancestor_prefixes_truncated,
                starts_with_prior_turns: canonical.starts_with_prior_turns,
                canonical_input_bytes: canonical.canonical_input_bytes,
                protocol: protocol_name(&protocol).into(),
                captured_at: captured_at.into(),
                guarded_route: Some(guarded),
            });
            let deadline =
                Instant::now() + self.execution.remaining().min(super::super::eval::WORK);
            let permit = tokio::time::timeout_at(
                deadline,
                self.execution.trajectory_sender().reserve_owned(),
            )
            .await
            .map_err(|_| rejection(&self.execution, ErrorCode::CapacityExceeded))?
            .map_err(|_| error())?;
            let (reply, receive) = oneshot::channel();
            self.execution
                .send_trajectory(
                    permit,
                    super::super::actor::Job::TrajectoryBegin(Box::new(Begin {
                        execution: self.execution.clone(),
                        observation: self.observation.clone(),
                        input,
                        evidence,
                        observe: self.observe,
                        reservation,
                        deadline,
                        reply,
                    })),
                    deadline,
                )
                .map_err(|c| rejection(&self.execution, c))?;
            Ok::<_, crate::gateway::error::BitrouterError>((receive, deadline))
        }
        .await;
        let (receive, deadline) = match queued {
            Ok(value) => value,
            Err(error) => {
                self.observation.no_start(Disposition::NoTransaction);
                return Err(error);
            }
        };
        match tokio::time::timeout_at(
            deadline + super::super::transaction::CLEANUP_TIMEOUT,
            receive,
        )
        .await
        {
            Ok(Ok(result)) => result.map_err(|e| rejection(&self.execution, e.0.code)),
            _ => {
                if !self.observation.start_resolved() {
                    self.observation.fail();
                }
                Err(error())
            }
        }
    }
}
