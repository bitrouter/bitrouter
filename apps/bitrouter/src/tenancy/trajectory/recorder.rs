use super::*;
use crate::{
    gateway::{
        observability::decision::{CapturedEvalDecision, EvalInvocation},
        pipeline::settlement::{SettlementContext, SettlementRecorder},
    },
    trajectory::types::{RequestStatus, StoredRequest, TrajectoryEvent, TrajectoryEvidence},
};
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, TimeDelta};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize)]
pub(in crate::tenancy) struct Facts {
    pub(super) evidence: TrajectoryEvidence,
    pub(super) status: RequestStatus,
    duration_ms: u64,
}
impl Facts {
    fn capture(
        execution: &TenantExecution,
        context: &SettlementContext,
        pending: &crate::gateway::observability::decision::PendingEvalDecisionStore,
    ) -> Result<Self> {
        let caller = execution.caller();
        ensure!(
            context.request_id == execution.id().as_str()
                && context.caller.user_id() == caller.user_id()
                && context.caller.api_key_id() == caller.api_key_id()
                && !context.caller.is_local()
                && context.caller.launch_id().is_none(),
            "trajectory settlement caller mismatch"
        );
        let mut structural =
            BTreeMap::from([("settlement.duration_ms".into(), context.request_duration_ms)]);
        if context.usage_origin != bitrouter_ai::types::UsageOrigin::Unknown {
            for (key, value) in [
                ("prompt_tokens", context.prompt_tokens),
                ("completion_tokens", context.completion_tokens),
                ("reasoning_tokens", context.reasoning_tokens),
                ("cache_read_tokens", context.cache_read_tokens),
                ("cache_write_tokens", context.cache_write_tokens),
            ] {
                structural.insert(format!("settlement.{key}"), value);
            }
        }
        let mut categorical = BTreeMap::from([
            (
                "settlement.usage_origin".into(),
                context.usage_origin.as_str().into(),
            ),
            (
                "settlement.outcome".into(),
                if context.error.is_some() {
                    "failed"
                } else {
                    "settled"
                }
                .into(),
            ),
        ]);
        if let Some(error) = &context.error {
            categorical.insert("settlement.error_code".into(), error.error_code().into());
        }
        if let Some(invocation) = context.get_event::<EvalInvocation>() {
            let decision = match context.get_events::<CapturedEvalDecision>().last() {
                Some(snapshot) => snapshot.decision_for(invocation, caller.user_id()).cloned(),
                None => pending.peek(invocation, caller.user_id()),
            };
            if let Some(decision) = decision {
                decision
                    .observation_snapshot()
                    .write_namespaced(&mut structural, &mut categorical);
            }
        }
        // No ambient metering event, price table, raw usage, provider body or
        // externally supplied error/finish text becomes retained evidence.
        let facts = Self {
            evidence: TrajectoryEvidence {
                structural,
                categorical,
                routing_evidence: crate::gateway::observability::decision::routing_evidence(
                    context,
                )?,
                digests: Default::default(),
                route_measurement: None,
            },
            status: if context.error.is_some() {
                RequestStatus::Failed
            } else {
                RequestStatus::Settled
            },
            duration_ms: context.request_duration_ms,
        };
        crate::trajectory::store::owned::io::json_size(&facts, 1024 * 1024)
            .map_err(|_| anyhow::anyhow!("trajectory terminal facts exceed bound"))?;
        Ok(facts)
    }
    pub(super) fn timestamp(
        &self,
        request: &StoredRequest,
        events: &[TrajectoryEvent],
    ) -> Result<String> {
        let start = events
            .iter()
            .find(|e| e.event_id == request.start_event_id)
            .context("trajectory start missing")?;
        let start = DateTime::parse_from_rfc3339(&start.captured_at)?;
        let measured = start
            .checked_add_signed(TimeDelta::milliseconds(i64::try_from(self.duration_ms)?))
            .context("trajectory duration overflow")?;
        let head = events
            .last()
            .map(|e| DateTime::parse_from_rfc3339(&e.captured_at))
            .transpose()?
            .unwrap_or(start);
        Ok(measured
            .max(head)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}
impl Recorder {
    pub(in crate::tenancy) fn new(
        execution: &Arc<TenantExecution>,
        observation: Arc<RequiredObservation>,
        pending: crate::gateway::observability::decision::PendingEvalDecisionStore,
    ) -> Self {
        Self {
            execution: execution.clone(),
            observation,
            pending,
        }
    }
}
#[async_trait]
impl SettlementRecorder for Recorder {
    async fn record(&self, context: &mut SettlementContext) -> crate::gateway::error::Result<()> {
        let guard = self.observation.begin()?;
        let caller = self.execution.caller();
        if context.request_id != self.execution.id().as_str()
            || context.caller.user_id() != caller.user_id()
            || context.caller.api_key_id() != caller.api_key_id()
            || context.caller.is_local()
            || context.caller.launch_id().is_some()
        {
            return Err(error());
        }
        let start = match self.observation.trajectory_start()? {
            super::super::eval::observation::TrajectoryStart::Committed(start) => start,
            super::super::eval::observation::TrajectoryStart::NoStart => {
                if context.error.is_none() || context.has_event::<CapturedEvalDecision>() {
                    return Err(error());
                }
                return guard.skip_no_start();
            }
            super::super::eval::observation::TrajectoryStart::Absent => {
                if context.has_event::<EvalInvocation>()
                    || context.has_event::<CapturedEvalDecision>()
                {
                    return Err(error());
                }
                return guard.skip();
            }
        };
        let facts =
            Arc::new(Facts::capture(&self.execution, context, &self.pending).map_err(|_| error())?);
        guard.prepare_trajectory(&start, &facts)?;
        let deadline = Instant::now() + super::super::eval::WORK;
        let permit =
            tokio::time::timeout_at(deadline, self.execution.trajectory_sender().reserve_owned())
                .await
                .map_err(|_| error())?
                .map_err(|_| error())?;
        let (reply, receive) = oneshot::channel();
        self.observation.queued()?;
        self.execution
            .send_trajectory(
                permit,
                super::super::actor::Job::TrajectoryTerminal(Box::new(Terminal {
                    execution: self.execution.clone(),
                    observation: self.observation.clone(),
                    start,
                    facts,
                    deadline,
                    reply,
                })),
                deadline,
            )
            .map_err(|_| error())?;
        match tokio::time::timeout_at(
            deadline + super::super::transaction::CLEANUP_TIMEOUT,
            receive,
        )
        .await
        {
            Ok(Ok(Ok(()))) => {}
            _ if self.observation.confirmed() => {}
            _ => return Err(error()),
        }
        guard.finish()
    }
}
