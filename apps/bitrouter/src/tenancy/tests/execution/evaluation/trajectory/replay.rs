//! Real actor publication followed by read-only validation of damaged closure.
use super::*;
use crate::{
    core::eval::store::OwnedError,
    gateway::{
        error::BitrouterError,
        observability::event::EventBus,
        pipeline::settlement::{SettlementContext, SettlementRecorder},
    },
    storage::operation_budget::OperationBudget,
    tenancy::{
        eval::observation::TrajectoryStart,
        execution::TenantExecution,
        trajectory::{Adapter, Recorder, StartProof},
        transaction::Disposition,
    },
    trajectory::{
        store::{TrajectoryStore, owned::io::Budget},
        types::{RequestStatus, TrajectoryEvidence},
    },
};
use bitrouter_ai::types::{ModelOperation, UsageOrigin};
use sea_orm::{DatabaseTransaction, TransactionTrait};
use std::collections::BTreeMap;

#[tokio::test]
async fn terminal_replay_checks_exact_eval_closure_without_any_write() -> Result<()> {
    tokio::time::timeout(TIMEOUT, exercise()).await?
}

async fn exercise() -> Result<()> {
    let (fixture, revision) = fixture("https://example.invalid/v1", LearningMode::Observe).await?;
    let headers = credential(&fixture, &revision).await?;
    let execution = admitted(&fixture, &headers).await?;
    let pipeline = execution.pipeline()?;
    let observation = execution
        .trajectory_observation()
        .context("observation")?
        .clone();
    let registration = fixture.service.pipelines.register_observed(
        execution.id(),
        pipeline.clone(),
        observation.clone(),
    )?;
    observation.entered()?;
    let context = preparation::context(&execution, &pipeline).await?;
    let adapter = Adapter::new(&execution, observation.clone())?;
    let (correlated, _) = adapter
        .begin(
            &context,
            ApiProtocol::ChatCompletions,
            "2026-10-25T00:00:00Z",
            preparation::guarded()?,
        )
        .await?;
    ensure!(fixture.service.cleanup.disposition() == Disposition::Committed);
    let TrajectoryStart::Committed(proof) = observation.trajectory_start()? else {
        anyhow::bail!("actor did not acknowledge committed start");
    };
    ensure!(proof.matches(&execution) && proof.request() == correlated.request_id);
    drop((context, adapter, pipeline));
    Recorder::new(&execution, observation.clone(), Default::default())
        .record(&mut terminal_context(&execution))
        .await?;
    ensure!(observation.confirmed());
    ensure!(fixture.service.cleanup.disposition() == Disposition::Committed);
    // The first publication took the real actor's insertion path.
    for table in [
        "eval_subjects",
        "eval_results",
        "eval_admission_events",
        "trajectory_outbox",
    ] {
        ensure!(count(&fixture.db, table, "1=1").await? == 1);
    }
    let outcome = inspect(&fixture, &execution, &proof).await;
    drop((proof, registration, execution, observation));
    let drained = fixture.service.pipelines.close_and_drain().await;
    let shutdown = fixture.service.shutdown().await;
    outcome?;
    drained?;
    shutdown?;
    Ok(())
}

async fn inspect(fixture: &Fixture, execution: &TenantExecution, proof: &StartProof) -> Result<()> {
    // Each damaged snapshot is inspected in the same transaction before its
    // explicit rollback. total_changes counts writes even when later rolled back.
    // https://www.sqlite.org/lang_corefunc.html#total_changes
    let damages: &[(&str, &[&str])] = &[
        ("intact", &[]),
        ("missing subject", &["DELETE FROM eval_subjects"]),
        ("missing result", &["DELETE FROM eval_results"]),
        ("missing admission", &["DELETE FROM eval_admission_events"]),
        (
            "missing entire Eval closure",
            &[
                "DELETE FROM eval_admission_events",
                "DELETE FROM eval_results",
                "DELETE FROM eval_subjects",
            ],
        ),
        (
            "malformed subject",
            &["UPDATE eval_subjects SET subject_json='{}'"],
        ),
        (
            "malformed result",
            &["UPDATE eval_results SET result_json='{}'"],
        ),
        (
            "subject creator",
            &["UPDATE eval_subjects SET creator_principal_id='other-principal'"],
        ),
        (
            "result creator",
            &["UPDATE eval_results SET creator_principal_id='other-principal'"],
        ),
        (
            "admission creator",
            &["UPDATE eval_admission_events SET creator_principal_id='other-principal'"],
        ),
        (
            "subject namespace",
            &["UPDATE eval_subjects SET tenant_id='other-tenant'"],
        ),
        (
            "result namespace",
            &["UPDATE eval_results SET tenant_id='other-tenant'"],
        ),
        (
            "admission namespace",
            &["UPDATE eval_admission_events SET tenant_id='other-tenant'"],
        ),
        (
            "admission authority",
            &["UPDATE eval_admission_events SET authority_id='other-authority'"],
        ),
        (
            "admission status",
            &["UPDATE eval_admission_events SET status='rejected'"],
        ),
        (
            "admission reason",
            &["UPDATE eval_admission_events SET reason='other-reason'"],
        ),
        (
            "extra admission",
            &[
                "INSERT INTO eval_admission_events (installation_id,tenant_id,creator_principal_id,result_id,status,reason,authority_id,created_at) SELECT installation_id,tenant_id,creator_principal_id,result_id,status,reason,authority_id,created_at FROM eval_admission_events",
            ],
        ),
        ("intact after all rollbacks", &[]),
    ];
    for (name, statements) in damages {
        let tx = fixture.db.begin().await?;
        let inspected = async {
            for statement in *statements {
                tx.execute_unprepared(statement).await?;
            }
            let before = changes(&tx).await?;
            let result = replay_in(execution, proof, &tx).await;
            ensure!(
                changes(&tx).await? == before,
                "replay wrote inside transaction: {name}"
            );
            if statements.is_empty() {
                result?;
                replay_in(execution, proof, &tx).await?;
                ensure!(changes(&tx).await? == before, "second exact replay wrote");
            } else {
                let error = result.err().context(format!("replay accepted {name}"))?;
                ensure!(
                    matches!(
                        error.downcast_ref::<OwnedError>(),
                        Some(OwnedError::Integrity(_))
                    ),
                    "wrong rejection for {name}: {error:#}"
                );
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        tx.rollback().await?;
        inspected?;
    }
    fixture.service.current().map_err(failure)?;
    Ok(())
}

async fn replay_in(
    execution: &TenantExecution,
    proof: &StartProof,
    tx: &DatabaseTransaction,
) -> Result<()> {
    let store = TrajectoryStore::for_execution(execution);
    let check = || Ok(());
    let shared = OperationBudget::new(&check);
    let mut budget = Budget::owned(&shared);
    ensure!(
        store
            .request_in(tx, proof.request(), &mut budget)
            .await?
            .context("request")?
            .status
            != RequestStatus::Started
    );
    let owner = store.owner()?;
    let settlement = store
        .settle_current_in(
            tx,
            proof.request(),
            |request, events, sequence| {
                crate::trajectory::settlement::build_from_evidence(
                    &owner,
                    request,
                    events,
                    sequence,
                    terminal_evidence(),
                    "2026-10-25T00:00:00.005Z".into(),
                    RequestStatus::Failed,
                )
            },
            &mut budget,
        )
        .await?;
    let outbox = settlement.outbox.as_ref().context("outbox")?;
    store.check_delivered_in(tx, outbox, &mut budget).await?;
    execution
        .eval_store()
        .check_trajectory_in(tx, proof, &settlement, &shared)
        .await?;
    Ok(())
}

async fn changes(tx: &DatabaseTransaction) -> Result<i64> {
    let row = tx
        .query_one(Statement::from_string(
            tx.get_database_backend(),
            "SELECT total_changes() AS writes".to_owned(),
        ))
        .await?
        .context("SQLite changes")?;
    Ok(row.try_get("", "writes")?)
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
