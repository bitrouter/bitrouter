use super::super::preparation;
use super::*;
use crate::{
    gateway::{
        error::BitrouterError,
        observability::event::EventBus,
        pipeline::settlement::{SettlementContext, SettlementRecorder},
    },
    storage::operation_budget::OperationBudget,
    tenancy::{
        eval::observation::{RequiredObservation, TrajectoryStart},
        execution::{TenantExecution, lifecycle::Registration},
        trajectory::{Adapter, Recorder, StartProof},
        transaction::Disposition,
    },
    trajectory::{
        store::{TrajectoryStore, owned::io::Budget},
        types::{RequestStatus, Settlement, TrajectoryEvidence},
    },
};
use bitrouter_ai::types::{ModelOperation, UsageOrigin};
use std::collections::BTreeMap;

const STARTED: &str = "2026-10-25T00:00:00Z";
const FINISHED: &str = "2026-10-25T00:00:00.005Z";

pub(super) struct Harness {
    pub(super) fixture: Fixture,
    pub(super) execution: Arc<TenantExecution>,
    pub(super) observation: Arc<RequiredObservation>,
    pub(super) proof: Arc<StartProof>,
    pub(super) request: String,
    registration: Registration,
}

impl Harness {
    pub(super) async fn begin(database: &mut database::Fixture) -> Result<Self> {
        let db = database.connect().await?;
        let fixture = Fixture::with_database(
            db,
            crate::tenancy::actor::OperationLimits::default(),
            &mut |service| database.retain(service),
        )
        .await?;
        let (fixture, revision) = policy_fixture_with_existing(
            fixture,
            "https://example.invalid/v1",
            LearningMode::Observe,
            ("selected", document()),
            true,
            Some(json!({"enabled":true})),
            &mut |service| database.retain(service),
        )
        .await?;
        let headers = credential(&fixture, &revision).await?;
        let execution = admitted(&fixture, &headers).await?;
        let pipeline = execution.pipeline()?;
        let observation = execution
            .trajectory_observation()
            .context("missing retained trajectory observation")?
            .clone();
        let registration = fixture.service.pipelines.register_observed(
            execution.id(),
            pipeline.clone(),
            observation.clone(),
        )?;
        observation.entered()?;
        let context = preparation::context(&execution, &pipeline).await?;
        drop(pipeline);
        let adapter = Adapter::new(&execution, observation.clone())?;
        // Production adapter canonicalization, queue admission, actor BEGIN and
        // acknowledged COMMIT create the proof. No Begin::committed call here.
        let (correlated, _) = adapter
            .begin(
                &context,
                ApiProtocol::ChatCompletions,
                STARTED,
                preparation::guarded()?,
            )
            .await?;
        ensure!(fixture.service.cleanup.disposition() == Disposition::Committed);
        let TrajectoryStart::Committed(proof) = observation.trajectory_start()? else {
            anyhow::bail!("actor acknowledged without its committed start proof")
        };
        ensure!(proof.matches(&execution));
        ensure!(proof.request() == correlated.request_id);
        ensure!(!observation.confirmed());
        Ok(Self {
            fixture,
            execution,
            observation,
            proof,
            request: correlated.request_id,
            registration,
        })
    }

    /// The real recorder queues the real actor's compound terminal transaction.
    pub(super) async fn record(&self) -> Result<()> {
        Recorder::new(
            &self.execution,
            self.observation.clone(),
            Default::default(),
        )
        .record(&mut terminal_context(&self.execution))
        .await?;
        ensure!(self.observation.confirmed());
        Ok(())
    }

    /// Borrow the same production transaction participants as the actor so fault
    /// errors and raw rows can be inspected before the test owns rollback. The
    /// actor/recorder path is separately exercised in every successful/fault case.
    pub(super) async fn terminal_in(&self, tx: &DatabaseTransaction) -> Result<Settlement> {
        let store = TrajectoryStore::for_execution(&self.execution);
        let check = || Ok(());
        let shared = OperationBudget::new(&check);
        let mut budget = Budget::owned(&shared);
        let request = store
            .request_in(tx, &self.request, &mut budget)
            .await?
            .context("committed request missing")?;
        let existing = request.status != RequestStatus::Started;
        let owner = store.owner()?;
        let settlement = store
            .settle_current_in(
                tx,
                &self.request,
                |request, events, sequence| {
                    crate::trajectory::settlement::build_from_evidence(
                        &owner,
                        request,
                        events,
                        sequence,
                        terminal_evidence(),
                        FINISHED.into(),
                        RequestStatus::Failed,
                    )
                },
                &mut budget,
            )
            .await?;
        let outbox = settlement.outbox.as_ref().context("required Eval outbox")?;
        if existing {
            store.check_delivered_in(tx, outbox, &mut budget).await?;
            self.execution
                .eval_store()
                .check_trajectory_in(tx, &self.proof, &settlement, &shared)
                .await?;
        } else {
            let changed = self
                .execution
                .eval_store()
                .publish_trajectory_in(tx, &self.proof, &settlement, &shared)
                .await?;
            ensure!(
                changed,
                "initial compound publication did not create Eval closure"
            );
            store.mark_delivered_in(tx, outbox, &mut budget).await?;
        }
        Ok(settlement)
    }

    pub(super) async fn finish(self, expected_failure: bool) -> Result<()> {
        let Self {
            fixture,
            execution,
            observation,
            proof,
            registration,
            ..
        } = self;
        drop((registration, execution, observation, proof));
        let drained = fixture.service.pipelines.close_and_drain().await;
        if expected_failure {
            ensure!(drained.is_err(), "failed observation falsely drained");
            ensure!(
                fixture.service.current().is_err(),
                "failed observation did not fence"
            );
            ensure!(
                fixture.service.shutdown().await.is_err(),
                "failed custody falsely shut down"
            );
        } else {
            drained?;
            ensure!(fixture.service.current().is_ok());
            fixture.service.shutdown().await?;
        }
        Ok(())
    }
}

fn terminal_evidence() -> TrajectoryEvidence {
    TrajectoryEvidence {
        structural: BTreeMap::from([("settlement.duration_ms".into(), 5)]),
        categorical: BTreeMap::from([
            (
                "settlement.usage_origin".into(),
                UsageOrigin::Unknown.as_str().into(),
            ),
            ("settlement.outcome".into(), "failed".into()),
            (
                "settlement.error_code".into(),
                BitrouterError::bad_request("fixture stopped")
                    .error_code()
                    .into(),
            ),
        ]),
        digests: Default::default(),
        route_measurement: None,
        routing_evidence: Vec::new(),
    }
}

fn terminal_context(execution: &TenantExecution) -> SettlementContext {
    SettlementContext {
        operation: ModelOperation::Generation,
        request_id: execution.id().to_string(),
        caller: execution.caller(),
        target: None,
        model_id: "model-a".into(),
        reasoning_effort: None,
        provider_id: "mock".into(),
        account_label: None,
        prompt_tokens: 0,
        completion_tokens: 0,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        usage_origin: UsageOrigin::Unknown,
        raw_usage: None,
        web_search_count: 0,
        media_input_count: 0,
        media_output_count: 0,
        server_tool_calls: Vec::new(),
        streamed: false,
        request_duration_ms: 5,
        upstream_duration_ms: None,
        ttft_ms: None,
        generation_duration_ms: None,
        first_token_kind: None,
        finish_reason: None,
        error: Some(BitrouterError::bad_request("fixture stopped")),
        events: EventBus::default(),
    }
}
