//! Real PostgreSQL rollback and replay campaigns for the two projection phases.

use super::*;
use crate::gateway::shared::{
    authority::Authority, crypto::DeploymentKey, schema, test_database::EmptyDatabase,
};
use crate::storage::evaluation::shared::SharedEvaluation;
use crate::trajectory::types::{
    BeginRequest, EpisodeStart, HistoryCompleteness, KeyedDigest, TRAJECTORY_SCHEMA_VERSION,
    TrajectoryEvent, TrajectoryEventKind, TrajectoryEvidence,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::{collections::BTreeMap, time::Duration};

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege"]
async fn postgres_terminal_projection() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(60), exercise(&mut database))
        .await
        .context("projection campaign deadline exceeded")
        .and_then(|result| result);
    database.finish(result).await
}

async fn exercise(database: &mut EmptyDatabase) -> Result<()> {
    let first_db = database.connect(4, Duration::from_secs(3)).await?;
    let second_db = database.connect(4, Duration::from_secs(3)).await?;
    let namespace = format!("projection-{}", uuid::Uuid::new_v4());
    let first = Store::new(first_db.clone(), &namespace, Duration::from_secs(3))?;
    let second = Store::new(second_db.clone(), &namespace, Duration::from_secs(3))?;
    async {
        schema::initialize_all(&first).await?;
        let deployment_key = rand::random();
        Authority::new(first.clone(), DeploymentKey::from_bytes(deployment_key)?).initialize().await?;
        let a = engine(first.clone(), deployment_key).await?;
        let b = engine(second.clone(), deployment_key).await?;
        lease_recovery(&a, &b).await?;
        for guarded in [false, true] {
            let terminal = seed(&a, guarded).await?;
            let tx = first.begin().await?;
            a.authority.fence(&tx).await?;
            let mut fence = match &terminal.binding { Some(binding) => Some(a.episode_fence(&tx, binding, Store::now(&tx).await?).await?), None => None };
            a.project_in(&tx, &terminal, fence.as_mut()).await?;
            tx.rollback().await?;
            assert_phase(&a, &terminal, false, false).await?;
            let (left, right) = tokio::join!(a.deliver(&terminal.record.event), b.deliver(&terminal.record.event));
            left?; right?;
            assert_phase(&b, &terminal, true, true).await?;
            let original: Stored<Receipt> = first.get(Kind::ProjectionReceipt, &terminal.record.event.event_id).await?.context("missing delivered receipt")?;
            b.deliver(&terminal.record.event).await?;
            let replay: Stored<Receipt> = second.get(Kind::ProjectionReceipt, &terminal.record.event.event_id).await?.context("missing replay receipt")?;
            ensure!(original.revision()? == replay.revision()? && original.value.projection == replay.value.projection);
            if guarded {
                admission_provenance(&b, &terminal, &original).await?;
                receipt_phase_corruption(&b, &terminal, &original).await?;
            }
            let mut forged = terminal.record.event.clone(); forged.tenant_id = "foreign".into();
            ensure!(b.deliver(&forged).await.is_err());
            // An existing receipt cannot recreate a disappeared domain row.
            let tx = first.begin().await?;
            tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "DELETE FROM gateway_shared_metering_requests WHERE namespace=$1 AND execution_id=$2",
                [namespace.clone().into(), terminal.record.event.execution.execution_id.clone().into()])).await?;
            tx.commit().await?;
            ensure!(b.deliver(&terminal.record.event).await.is_err());
            let tx = first.begin().await?;
            ensure!(SharedMetering::new(&namespace)?.get_in(&tx, &terminal.record.event.tenant_id, &terminal.record.principal_id, &terminal.record.event.execution.execution_id).await?.is_none());
            tx.commit().await?;
        }
        failure_boundaries(&a).await?;
        Ok::<(), anyhow::Error>(())
    }.await
}

async fn engine(store: Store, deployment_key: [u8; 32]) -> Result<Engine> {
    let authority = Authority::new(store, DeploymentKey::from_bytes(deployment_key)?);
    Engine::join(authority, 30_000).await
}

async fn lease_recovery(first: &Engine, second: &Engine) -> Result<()> {
    let terminal = seed(first, true).await?;
    let event = &terminal.record.event;
    let claim_a = first
        .claim_outbox(1, 100)
        .await?
        .pop()
        .context("first claim missing")?;
    ensure!(claim_a.event == *event);
    let tx = first.store().begin().await?;
    first.authority.fence(&tx).await?;
    let mut fence = first
        .episode_fence(
            &tx,
            terminal.binding.as_ref().context("missing binding")?,
            Store::now(&tx).await?,
        )
        .await?;
    first.project_in(&tx, &terminal, Some(&mut fence)).await?;
    tx.commit().await?;
    assert_phase(first, &terminal, true, false).await?;

    // Observe database expiry, not a fixed sleep assumption. A different
    // incarnation recovers a committed Projected receipt without re-finalizing.
    wait_for_claim_expiry(first, &event.event_id).await?;
    let claim_b = second
        .claim_outbox(1, 100)
        .await?
        .pop()
        .context("replacement claim missing")?;
    ensure!(claim_b.event == *event);
    ensure!(first.acknowledge_outbox(&claim_a).await.is_err());
    ensure!(first.retry_outbox(&claim_a, 100).await.is_err());
    second.deliver(&claim_b.event).await?;
    assert_phase(second, &terminal, true, true).await?;
    let delivered: Stored<Receipt> = first
        .store()
        .get(Kind::ProjectionReceipt, &event.event_id)
        .await?
        .context("missing Delivered receipt")?;

    // Lose ACK after Delivered, replace the lease again, then verify exact
    // completion replay and idempotent ACK by only the current claimant.
    wait_for_claim_expiry(second, &event.event_id).await?;
    let claim_c = first
        .claim_outbox(1, 1000)
        .await?
        .pop()
        .context("Delivered replay claim missing")?;
    ensure!(claim_c.event == *event);
    first.deliver(&claim_c.event).await?;
    let replay: Stored<Receipt> = second
        .store()
        .get(Kind::ProjectionReceipt, &event.event_id)
        .await?
        .context("missing replay receipt")?;
    ensure!(
        replay.revision()? == delivered.revision()?
            && replay.value.completion == delivered.value.completion
    );
    ensure!(second.acknowledge_outbox(&claim_b).await.is_err());
    first.acknowledge_outbox(&claim_c).await?;
    first.acknowledge_outbox(&claim_c).await?;
    let outbox: Stored<super::super::super::Outbox> = second
        .store()
        .get(Kind::Outbox, &event.event_id)
        .await?
        .context("missing acknowledged outbox")?;
    ensure!(outbox.value.acknowledged_at_ms.is_some() && outbox.value.deliveries == 3);
    ensure!(second.claim_outbox(1, 1000).await?.is_empty());
    Ok(())
}

async fn wait_for_claim_expiry(engine: &Engine, id: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let tx = engine.store().begin().await?;
            let outbox: Stored<super::super::super::Outbox> = engine
                .store()
                .read(&tx, Kind::Outbox, id)
                .await?
                .context("claimed outbox missing")?;
            ensure!(outbox.value.acknowledged_at_ms.is_none());
            let expired =
                outbox.due_at_ms().context("outbox lost lease expiry")? <= Store::now(&tx).await?;
            tx.commit().await?;
            if expired {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .context("outbox lease did not expire")?
}

async fn admission_provenance(
    engine: &Engine,
    terminal: &Terminal,
    original: &Stored<Receipt>,
) -> Result<()> {
    let Some(Completion::Trajectory { admission }) = &original.value.completion else {
        anyhow::bail!("fixture lost its original admission");
    };
    let event = &terminal.record.event;
    for (column, changed, retained) in [
        (
            "authority_id",
            "foreign-evaluator",
            admission.authority_id.as_str(),
        ),
        (
            "reason",
            "different-valid-reason",
            admission.reason.as_str(),
        ),
        (
            "created_at",
            "2000-01-01T00:00:00Z",
            admission.created_at.as_str(),
        ),
    ] {
        // The column is selected only from the static fixture list above.
        // https://www.postgresql.org/docs/current/sql-update.html
        let sql = format!(
            "UPDATE gateway_shared_eval_admission_events SET {column}=$1 WHERE namespace=$2 AND result_id=$3 AND sequence=$4"
        );
        let alter = |value: &str| {
            Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                &sql,
                [
                    value.into(),
                    engine.store().namespace().into(),
                    admission.result_id.clone().into(),
                    admission.sequence.into(),
                ],
            )
        };
        ensure!(
            engine
                .store()
                .database()
                .execute(alter(changed))
                .await?
                .rows_affected()
                == 1
        );
        let rejected = engine.deliver(event).await;
        ensure!(
            engine
                .store()
                .database()
                .execute(alter(retained))
                .await?
                .rows_affected()
                == 1
        );
        let expected = if column == "authority_id" {
            "required evaluation lost its trusted admission"
        } else {
            "delivered admission evidence changed"
        };
        ensure!(
            rejected
                .err()
                .is_some_and(|error| error.to_string() == expected),
            "changed admission {column} did not fail its evidence check"
        );

        let tx = engine.store().begin().await?;
        let receipt: Stored<Receipt> = engine
            .store()
            .read(&tx, Kind::ProjectionReceipt, &event.event_id)
            .await?
            .context("receipt disappeared")?;
        ensure!(
            receipt.revision()? == original.revision()?
                && receipt.value.completion == original.value.completion
        );
        let outbox: Stored<super::super::super::Outbox> = engine
            .store()
            .read(&tx, Kind::Outbox, &event.event_id)
            .await?
            .context("terminal outbox disappeared")?;
        ensure!(outbox.value.acknowledged_at_ms.is_none());
        let latest = SharedEvaluation::new(engine.store().namespace())?
            .latest_admission_in(&tx, &event.tenant_id, &admission.result_id)
            .await?;
        ensure!(
            latest.as_ref() == Some(admission),
            "replay replaced the original admission"
        );
        tx.commit().await?;
        engine.deliver(event).await?;
    }
    Ok(())
}

async fn receipt_phase_corruption(
    engine: &Engine,
    terminal: &Terminal,
    original: &Stored<Receipt>,
) -> Result<()> {
    let Evaluation::Trajectory { reference } = &original.value.projection.evaluation else {
        anyhow::bail!("missing trajectory receipt");
    };
    let event = &terminal.record.event;
    let source = serde_json::to_value(&original.value)?;
    let mut missing = source.clone();
    missing
        .as_object_mut()
        .context("receipt must be an object")?
        .remove("completion");
    let mut cleared = source.clone();
    cleared["completion"] = serde_json::Value::Null;
    let original_body = serde_json::to_string(&original.value)?;
    // Read every delivery metadata field so a failed replay cannot silently
    // repeat bookkeeping. JSON object field order is stable within this query.
    // https://www.postgresql.org/docs/current/functions-json.html
    let delivery_query = || {
        Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT to_jsonb(o)::text AS snapshot FROM gateway_shared_trajectory_outbox o WHERE namespace=$1 AND owner_user_id=$2 AND outbox_id=$3",
            [
                engine.store().namespace().into(),
                event.tenant_id.clone().into(),
                reference.outbox_id.clone().into(),
            ],
        )
    };
    let delivery_before: String = engine
        .store()
        .database()
        .query_one(delivery_query())
        .await?
        .context("missing delivered trajectory outbox")?
        .try_get("", "snapshot")?;
    for (body, status) in [
        (serde_json::to_string(&cleared)?, "delivered"),
        (serde_json::to_string(&missing)?, "delivered"),
        (original_body.clone(), "projected"),
        (original_body.clone(), "unknown"),
    ] {
        let update = |body: &str, status: &str| {
            Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE gateway_shared_records SET body=$1,status=$2 WHERE namespace=$3 AND kind='projection_receipt' AND key=$4",
                [
                    body.into(),
                    status.into(),
                    engine.store().namespace().into(),
                    event.event_id.clone().into(),
                ],
            )
        };
        ensure!(
            engine
                .store()
                .database()
                .execute(update(&body, status))
                .await?
                .rows_affected()
                == 1
        );
        let result = engine.deliver(event).await;
        let after = engine.store().database().query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT body,status,revision FROM gateway_shared_records WHERE namespace=$1 AND kind='projection_receipt' AND key=$2",
            [engine.store().namespace().into(), event.event_id.clone().into()],
        )).await?.context("corrupt receipt disappeared")?;
        ensure!(
            engine
                .store()
                .database()
                .execute(update(&original_body, "delivered"))
                .await?
                .rows_affected()
                == 1
        );
        ensure!(result.err().is_some_and(
            |error| error.to_string() == "projection receipt phase differs from completion"
        ));
        ensure!(
            after.try_get::<String>("", "body")? == body
                && after.try_get::<String>("", "status")? == status
        );
        ensure!(u64::try_from(after.try_get::<i64>("", "revision")?)? == original.revision()?);
        let delivery_after: String = engine
            .store()
            .database()
            .query_one(delivery_query())
            .await?
            .context("trajectory outbox disappeared")?
            .try_get("", "snapshot")?;
        ensure!(delivery_after == delivery_before);
        let tx = engine.store().begin().await?;
        let admission = SharedEvaluation::new(engine.store().namespace())?
            .latest_admission_in(&tx, &event.tenant_id, &reference.result_digest)
            .await?
            .context("admission disappeared")?;
        ensure!(original.value.completion == Some(Completion::Trajectory { admission }));
        let outbox: Stored<super::super::super::Outbox> = engine
            .store()
            .read(&tx, Kind::Outbox, &event.event_id)
            .await?
            .context("terminal outbox disappeared")?;
        ensure!(outbox.value.acknowledged_at_ms.is_none());
        tx.commit().await?;
        engine.deliver(event).await?;
    }
    Ok(())
}

async fn seed(engine: &Engine, guarded: bool) -> Result<Terminal> {
    let mut record = crate::metering::shared::tests::fixture()?;
    let id = record.event.execution.execution_id.clone();
    record.event.event_id = format!("gateway-terminal-{id}");
    for (index, (view, facts)) in record
        .event
        .execution
        .attempts
        .iter_mut()
        .zip(&mut record.attempts)
        .enumerate()
    {
        view.attempt_id = format!("{id}-attempt-{index}");
        facts.attempt_id.clone_from(&view.attempt_id);
    }
    let tx = engine.store().begin().await?;
    engine.authority.fence(&tx).await?;
    let (binding, inputs) = if guarded {
        let begin = start(&record)?;
        let binding = TrajectoryBinding {
            owner_user_id: record.event.tenant_id.clone(),
            episode_id: begin.episode.episode_id.clone(),
            request_id: begin.event.request_id.clone().context("missing request")?,
            correlation_epoch: begin.episode.correlation_key_id.clone(),
        };
        let mut fence = engine
            .episode_fence(&tx, &binding, Store::now(&tx).await?)
            .await?;
        fence.value.pending.insert(id.clone());
        engine
            .store()
            .save(&tx, &mut fence, "pending", None, Store::now(&tx).await?)
            .await?;
        let trajectory = SharedTrajectory::new(engine.store().namespace())?;
        trajectory
            .begin_in(&tx, &record.event.tenant_id, &begin)
            .await?;
        let route = crate::trajectory::shared::tests::guarded_batch(&[], &begin.event)?;
        trajectory
            .append_route_in(&tx, &record.event.tenant_id, &route)
            .await?;
        let decision = crate::trajectory::evaluation::decode_eval_decision(&route[0])?
            .context("missing decision")?;
        record.request_id = Some(binding.request_id.clone());
        let inputs =
            crate::gateway::shared::capture::projection::tests::selected(&record, decision, true)?;
        (Some(binding), inputs)
    } else {
        record.request_id = None;
        (
            None,
            crate::gateway::shared::capture::projection::tests::untracked(&record)?,
        )
    };
    let execution = Execution {
        tenant_id: record.event.tenant_id.clone(),
        principal_id: record.principal_id.clone(),
        authorization_revision: "fixture".into(),
        tenant_revision: 1,
        owner: engine.owner.clone(),
        trajectory: binding.clone(),
        projection: Some(Box::new(inputs.clone())),
        view: record.event.execution.clone(),
    };
    ensure!(
        engine
            .store()
            .create(
                &tx,
                NewRecord {
                    kind: Kind::Execution,
                    key: &id,
                    status: "terminal",
                    due_at_ms: None,
                    now_ms: 2000,
                    value: &execution
                }
            )
            .await?
    );
    let mut persisted: Stored<Execution> = engine.store().lock(&tx, Kind::Execution, &id).await?;
    engine
        .save_execution(&tx, &mut persisted, "terminal", None, 2000)
        .await?;
    for (view, facts) in record.event.execution.attempts.iter().zip(&record.attempts) {
        let attempt = super::super::super::Attempt {
            execution_id: id.clone(),
            credential: crate::gateway::shared::credentials::CredentialStamp {
                id: "fixture".into(),
                pool_id: view.resource_pool_id.clone(),
                version: 1,
            },
            resource_revision: 1,
            attribution: facts.attribution.clone(),
            terminal_facts: facts.terminal.clone(),
            view: view.clone(),
        };
        ensure!(
            engine
                .store()
                .create(
                    &tx,
                    NewRecord {
                        kind: Kind::Attempt,
                        key: &view.attempt_id,
                        status: "terminal",
                        due_at_ms: None,
                        now_ms: 2000,
                        value: &attempt
                    }
                )
                .await?
        );
    }
    let outbox = super::super::super::Outbox {
        event: record.event.clone(),
        lease: None,
        deliveries: 0,
        acknowledged_at_ms: None,
    };
    ensure!(
        engine
            .store()
            .create(
                &tx,
                NewRecord {
                    kind: Kind::Outbox,
                    key: &record.event.event_id,
                    status: "pending",
                    due_at_ms: Some(2000),
                    now_ms: 2000,
                    value: &outbox
                }
            )
            .await?
    );
    tx.commit().await?;
    Ok(Terminal {
        record,
        inputs: Box::new(inputs),
        binding,
    })
}

fn start(record: &Record) -> Result<BeginRequest> {
    let id = &record.event.execution.execution_id;
    let digest = KeyedDigest::parse(format!("hmac-sha256:key-1:{}", "1".repeat(64)))?;
    let mut event = TrajectoryEvent {
        schema_version: TRAJECTORY_SCHEMA_VERSION,
        event_id: format!("start-{id}"),
        owner_user_id: record.event.tenant_id.clone(),
        episode_id: format!("episode-{id}"),
        request_id: Some(format!("request-{id}")),
        sequence: 1,
        kind: TrajectoryEventKind::RequestStarted,
        evidence: TrajectoryEvidence {
            structural: BTreeMap::new(),
            categorical: BTreeMap::from([
                ("correlation.source".into(), "explicit_root".into()),
                ("history.completeness".into(), "complete".into()),
            ]),
            digests: BTreeMap::new(),
            route_measurement: None,
            routing_evidence: vec![],
        },
        captured_at: "1970-01-01T00:00:01Z".into(),
        content_digest: String::new(),
    };
    event.content_digest = event.semantic_digest()?;
    Ok(BeginRequest {
        episode: EpisodeStart {
            episode_id: event.episode_id.clone(),
            correlation_digest: digest.clone(),
            correlation_key_id: "key-1".into(),
            correlation_source: "explicit_root".into(),
            completeness: HistoryCompleteness::Complete,
        },
        event,
        full_input_digest: digest,
        native_parent_id: None,
        protocol: "responses".into(),
    })
}

async fn assert_phase(
    engine: &Engine,
    terminal: &Terminal,
    projected: bool,
    delivered: bool,
) -> Result<()> {
    let tx = engine.store().begin().await?;
    let event = &terminal.record.event;
    let receipt: Option<Stored<Receipt>> = engine
        .store()
        .read(&tx, Kind::ProjectionReceipt, &event.event_id)
        .await?;
    ensure!(receipt.is_some() == projected);
    if let Some(receipt) = &receipt {
        ensure!(receipt.value.completion.is_some() == delivered);
    }
    let row = SharedMetering::new(engine.store().namespace())?
        .get_in(
            &tx,
            &event.tenant_id,
            &terminal.record.principal_id,
            &event.execution.execution_id,
        )
        .await?;
    ensure!(row.is_some() == projected);
    if let Some(binding) = &terminal.binding {
        let fence = engine
            .episode_fence(&tx, binding, Store::now(&tx).await?)
            .await?;
        ensure!(fence.value.pending.contains(&event.execution.execution_id) != projected);
        let history = SharedTrajectory::new(engine.store().namespace())?
            .history_in(&tx, &event.tenant_id, &binding.episode_id)
            .await?;
        ensure!(
            history
                .iter()
                .filter(|event| event.kind == TrajectoryEventKind::RequestSettled)
                .count()
                == usize::from(projected)
        );
    }
    if let Some(receipt) = receipt {
        let eval = SharedEvaluation::new(engine.store().namespace())?;
        let subject_id = match &receipt.value.projection.evaluation {
            Evaluation::Request { eval_id, .. } => eval_id,
            Evaluation::Trajectory { reference } => {
                ensure!(
                    eval.result_in(&tx, &event.tenant_id, &reference.result_digest)
                        .await?
                        .is_some()
                        == delivered
                );
                ensure!(
                    eval.latest_admission_in(&tx, &event.tenant_id, &reference.result_digest)
                        .await?
                        .is_some()
                        == delivered
                );
                &reference.subject_id
            }
            Evaluation::None { .. } => anyhow::bail!("fixture requires evaluation"),
        };
        ensure!(
            eval.subject_in(&tx, &event.tenant_id, subject_id)
                .await?
                .is_some()
                == delivered
        );
    }
    tx.commit().await?;
    Ok(())
}

async fn failure_boundaries(engine: &Engine) -> Result<()> {
    for (table, predicate, projected) in [
        ("gateway_shared_metering_requests", "true", false),
        (
            "gateway_shared_trajectory_events",
            "NEW.kind='request_settled'",
            false,
        ),
        ("gateway_shared_trajectory_outbox", "true", false),
        (
            "gateway_shared_records",
            "NEW.kind='projection_receipt'",
            false,
        ),
        (
            "gateway_shared_records",
            "TG_OP='UPDATE' AND NEW.kind='episode_fence'",
            false,
        ),
        ("gateway_shared_eval_subjects", "true", true),
        ("gateway_shared_eval_results", "true", true),
        ("gateway_shared_eval_admission_events", "true", true),
        (
            "gateway_shared_records",
            "NEW.kind='projection_receipt' AND NEW.status='delivered'",
            true,
        ),
    ] {
        let terminal = seed(engine, true).await?;
        let function = format!("projection_fail_{}", uuid::Uuid::new_v4().simple());
        let statement = format!(
            "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $body$ BEGIN IF NEW.namespace='{}' AND ({predicate}) THEN RAISE EXCEPTION 'injected projection write failure'; END IF; RETURN NEW; END $body$",
            engine.store().namespace()
        );
        // Only generated ASCII names, validated namespace and static predicates
        // enter fixture DDL. Trigger exceptions abort the real transaction.
        // https://www.postgresql.org/docs/current/plpgsql-trigger.html
        engine
            .store()
            .database()
            .execute(Statement::from_string(DatabaseBackend::Postgres, statement))
            .await?;
        engine.store().database().execute(Statement::from_string(DatabaseBackend::Postgres, format!("CREATE TRIGGER {function} BEFORE INSERT OR UPDATE ON {table} FOR EACH ROW EXECUTE FUNCTION {function}()"))).await?;
        let result = engine.deliver(&terminal.record.event).await;
        engine
            .store()
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DROP TRIGGER {function} ON {table}"),
            ))
            .await?;
        engine
            .store()
            .database()
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                format!("DROP FUNCTION {function}()"),
            ))
            .await?;
        ensure!(
            result
                .err()
                .is_some_and(|error| error.chain().any(|cause| cause
                    .to_string()
                    .contains("injected projection write failure"))),
            "injected boundary did not report its write failure: {table}"
        );
        assert_phase(engine, &terminal, projected, false).await?;
        engine.deliver(&terminal.record.event).await?;
        assert_phase(engine, &terminal, true, true).await?;
    }
    Ok(())
}
