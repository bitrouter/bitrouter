//! Real PostgreSQL rollback and replay campaigns for the two projection phases.

use super::*;
use crate::gateway::shared::{authority::Authority, schema, test_database::EmptyDatabase};
use crate::runtime_crypto::DeploymentKey;
use crate::storage::evaluation::shared::SharedEvaluation;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::time::Duration;

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
        let a = join_engine(first.clone(), deployment_key).await?;
        let b = join_engine(second.clone(), deployment_key).await?;
        lease_recovery(&a, &b).await?;
        {
            let terminal = seed(&a).await?;
            let tx = first.begin().await?;
            a.authority.fence(&tx).await?;
            a.project_in(&tx, &terminal).await?;
            tx.rollback().await?;
            assert_phase(&a, &terminal, false, false).await?;
            let (left, right) = tokio::join!(a.deliver(&terminal.record.event), b.deliver(&terminal.record.event));
            left?; right?;
            assert_phase(&b, &terminal, true, true).await?;
            let original: Stored<Receipt> = first.get(Kind::ProjectionReceipt, &terminal.record.event.event_id).await?.context("missing delivered receipt")?;
            b.deliver(&terminal.record.event).await?;
            let replay: Stored<Receipt> = second.get(Kind::ProjectionReceipt, &terminal.record.event.event_id).await?.context("missing replay receipt")?;
            ensure!(original.revision()? == replay.revision()? && original.value.projection == replay.value.projection);
            receipt_phase_corruption(&b, &terminal, &original).await?;
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
        prejoin_rejects_guarded_history(&a, deployment_key).await?;
        Ok::<(), anyhow::Error>(())
    }.await
}

async fn join_engine(store: Store, deployment_key: [u8; 32]) -> Result<Engine> {
    let authority = Authority::new(store, DeploymentKey::from_bytes(deployment_key)?);
    Engine::join(authority, 30_000).await
}

async fn lease_recovery(first: &Engine, second: &Engine) -> Result<()> {
    let terminal = seed(first).await?;
    let event = &terminal.record.event;
    let claim_a = first
        .claim_outbox(1, 100)
        .await?
        .pop()
        .context("first claim missing")?;
    ensure!(claim_a.event == *event);
    let tx = first.store().begin().await?;
    first.authority.fence(&tx).await?;
    first.project_in(&tx, &terminal).await?;
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

async fn receipt_phase_corruption(
    engine: &Engine,
    terminal: &Terminal,
    original: &Stored<Receipt>,
) -> Result<()> {
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
    let evidence_before = evidence_snapshot(engine).await?;
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
        ensure!(evidence_snapshot(engine).await? == evidence_before);
        let tx = engine.store().begin().await?;
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

async fn seed(engine: &Engine) -> Result<Terminal> {
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
    record.request_id = None;
    let inputs = crate::gateway::shared::capture::projection::tests::untracked(&record)?;
    let execution = Execution {
        tenant_id: record.event.tenant_id.clone(),
        principal_id: record.principal_id.clone(),
        authorization_revision: "fixture".into(),
        tenant_revision: 1,
        owner: engine.owner.clone(),
        trajectory: None,
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
    if let Some(receipt) = receipt {
        let eval = SharedEvaluation::new(engine.store().namespace())?;
        let subject_id = match &receipt.value.projection.evaluation {
            Evaluation::Request { eval_id, .. } => eval_id,
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
            "gateway_shared_records",
            "NEW.kind='projection_receipt'",
            false,
        ),
        ("gateway_shared_eval_subjects", "true", true),
        (
            "gateway_shared_records",
            "NEW.kind='projection_receipt' AND NEW.status='delivered'",
            true,
        ),
    ] {
        let terminal = seed(engine).await?;
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

async fn evidence_snapshot(engine: &Engine) -> Result<String> {
    engine.store().database().query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT jsonb_build_object(
            'metering',(SELECT COALESCE(jsonb_agg(to_jsonb(m) ORDER BY execution_id),'[]'::jsonb) FROM gateway_shared_metering_requests m WHERE namespace=$1),
            'subjects',(SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY eval_id),'[]'::jsonb) FROM gateway_shared_eval_subjects s WHERE namespace=$1),
            'results',(SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY result_id),'[]'::jsonb) FROM gateway_shared_eval_results r WHERE namespace=$1),
            'admissions',(SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY sequence),'[]'::jsonb) FROM gateway_shared_eval_admission_events a WHERE namespace=$1)
        )::text AS snapshot",
        [engine.store().namespace().into()]
    )).await?.context("missing evidence snapshot")?.try_get("", "snapshot").map_err(Into::into)
}

async fn record_snapshot(store: &Store) -> Result<String> {
    store.database().query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY id),'[]'::jsonb)::text AS snapshot FROM gateway_shared_records r WHERE namespace=$1",
        [store.namespace().into()]
    )).await?.context("missing record snapshot")?.try_get("", "snapshot").map_err(Into::into)
}

async fn replace_body(
    store: &Store,
    kind: &str,
    key: &str,
    body: &serde_json::Value,
) -> Result<()> {
    ensure!(store.database().execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "UPDATE gateway_shared_records SET body=$1 WHERE namespace=$2 AND kind=$3 AND key=$4",
        [serde_json::to_string(body)?.into(), store.namespace().into(), kind.into(), key.into()]
    )).await?.rows_affected() == 1);
    Ok(())
}

async fn prejoin_rejects_guarded_history(engine: &Engine, key: [u8; 32]) -> Result<()> {
    // More than one page of terminal history precedes the attacked last row.
    // Current ledger/head/publications remain unchanged throughout the campaign.
    for _ in 0..33 {
        seed(engine).await?;
    }
    let store = engine.store();
    let last = store.database().query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT key FROM gateway_shared_records WHERE namespace=$1 AND kind='execution' ORDER BY key DESC LIMIT 1",
        [store.namespace().into()]
    )).await?.context("missing terminal history")?.try_get::<String>("", "key")?;
    let retained: Stored<serde_json::Value> = store
        .get(Kind::Execution, &last)
        .await?
        .context("missing execution")?;
    let original = retained.value;
    let mut old_requirement = original.clone();
    old_requirement["projection"]["requirements"]["trajectory"] = "guarded_policies".into();
    let mut old_capture = original.clone();
    old_capture["projection"]["policy"]["guarded"] = true.into();
    let mut old_binding = original.clone();
    let binding = serde_json::json!({"owner_user_id": original["tenant_id"], "episode_id":"retired", "request_id":"retired", "correlation_epoch":"retired"});
    old_binding["trajectory"] = binding.clone();
    let mut missing_capture = original.clone();
    missing_capture["projection"] = serde_json::Value::Null;
    let mut old_digest = original.clone();
    old_digest["projection"]["requirements"]["policy_lock_digest"] =
        old_digest["projection"]["requirements"]["source_digest"].clone();
    old_digest["projection"]["requirements"]
        .as_object_mut()
        .context("requirements object")?
        .remove("source_digest");
    for body in [
        old_requirement,
        old_capture,
        old_binding,
        missing_capture,
        old_digest,
    ] {
        replace_body(store, "execution", &last, &body).await?;
        let before = record_snapshot(store).await?;
        let evidence = evidence_snapshot(engine).await?;
        let rejected = join_engine(store.clone(), key).await;
        ensure!(
            rejected.is_err(),
            "retired execution registered a new incarnation"
        );
        ensure!(record_snapshot(store).await? == before);
        ensure!(evidence_snapshot(engine).await? == evidence);
        replace_body(store, "execution", &last, &original).await?;
    }
    let terminal = seed(engine).await?;
    engine.deliver(&terminal.record.event).await?;
    let receipt_key = &terminal.record.event.event_id;
    let retained: Stored<serde_json::Value> = store
        .get(Kind::ProjectionReceipt, receipt_key)
        .await?
        .context("missing projection receipt")?;
    let original_receipt = retained.value;
    let mut old_sink = original_receipt.clone();
    old_sink["projection"]["evaluation"] = serde_json::json!({"sink":"trajectory", "reference":{}});
    let mut old_completion = original_receipt.clone();
    old_completion["completion"] = serde_json::json!({"sink":"trajectory", "admission":{}});
    let mut bound = original_receipt.clone();
    bound["projection"]["binding"] = binding;
    for body in [old_sink, old_completion, bound] {
        replace_body(store, "projection_receipt", receipt_key, &body).await?;
        let before = record_snapshot(store).await?;
        ensure!(
            join_engine(store.clone(), key).await.is_err(),
            "retired projection registered an incarnation"
        );
        ensure!(record_snapshot(store).await? == before);
        replace_body(store, "projection_receipt", receipt_key, &original_receipt).await?;
    }
    let guard_id = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    store.database().execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "INSERT INTO gateway_shared_records (id,namespace,kind,key,revision,status,due_at_ms,updated_at_ms,body) VALUES ($1,$2,'episode_fence','retired-fence',0,'pending',NULL,0,'{}')",
        [guard_id.clone().into(), store.namespace().into()]
    )).await?;
    let before = record_snapshot(store).await?;
    ensure!(join_engine(store.clone(), key).await.is_err());
    ensure!(record_snapshot(store).await? == before);
    store
        .database()
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "DELETE FROM gateway_shared_records WHERE namespace=$1 AND id=$2",
            [store.namespace().into(), guard_id.into()],
        ))
        .await?;
    for (kind, json_kind) in [
        ("guard_activated", "request_started"),
        ("request_started", "guard_activated"),
    ] {
        let event = format!("retired-{}", uuid::Uuid::new_v4());
        store.database().execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
            "INSERT INTO gateway_shared_trajectory_events (namespace,owner_user_id,event_id,episode_id,request_id,sequence,kind,captured_at,content_digest,event_json) VALUES ($1,'tenant',$2,$2,NULL,1,$3,'2026-11-18T00:00:00Z',$4,$5)",
            [store.namespace().into(), event.clone().into(), kind.into(), format!("sha256:{}", "a".repeat(64)).into(), serde_json::json!({"kind":json_kind}).to_string().into()]
        )).await?;
        let before = record_snapshot(store).await?;
        ensure!(join_engine(store.clone(), key).await.is_err());
        ensure!(record_snapshot(store).await? == before);
        let retained = store.database().query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
            "SELECT kind,event_json FROM gateway_shared_trajectory_events WHERE namespace=$1 AND event_id=$2",
            [store.namespace().into(), event.clone().into()]
        )).await?.context("guard history disappeared")?;
        ensure!(retained.try_get::<String>("", "kind")? == kind);
        ensure!(
            retained.try_get::<String>("", "event_json")?
                == serde_json::json!({"kind":json_kind}).to_string()
        );
        store
            .database()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "DELETE FROM gateway_shared_trajectory_events WHERE namespace=$1 AND event_id=$2",
                [store.namespace().into(), event.into()],
            ))
            .await?;
    }
    // All current request captures/receipts still admit a fresh worker.
    join_engine(store.clone(), key).await?;
    Ok(())
}
