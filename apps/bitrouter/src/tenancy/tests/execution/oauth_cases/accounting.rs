use super::*;
use bitrouter_ai::{
    tenancy::execution::{Outcome, ReceiptState},
    types::ApiProtocol,
};
use sea_orm::{ConnectionTrait, TransactionTrait};

/// Component classification regression using an actual model consumer. The
/// private unknown fact is deliberately injected; this is not evidence of an
/// upstream refresh. The positive/fault socket campaigns prove that consumer.
#[tokio::test]
async fn auxiliary_unknown_does_not_erase_actual_model_usage_or_create_a_model_attempt()
-> Result<()> {
    let harness = Harness::new(false).await?;
    harness.provider.model_gate.add_permits(1);
    let execution = harness
        .fixture
        .service
        .admit_execution(
            harness
                .fixture
                .service
                .submission(&harness.headers)
                .map_err(failure)?,
            MODEL,
            &ApiProtocol::Messages,
        )
        .await
        .map_err(failure)?;
    let id = execution.id().clone();
    let owner = execution.receipt_owner()?.clone();
    let row = harness.row().await?;
    let mut auxiliary = owner
        .auxiliary
        .reserve(harness.slot.clone(), row.metadata.slot.clone(), 0)
        .map_err(|code| anyhow::anyhow!("aux reservation {code:?}"))?;
    auxiliary
        .arm([17; 32])
        .map_err(|code| anyhow::anyhow!("aux arm {code:?}"))?;
    drop(auxiliary); // Synchronous private Unknown; model facts remain independent.
    let input = crate::gateway::http::parse_input(
        &ApiProtocol::Messages,
        json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25}),
    )?;
    let crate::gateway::pipeline::types::PipelineInput::Generation(prompt) = input else {
        anyhow::bail!("generation input")
    };
    let mut request =
        crate::gateway::pipeline::types::PipelineRequest::new(MODEL, execution.caller(), *prompt);
    request.request_id = id.to_string();
    request.inbound_protocol = Some(ApiProtocol::Messages);
    request.headers = harness.headers.clone();
    let pipeline = execution.pipeline()?;
    execution.attach(pipeline.clone(), &mut request)?;
    ensure!(
        tokio::time::timeout(TIMEOUT, Arc::clone(&pipeline).execute_detached(request))
            .await?
            .is_err(),
        "auxiliary uncertainty cannot acknowledge success"
    );
    let receipt = harness.receipt(&id).await?;
    ensure!(receipt.outcome == Outcome::Failed && receipt.state == ReceiptState::Incomplete);
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed);
    ensure!(
        receipt.usage == receipt.attempts[0].usage,
        "aggregate must still conform to all actual model attempts"
    );
    ensure!(
        receipt.usage.input_tokens == Some(16)
            && receipt.usage.output_tokens == Some(7)
            && receipt.usage.cache_read_tokens == Some(3)
            && receipt.usage.cache_write_tokens == Some(2)
    );
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 0
            && harness.provider.models.load(Ordering::SeqCst) == 1
    );
    ensure!(owner.durable() && owner.failure().is_none());
    ensure!(
        owner.finish(Outcome::Completed).await
            == Err(bitrouter_ai::tenancy::operation::ErrorCode::StorageUnavailable),
        "durable auxiliary Unknown must still refuse a success ACK"
    );
    owner
        .finish_or_join(Outcome::Cancelled)
        .await
        .map_err(|code| anyhow::anyhow!("durable settlement join: {code:?}"))?;
    let before = crate::tenancy::receipts::store::inventory(
        &harness.fixture.db,
        &harness.fixture.installation,
    )
    .await?;
    ensure!(
        before.rows == 1
            && before.unknown.len() == 1
            && before.unknown[0].0 == harness.fixture.tenant
            && before.unknown[0].1 == id
    );
    drop((pipeline, execution, owner));
    tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == serde_json::to_vec(&receipt)?);
    let after = crate::tenancy::receipts::store::inventory(
        &harness.fixture.db,
        &harness.fixture.installation,
    )
    .await?;
    ensure!(
        before.bytes == after.bytes && before.rows == after.rows && before.unknown == after.unknown
    );
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 0
            && harness.provider.models.load(Ordering::SeqCst) == 1
    );
    harness.close().await
}

#[tokio::test]
async fn auxiliary_presence_and_unknown_projection_are_checked_on_lookup_and_restart_inventory()
-> Result<()> {
    let harness = Harness::new(true).await?;
    harness.provider.refresh_gate.add_permits(1);
    harness.provider.model_gate.add_permits(1);
    let response = harness.send(&Harness::client(false)?, false).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    response.bytes().await?;
    harness.fixture.service.shutdown().await?;
    let before = crate::tenancy::receipts::store::inventory(
        &harness.fixture.db,
        &harness.fixture.installation,
    )
    .await?;
    let slots = storage::inventory(&harness.fixture.db, &harness.fixture.installation).await?;
    ensure!(slots >= 512 * 1024 && before.bytes > slots && before.rows == 1);
    for sql in [
        "DELETE FROM tenancy_receipt_auxiliary",
        "UPDATE tenancy_receipt_auxiliary SET unknown_outcome=1",
        "UPDATE tenancy_receipt_auxiliary SET evidence_json='{}'",
    ] {
        let transaction = harness.fixture.db.begin().await?;
        let result = async {
            transaction.execute_unprepared(sql).await?;
            ensure!(
                crate::tenancy::receipts::store::lookup(
                    &transaction,
                    &harness.fixture.installation,
                    &harness.fixture.tenant,
                    &id
                )
                .await
                .is_err()
            );
            ensure!(
                crate::tenancy::receipts::store::inventory(
                    &transaction,
                    &harness.fixture.installation
                )
                .await
                .is_err()
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        transaction.rollback().await?;
        result?;
        ensure!(harness.receipt(&id).await?.attempts.len() == 1);
    }
    let after = crate::tenancy::receipts::store::inventory(
        &harness.fixture.db,
        &harness.fixture.installation,
    )
    .await?;
    ensure!(before.bytes == after.bytes && before.rows == after.rows);
    harness.close().await
}

#[tokio::test]
async fn durable_started_slot_without_receipt_and_later_unknown_receipt_share_one_capacity_entry()
-> Result<()> {
    let mut harness = Harness::new(true).await?;
    *harness
        .provider
        .reply
        .lock()
        .map_err(|_| anyhow::anyhow!("reply lock"))? = (StatusCode::OK, "{}".to_owned());
    let request = harness.send(&Harness::client(false)?, false);
    let caller = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
    tokio::time::timeout(TIMEOUT, harness.refresh_seen.recv())
        .await?
        .context("real refresh started")?;
    let row = harness.row().await?;
    let storage::State::RefreshStarted(work) = row.metadata.state else {
        anyhow::bail!("durable start missing")
    };
    ensure!(
        crate::tenancy::receipts::store::lookup(
            &harness.fixture.db,
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &work.execution
        )
        .await?
        .is_none()
    );
    let pending = crate::tenancy::receipts::store::inventory(
        &harness.fixture.db,
        &harness.fixture.installation,
    )
    .await?;
    ensure!(
        pending.unknown.len() == 1
            && pending.unknown[0].0 == harness.fixture.tenant
            && pending.unknown[0].1 == work.execution
    );
    harness.provider.refresh_gate.add_permits(1);
    let response = tokio::time::timeout(TIMEOUT, caller).await???;
    ensure!(!response.status().is_success());
    let id = execution_id(&response)?;
    response.bytes().await?;
    ensure!(id == work.execution);
    let settled = crate::tenancy::receipts::store::inventory(
        &harness.fixture.db,
        &harness.fixture.installation,
    )
    .await?;
    ensure!(settled.unknown.len() == 1 && settled.rows == 1 && settled.unknown[0].1 == id);
    ensure!(
        storage::pending_unknown(&harness.fixture.db, &harness.fixture.installation)
            .await?
            .is_empty()
    );
    harness.fixture.service.shutdown().await?;
    harness.close().await
}

/// A real HTTP200 malformed refresh consumes the seed and leaves a quarantined
/// slot. Coherent edits to JSON plus its projection must not erase that unknown
/// merely because the unchanged digest columns still equal each other.
#[tokio::test]
async fn coherent_auxiliary_projection_corruption_cannot_discharge_pending_refresh() -> Result<()> {
    use crate::tenancy::receipts::{auxiliary::Fact, store as receipts};
    use sea_orm::{
        Statement,
        sea_query::{Alias, Expr, Query},
    };
    let harness = Harness::new(true).await?;
    *harness
        .provider
        .reply
        .lock()
        .map_err(|_| anyhow::anyhow!("reply lock"))? = (StatusCode::OK, "{}".to_owned());
    harness.provider.refresh_gate.add_permits(1);
    let response = harness.send(&Harness::client(false)?, false).send().await?;
    ensure!(!response.status().is_success());
    let id = execution_id(&response)?;
    response.bytes().await?;
    harness.fixture.service.shutdown().await?;
    let row = harness.row().await?;
    let storage::State::Quarantined(work) = &row.metadata.state else {
        anyhow::bail!("actual malformed refresh was not quarantined")
    };
    ensure!(work.execution == id && harness.provider.refreshes.load(Ordering::SeqCst) == 1);
    ensure!(harness.provider.models.load(Ordering::SeqCst) == 0);
    let receipt = harness.receipt(&id).await?;
    ensure!(receipt.attempts.is_empty() && receipt.state == ReceiptState::Incomplete);
    let inventory = receipts::inventory(&harness.fixture.db, &harness.fixture.installation).await?;
    ensure!(inventory.unknown.len() == 1 && inventory.unknown[0].1 == id);
    let fields = Query::select()
        .column(Alias::new("evidence_json"))
        .from(Alias::new("tenancy_receipt_auxiliary"))
        .and_where(
            Expr::col(Alias::new("installation_id")).eq(harness.fixture.installation.as_str()),
        )
        .and_where(Expr::col(Alias::new("tenant_id")).eq(harness.fixture.tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
        .to_owned();
    let original = harness
        .fixture
        .db
        .query_one(harness.fixture.db.get_database_backend().build(&fields))
        .await?
        .context("original auxiliary evidence")?
        .try_get::<String>("", "evidence_json")?;
    let facts: Vec<Fact> = serde_json::from_str(&original)?;
    let original_candidate = receipts::lookup_candidate(
        &harness.fixture.db,
        &harness.fixture.installation,
        &harness.fixture.tenant,
        &id,
    )
    .await?
    .context("original receipt with bound usage evidence")?;
    let usage_fields = Query::select()
        .columns([Alias::new("evidence_json"), Alias::new("evidence_digest")])
        .from(Alias::new("tenancy_usage_evidence"))
        .and_where(
            Expr::col(Alias::new("installation_id")).eq(harness.fixture.installation.as_str()),
        )
        .and_where(Expr::col(Alias::new("tenant_id")).eq(harness.fixture.tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
        .to_owned();
    let original_usage = harness
        .fixture
        .db
        .query_one(
            harness
                .fixture
                .db
                .get_database_backend()
                .build(&usage_fields),
        )
        .await?
        .context("original usage sidecar")?;
    let outcome: Result<()> = async {
        // These recomputed, self-consistent records are deliberate storage fault
        // injections. Point integrity alone cannot bind them to another work,
        // generation, credential slot or creator of the pending refresh.
        for dimension in ["work", "generation", "credential", "principal"] {
            let mut changed = facts.clone();
            let fact = changed.first_mut().context("actual refresh fact")?;
            let mut public = receipt.clone();
            match dimension {
                "work" => fact.work = uuid::Uuid::new_v4().to_string(),
                "generation" => fact.generation += 1,
                "credential" => {
                    fact.credential.resource_id = "different-slot".try_into()?;
                    fact.slot = storage::slot(&harness.fixture.installation, &fact.credential)?;
                }
                _ => public.principal_id = "different-creator".try_into()?,
            }
            let replacement = receipts::Candidate::new(public, receipts::Disposition {
                dispatch: bitrouter_ai::tenancy::operation::DispatchState::NotStarted,
                statuses: Vec::new(), auxiliary_digest: None,
            })?.with_auxiliary(changed)?.with_preserved_usage_evidence_for_test(&original_candidate)?;
            let transaction = harness.fixture.db.begin().await?;
            let check: Result<()> = async {
                for table in ["tenancy_receipt_auxiliary", "tenancy_usage_evidence", "tenancy_execution_receipts"] {
                    let deletion = Query::delete().from_table(Alias::new(table))
                        .and_where(Expr::col(Alias::new("installation_id")).eq(harness.fixture.installation.as_str()))
                        .and_where(Expr::col(Alias::new("tenant_id")).eq(harness.fixture.tenant.as_str()))
                        .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str())).to_owned();
                    ensure!(transaction.execute(transaction.get_database_backend().build(&deletion)).await?.rows_affected() == 1);
                }
                ensure!(receipts::insert(&transaction, &replacement).await?);
                crate::tenancy::receipts::auxiliary_store::insert(&transaction, &replacement).await?;
                ensure!(receipts::lookup(&transaction, &harness.fixture.installation, &harness.fixture.tenant, &id).await?.is_some());
                let preserved_usage = transaction.query_one(transaction.get_database_backend().build(&usage_fields))
                    .await?.context("preserved usage sidecar")?;
                for field in ["evidence_json", "evidence_digest"] {
                    ensure!(preserved_usage.try_get::<String>("", field)? == original_usage.try_get::<String>("", field)?,
                        "{dimension} fault changed unrelated original usage evidence");
                }
                ensure!(storage::pending_unknown(&transaction, &harness.fixture.installation).await.is_err(), "unbound {dimension} discharged pending work");
                ensure!(receipts::inventory(&transaction, &harness.fixture.installation).await.is_err());
                Ok(())
            }.await;
            transaction.rollback().await?;
            check?;
            ensure!(
                serde_json::to_vec(&harness.receipt(&id).await?)?
                    == serde_json::to_vec(&receipt)?
            );
        }
        let mutation = Statement::from_sql_and_values(harness.fixture.db.get_database_backend(),
            "UPDATE tenancy_receipt_auxiliary SET evidence_json=json_set(evidence_json, '$[0].outcome', 'rejected', '$[0].http_status', 400),unknown_outcome=0 WHERE installation_id=? AND tenant_id=? AND execution_id=?",
            [harness.fixture.installation.as_str().into(), harness.fixture.tenant.as_str().into(), id.as_str().into()]);
        ensure!(harness.fixture.db.execute(mutation).await?.rows_affected() == 1);
        ensure!(harness.receipt(&id).await.is_err());
        ensure!(receipts::inventory(&harness.fixture.db, &harness.fixture.installation).await.is_err());
        ensure!(storage::pending_unknown(&harness.fixture.db, &harness.fixture.installation).await.is_err());
        let restarted = tokio::time::timeout(TIMEOUT, Service::start_prepared(
            harness.fixture.db.clone(), None, Default::default(),
            crate::tenancy::store::load(&harness.fixture.db, chrono::Utc::now()).await?,
            harness.resources.clone(),
        )).await?;
        match restarted {
            Err(_) => {},
            Ok(service) => {
                service.shutdown().await?;
                anyhow::bail!("corrupted auxiliary evidence passed actual service startup");
            }
        }
        Ok(())
    }.await;
    // Restore committed fixture corruption on either outcome before closing the
    // owned peer/provider. The original immutable receipt bytes are not edited.
    let restored: Result<()> = async {
        let restoration = Query::update()
            .table(Alias::new("tenancy_receipt_auxiliary"))
            .values([
                (Alias::new("evidence_json"), original.into()),
                (Alias::new("unknown_outcome"), true.into()),
            ])
            .and_where(
                Expr::col(Alias::new("installation_id")).eq(harness.fixture.installation.as_str()),
            )
            .and_where(Expr::col(Alias::new("tenant_id")).eq(harness.fixture.tenant.as_str()))
            .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
            .to_owned();
        ensure!(
            harness
                .fixture
                .db
                .execute(
                    harness
                        .fixture
                        .db
                        .get_database_backend()
                        .build(&restoration)
                )
                .await?
                .rows_affected()
                == 1
        );
        ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == serde_json::to_vec(&receipt)?);
        let restored =
            receipts::inventory(&harness.fixture.db, &harness.fixture.installation).await?;
        ensure!(restored.bytes == inventory.bytes && restored.unknown == inventory.unknown);
        ensure!(
            storage::pending_unknown(&harness.fixture.db, &harness.fixture.installation)
                .await?
                .is_empty()
        );
        ensure!(
            harness.provider.refreshes.load(Ordering::SeqCst) == 1
                && harness.provider.models.load(Ordering::SeqCst) == 0
        );
        Ok(())
    }
    .await;
    let closed = harness.close().await;
    outcome?;
    restored?;
    closed
}
