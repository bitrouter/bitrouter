//! Judge overhead is owned by a job/session, separate from coding experiments.
//! Reservations commit with job attempts; missing dispatch or settlement is
//! unknown. Current metering evidence supplies prices, including late receipts.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bitrouter_ai::types::{StreamPart, ToolChoice, UsageOrigin};
use bitrouter_sdk::PipelineEvent;
use bitrouter_sdk::language_model::hooks::{HopOutcome, Phase, RequestOutcome};
use bitrouter_sdk::language_model::{
    HookDecision, ObserveHook, PipelineContext, PreRequestHook, RoutingTarget, SettlementContext,
    SettlementRecorder, StreamContext,
};
use sea_orm::{DatabaseConnection, DatabaseTransaction, TransactionTrait};
use serde::{Deserialize, Serialize};

use super::jobs::JudgeJob;
use super::runtime::{ExecutionHop, ExecutionOutcome};
use super::store::EvolutionStore;
use crate::acp_trajectory::SessionIdentity;
use crate::metering::recorder::MeteringSettlementEvent;

pub mod report;

const KIND: &str = "judge_attempt";
const LOOKUP_KIND: &str = "judge_request_owner";
const VERSION: &str = "checkpoint-judge-attempt-cost-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JudgeAttempt {
    version: String,
    request_id: String,
    job_id: String,
    identity: SessionIdentity,
    checkpoint_id: String,
    model: String,
    reserved_at: String,
    attempt_number: usize,
    started_at: Option<String>,
    settled_at: Option<String>,
    hops: Vec<ExecutionHop>,
    observed_usage_origin: Option<UsageOrigin>,
    outcome: Option<ExecutionOutcome>,
}

#[derive(Debug, Serialize)]
struct JudgeAttemptObserved {
    request_id: String,
    owner: String,
    #[serde(skip)]
    hops: Arc<Mutex<Vec<ExecutionHop>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RequestOwner {
    request_id: String,
    owner: String,
}

struct AttemptSettlement {
    hops: Vec<ExecutionHop>,
    observed_usage_origin: Option<UsageOrigin>,
}

impl PipelineEvent for JudgeAttemptObserved {
    fn event_name(&self) -> &'static str {
        "evolution.judge_attempt_observed"
    }
}

/// Shared with metering: rejecting a duplicate must preserve the first call's
/// charge, even though settlement otherwise runs for rejected requests too.
#[derive(Debug, Serialize)]
pub(crate) struct JudgeAttemptReplayRejected;

impl PipelineEvent for JudgeAttemptReplayRejected {
    fn event_name(&self) -> &'static str {
        "evolution.judge_attempt_replay_rejected"
    }
}

#[derive(Clone)]
pub struct JudgeCosts {
    db: DatabaseConnection,
}

impl JudgeCosts {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    fn lookup(&self) -> Result<EvolutionStore> {
        // One indexed request namespace lets the pipeline recognize a durable
        // reservation without adding ACP headers or caller-controlled purpose.
        // Report access still validates every reservation's job and owner.
        EvolutionStore::new(self.db.clone(), "internal:checkpoint-judge-costs")
    }

    fn store(&self, owner: &str) -> Result<EvolutionStore> {
        EvolutionStore::new(self.db.clone(), owner)
    }

    pub(crate) async fn reserve(
        &self,
        tx: &DatabaseTransaction,
        job: &JudgeJob,
        request_id: &str,
    ) -> Result<()> {
        let fresh = JudgeAttempt {
            version: VERSION.into(),
            request_id: request_id.into(),
            job_id: job.job_id.clone(),
            identity: job.identity.clone(),
            checkpoint_id: job.checkpoint_id.clone(),
            model: job.model.clone(),
            reserved_at: chrono::Utc::now().to_rfc3339(),
            attempt_number: job
                .request_ids
                .len()
                .checked_add(1)
                .context("judge attempt count overflow")?,
            started_at: None,
            settled_at: None,
            hops: vec![],
            observed_usage_origin: None,
            outcome: None,
        };
        let (revision, saved): (_, JudgeAttempt) = self
            .store(&job.identity.owner)?
            .initialize_in(tx, KIND, request_id, Some(job.identity.key()?), &fresh)
            .await?;
        ensure!(
            revision == 0 && serde_json::to_string(&saved)? == serde_json::to_string(&fresh)?,
            "judge request identity was already reserved"
        );
        let binding = RequestOwner {
            owner: job.identity.owner.clone(),
            request_id: request_id.into(),
        };
        let (_, saved): (_, RequestOwner) = self
            .lookup()?
            .initialize_in(tx, LOOKUP_KIND, request_id, None, &binding)
            .await?;
        ensure!(
            saved == binding,
            "judge request is already owned by another scope"
        );
        Ok(())
    }

    async fn admit(&self, ctx: &mut PipelineContext) -> Result<()> {
        if !ctx.request_id().starts_with("brjudge_") {
            return Ok(());
        }
        let Some((_, binding)) = self
            .lookup()?
            .get::<RequestOwner>(LOOKUP_KIND, ctx.request_id())
            .await?
        else {
            return Ok(());
        };
        let store = self.store(&binding.owner)?;
        let Some((_, reserved)) = store.get::<JudgeAttempt>(KIND, ctx.request_id()).await? else {
            ctx.emit(JudgeAttemptReplayRejected);
            anyhow::bail!("reserved judge request metadata is missing");
        };
        // A reservation is for an in-process, tool-free evaluation only.
        let valid_request = binding.request_id == ctx.request_id()
            && reserved.identity.owner == binding.owner
            && reserved.request_id == ctx.request_id()
            && ctx.caller().is_local()
            && ctx.headers().is_empty()
            && ctx.original_model() == reserved.model
            && ctx.generation_prompt().is_some_and(|prompt| {
                prompt.tools.is_empty()
                    && prompt.tool_choice == Some(ToolChoice::None)
                    && !prompt.stream
            });
        if !valid_request {
            ctx.emit(JudgeAttemptReplayRejected);
            anyhow::bail!("judge request does not match its reserved purpose");
        }
        let tx = self.db.begin().await?;
        let (row, mut attempt): (_, JudgeAttempt) = store.lock(&tx, KIND, ctx.request_id()).await?;
        if attempt.started_at.is_some() {
            ctx.emit(JudgeAttemptReplayRejected);
            anyhow::bail!("judge request was already dispatched or interrupted");
        }
        ensure!(attempt.version == VERSION, "unknown judge cost contract");
        attempt.started_at = Some(chrono::Utc::now().to_rfc3339());
        store.save(&tx, row, &attempt).await?;
        tx.commit().await?;
        ctx.emit(JudgeAttemptObserved {
            request_id: attempt.request_id,
            owner: binding.owner,
            hops: Arc::new(Mutex::new(Vec::new())),
        });
        Ok(())
    }

    async fn finish(
        &self,
        observed: &JudgeAttemptObserved,
        settlement: Option<AttemptSettlement>,
        outcome: Option<ExecutionOutcome>,
    ) -> Result<()> {
        let store = self.store(&observed.owner)?;
        let tx = self.db.begin().await?;
        let (row, mut attempt): (_, JudgeAttempt) =
            store.lock(&tx, KIND, &observed.request_id).await?;
        ensure!(
            attempt.started_at.is_some(),
            "judge dispatch was not recorded"
        );
        if let Some(settlement) = settlement {
            ensure!(
                attempt.settled_at.is_none(),
                "judge attempt already settled"
            );
            attempt.hops = settlement.hops;
            attempt.observed_usage_origin = settlement.observed_usage_origin;
            attempt.settled_at = Some(chrono::Utc::now().to_rfc3339());
        }
        if let Some(outcome) = outcome {
            ensure!(attempt.outcome.is_none(), "judge attempt already ended");
            attempt.outcome = Some(outcome);
        }
        store.save(&tx, row, &attempt).await?;
        tx.commit().await?;
        Ok(())
    }
}

fn internal(error: anyhow::Error) -> bitrouter_sdk::BitrouterError {
    bitrouter_sdk::BitrouterError::internal(error.to_string())
}

fn hops(observed: &JudgeAttemptObserved) -> std::sync::MutexGuard<'_, Vec<ExecutionHop>> {
    match observed.hops.lock() {
        Ok(hops) => hops,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[async_trait]
impl PreRequestHook for JudgeCosts {
    async fn revalidate_context(
        &self,
        ctx: &PipelineContext,
    ) -> bitrouter_sdk::Result<HookDecision> {
        if ctx.request_id().starts_with("brjudge_")
            || ctx.get_event::<JudgeAttemptObserved>().is_some()
        {
            return Err(bitrouter_sdk::BitrouterError::bad_request(
                "reserved judge input cannot be reconstructed",
            ));
        }
        Ok(HookDecision::Allow)
    }

    async fn check(&self, ctx: &mut PipelineContext) -> bitrouter_sdk::Result<HookDecision> {
        self.admit(ctx).await.map_err(internal)?;
        Ok(HookDecision::Allow)
    }
}

#[cfg(test)]
mod native_admission_tests {
    use super::*;
    use crate::metering::pricing::PricingTable;
    use crate::metering::recorder::MeteringRecorder;
    use crate::metering::store::{MeteringStore, TimeWindow};
    use bitrouter_ai::protocol::decisions::DecisionsCodec;
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::builder::PipelineBuilder;
    use bitrouter_sdk::language_model::executor::MockExecutor;
    use bitrouter_sdk::language_model::operations::{HookStage, OperationScope};
    use bitrouter_sdk::language_model::routing::StaticRoutingTable;
    use bitrouter_sdk::language_model::types::PipelineRequest;

    #[tokio::test]
    async fn native_reserved_identity_rejection_keeps_metering_namespace_untouched() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let costs = JudgeCosts::new(db.clone());
        let request_id = "brjudge_reserved_native";
        let binding = RequestOwner {
            request_id: request_id.into(),
            owner: "reserved-owner".into(),
        };
        let tx = db.begin().await?;
        costs
            .lookup()?
            .initialize_in(&tx, LOOKUP_KIND, request_id, None, &binding)
            .await?;
        tx.commit().await?;
        let metering = MeteringStore::new(db);
        let mut builder = PipelineBuilder::new();
        builder
            .routing_table(Arc::new(StaticRoutingTable::new()))
            .executor(Arc::new(MockExecutor::new(Vec::new())))
            .served_operations(OperationScope::Both)
            .require_hook::<JudgeCosts>(HookStage::PreResolution, OperationScope::Both)
            .pre_resolution_hook_for(costs.clone(), OperationScope::Both)
            .settlement_recorder_for(
                MeteringRecorder::new(metering.clone(), Arc::new(PricingTable::new())),
                OperationScope::Both,
            );
        let pipeline = builder.build()?;
        let request = DecisionsCodec::parse_request(serde_json::json!({
            "model":"test","input":"evidence","questions":[{"type":"predicate","name":"q","instructions":"check"}]
        }))?;
        let mut invocation =
            PipelineRequest::new_decisions("test", CallerContext::local(), request);
        invocation.request_id = request_id.into();
        let error = pipeline
            .execute(invocation)
            .await
            .err()
            .context("reserved native call was admitted")?;
        assert!(
            error
                .to_string()
                .contains("reserved judge request metadata is missing")
        );
        assert_eq!(
            metering
                .get_request_count("local", TimeWindow::ThisMonth)
                .await?,
            0
        );
        let (_, saved) = costs
            .lookup()?
            .get::<RequestOwner>(LOOKUP_KIND, request_id)
            .await?
            .context("reservation lost")?;
        assert_eq!(saved, binding);
        Ok(())
    }
}

#[async_trait]
impl ObserveHook for JudgeCosts {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}
    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}

    async fn on_hop_start(&self, ctx: &PipelineContext, target: &RoutingTarget) {
        if let Some(observed) = ctx.get_event::<JudgeAttemptObserved>() {
            hops(observed).push(ExecutionHop {
                provider: target.provider_name.clone(),
                model: target.service_id.clone(),
                account: target.account_label.clone(),
                protocol: target.api_protocol.as_str().into(),
                status: "attempting".into(),
                error_code: None,
            });
        }
    }

    async fn on_hop_end(
        &self,
        ctx: &PipelineContext,
        _target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        if let Some(observed) = ctx.get_event::<JudgeAttemptObserved>()
            && let Some(hop) = hops(observed).last_mut()
        {
            let (status, error) = match outcome {
                HopOutcome::Generated(_) => ("completed", None),
                HopOutcome::StreamStarted => ("stream_started", None),
                HopOutcome::Failed(error) => ("failed", Some(error.error_code().into())),
            };
            hop.status = status.into();
            hop.error_code = error;
        }
    }

    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        if let Some(observed) = ctx.get_event::<JudgeAttemptObserved>() {
            let outcome = match outcome {
                RequestOutcome::Completed => ExecutionOutcome::Completed,
                RequestOutcome::Failed(_) => ExecutionOutcome::Failed,
                RequestOutcome::ClientDisconnected => ExecutionOutcome::ClientDisconnected,
            };
            if let Err(error) = self.finish(observed, None, Some(outcome)).await {
                tracing::warn!(%error, "judge cost terminal was not persisted");
            }
        }
    }
}

#[async_trait]
impl SettlementRecorder for JudgeCosts {
    async fn record(&self, ctx: &mut SettlementContext) -> bitrouter_sdk::Result<()> {
        let Some(observed) = ctx.get_event::<JudgeAttemptObserved>() else {
            return Ok(());
        };
        let settlement = AttemptSettlement {
            hops: hops(observed).clone(),
            observed_usage_origin: ctx
                .get_event::<MeteringSettlementEvent>()
                .filter(|event| event.request_id == ctx.request_id)
                .map(|event| event.usage_origin),
        };
        self.finish(observed, Some(settlement), None)
            .await
            .map_err(internal)
    }
}

#[cfg(test)]
mod tests;
