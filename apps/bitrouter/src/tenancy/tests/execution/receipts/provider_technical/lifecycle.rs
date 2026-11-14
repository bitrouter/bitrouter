use super::*;
use crate::tenancy::actor::provider_technical::tests::Point;
use bitrouter_ai::tenancy::execution::Outcome;

#[tokio::test]
async fn pending_optional_sql_never_owns_second_authority_credential_receipt_or_stop() -> Result<()>
{
    for point in [Point::BeforeInsert, Point::Driver, Point::AfterInsert] {
        let (_home, harness) = owned(
            "chat_completions",
            StatusCode::OK,
            serde_json::to_string(&json_response(
                "chat_completions",
                full_usage("chat_completions"),
            )?)?,
            false,
        )
        .await?;
        let gate = harness.fixture.service.technical_io.hold(point)?;
        let checked: Result<()> = async {
            let client = Harness::client(false)?;
            let response = harness.send(&client).send().await?;
            ensure!(response.status() == StatusCode::OK);
            let a = execution_id(&response)?;
            response.bytes().await?;
            tokio::time::timeout(TIMEOUT, gate.entered.cancelled()).await?;
            let (logical, physical, fenced) = harness
                .fixture
                .service
                .admission
                .optional_accounting_for_test()
                .map_err(|e| anyhow::anyhow!("accounting unavailable: {e:?}"))?;
            ensure!(
                logical > 0 && physical > 0 && !fenced,
                "pending SQL refunded or fenced original admission"
            );
            // These are the original real actor jobs. Their own five-second
            // budget must complete without releasing the held optional gate.
            tokio::time::timeout(crate::tenancy::MANAGEMENT_TIMEOUT, async {
                let token = harness.credential.secret.as_deref().context("credential")?;
                harness
                    .fixture
                    .service
                    .authenticate_live(token)
                    .await
                    .map_err(failure)?;
                ensure!(
                    harness
                        .fixture
                        .service
                        .credential(
                            &harness.fixture.operator,
                            &harness.fixture.tenant,
                            &harness.credential.credential_id
                        )
                        .await
                        .map_err(failure)?
                        .credential_id
                        == harness.credential.credential_id
                );
                let original = harness.receipt(&client, &a).await?;
                ensure!(original.outcome == Outcome::Completed && original.attempts.len() == 1);
                Ok::<(), anyhow::Error>(())
            })
            .await??;
            ensure!(
                !gate.release.is_cancelled(),
                "main progress depended on test releasing optional I/O"
            );
            tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(&a)).await??;
            harness.fixture.service.technical_io.clear_gate()?;
            let facts = measured(&harness, &client, &a).await?;
            ensure!(
                facts.is_some() == (point == Point::AfterInsert),
                "closed readback classified lost ACK or absence incorrectly"
            );
            let response = harness.send(&client).send().await?;
            ensure!(response.status() == StatusCode::OK);
            let b = execution_id(&response)?;
            response.bytes().await?;
            // Wait for the real disposable task before another actor read can
            // preempt it. The HTTP getter, not captured(), proves B was stored.
            let b_facts = measured(&harness, &client, &b)
                .await?
                .context("normal sample did not resume after confirmed no-write preemption")?;
            let b_original = harness.receipt(&client, &b).await?;
            ensure!(b != a && b_original.outcome == Outcome::Completed);
            b_facts.validate_against(&b_original)?;
            ensure!(
                b_facts.operations.len() == 1
                    && !b_facts.operations[0].streamed
                    && !b_facts.operations[0].partial
                    && b_facts.operations[0].upstream_duration_ms.is_some(),
                "later normal B lost its own durable provider sample"
            );
            ensure!(
                harness.calls.load(Ordering::SeqCst) == 2,
                "metrics retried inference"
            );
            Ok(())
        }
        .await;
        let shutdown = tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await?;
        checked?;
        shutdown?;
        let (logical, physical, fenced) = harness
            .fixture
            .service
            .admission
            .optional_accounting_for_test()
            .map_err(|e| anyhow::anyhow!("accounting unavailable: {e:?}"))?;
        ensure!(physical == 0 && !fenced);
        ensure!(
            logical
                == crate::tenancy::receipts::provider_technical::storage::retained_bytes(
                    &harness.fixture.db
                )
                .await?,
            "closed actual bytes disagree with logical ledger"
        );
    }
    Ok(())
}

#[tokio::test]
async fn stop_preempts_actual_disposable_driver_and_joins_before_capacity_release() -> Result<()> {
    let (_home, harness) = owned(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
    )
    .await?;
    let gate = harness.fixture.service.technical_io.hold(Point::Driver)?;
    let checked: Result<()> = async {
        let response = harness.send(&Harness::client(false)?).send().await?;
        ensure!(response.status() == StatusCode::OK);
        let id = execution_id(&response)?;
        response.bytes().await?;
        tokio::time::timeout(TIMEOUT, gate.entered.cancelled()).await?;
        let (logical, physical, fenced) = harness
            .fixture
            .service
            .admission
            .optional_accounting_for_test()
            .map_err(|e| anyhow::anyhow!("accounting unavailable: {e:?}"))?;
        ensure!(logical > 0 && physical > 0 && !fenced);
        tokio::time::timeout(
            crate::tenancy::MANAGEMENT_TIMEOUT,
            harness.fixture.service.shutdown(),
        )
        .await??;
        ensure!(
            !gate.release.is_cancelled(),
            "stop required external gate release"
        );
        tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(&id)).await??;
        let (logical, physical, fenced) = harness
            .fixture
            .service
            .admission
            .optional_accounting_for_test()
            .map_err(|e| anyhow::anyhow!("accounting unavailable: {e:?}"))?;
        ensure!(logical == 0 && physical == 0 && !fenced);
        Ok(())
    }
    .await;
    let shutdown = harness.fixture.service.shutdown().await;
    checked.and(shutdown)
}

#[tokio::test]
async fn healthy_required_48_service_survives_optional_name_conflict_and_real_ddl_refusal()
-> Result<()> {
    for conflict in [true, false] {
        let home = tempfile::tempdir()?;
        let lease =
            crate::tenancy::ownership::acquire_path(&home.path().join("healthy48.sqlite"), true)?;
        let db = crate::db::connect_tenancy(&format!("sqlite://{}", lease.path.display()), &lease)
            .await?;
        crate::db::run_migrations(&db).await?;
        if conflict {
            db.execute_unprepared("CREATE TABLE tenancy_provider_technical_facts(retain TEXT NOT NULL,technical_json TEXT NOT NULL); INSERT INTO tenancy_provider_technical_facts VALUES('original-unknown-object',printf('%.*c',1024,'x'))").await?;
        }
        // Unknown object exists BEFORE genuine startup inventory. Its lookalike
        // column cannot be adopted/counted as registered optional capacity.
        let fixture = Fixture::with_database(db, &mut |_| {}).await?;
        let harness = Harness::with_initial(
            "chat_completions",
            StatusCode::OK,
            serde_json::to_string(&json_response(
                "chat_completions",
                full_usage("chat_completions"),
            )?)?,
            false,
            None,
            false,
            Some((fixture, lease)),
        )
        .await?;
        let checked: Result<()> = async {
            ensure!(
                harness
                    .fixture
                    .service
                    .admission
                    .optional_accounting_for_test()
                    .map_err(|e| anyhow::anyhow!("accounting unavailable: {e:?}"))?
                    .0
                    == 0,
                "unknown optional object affected ordinary startup capacity"
            );
            if !conflict {
                harness
                    .fixture
                    .service
                    .technical_io
                    .hold(Point::DdlReadOnly)?;
            }
            crate::db::run_migrations(&harness.fixture.db).await?;
            let client = Harness::client(false)?;
            let response = harness.send(&client).send().await?;
            ensure!(response.status() == StatusCode::OK);
            let id = execution_id(&response)?;
            response.bytes().await?;
            tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(&id)).await??;
            ensure!(harness.receipt(&client, &id).await?.outcome == Outcome::Completed);
            let token = harness.credential.secret.as_deref().context("credential")?;
            harness
                .fixture
                .service
                .authenticate_live(token)
                .await
                .map_err(failure)?;
            ensure!(
                crate::db::migration::provider_technical::validate(&harness.fixture.db)
                    .await
                    .is_err()
            );
            let response = client
                .get(format!(
                    "{}/control/v1/tenants/{}/executions/{id}/provider-technical-facts/v1",
                    harness.base, harness.fixture.tenant
                ))
                .bearer_auth(token)
                .header(
                    "Bitrouter-Installation-Id",
                    harness.fixture.installation.as_str(),
                )
                .send()
                .await?;
            if conflict {
                ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
                let failure: bitrouter_ai::tenancy::operation::Error = response.json().await?;
                ensure!(
                    failure.code == bitrouter_ai::tenancy::operation::ErrorCode::AuthorityUnavailable
                );
            } else {
                ensure!(response.status() == StatusCode::OK);
                ensure!(response.headers().get("cache-control").is_some_and(|v| v == "no-store"));
                ensure!(response.json::<Option<bitrouter_ai::tenancy::provider_technical_facts::ProviderTechnicalFactsV1>>().await?.is_none());
                ensure!(!crate::db::migration::provider_technical::available(&harness.fixture.db).await?);
            }
            let version = harness
                .fixture
                .db
                .query_one(sea_orm::Statement::from_string(
                    sea_orm::DatabaseBackend::Sqlite,
                    "SELECT count(*) AS count FROM seaql_migrations WHERE version LIKE '%000049%'",
                ))
                .await?
                .context("required ledger")?;
            ensure!(version.try_get::<i64>("", "count")? == 0);
            if conflict {
                let row = harness
                    .fixture
                    .db
                    .query_one(sea_orm::Statement::from_string(
                        sea_orm::DatabaseBackend::Sqlite,
                        "SELECT retain FROM tenancy_provider_technical_facts",
                    ))
                    .await?
                    .context("preserved unknown object")?;
                ensure!(row.try_get::<String>("", "retain")? == "original-unknown-object");
            }
            ensure!(harness.calls.load(Ordering::SeqCst) == 1);
            Ok(())
        }
        .await;
        let shutdown = harness.fixture.service.shutdown().await;
        checked.and(shutdown)?;
    }
    Ok(())
}

#[tokio::test]
async fn actual_commit_then_unwind_or_unreadable_readback_never_refunds_possible_rows() -> Result<()>
{
    for panic_after_insert in [true, false] {
        let (_home, harness) = owned(
            "chat_completions",
            StatusCode::OK,
            serde_json::to_string(&json_response(
                "chat_completions",
                full_usage("chat_completions"),
            )?)?,
            false,
        )
        .await?;
        let gate = harness
            .fixture
            .service
            .technical_io
            .hold(if panic_after_insert {
                Point::PanicAfterInsert
            } else {
                Point::AfterInsert
            })?;
        let checked: Result<()> = async {
            let client = Harness::client(false)?;
            let response = harness.send(&client).send().await?;
            ensure!(response.status() == StatusCode::OK);
            let a = execution_id(&response)?;
            response.bytes().await?;
            tokio::time::timeout(TIMEOUT, gate.entered.cancelled()).await?;
            if !panic_after_insert {
                // Corrupt only the optional fixture object while its INSERT is
                // positively committed and the outer caller has no result yet.
                harness
                    .fixture
                    .db
                    .execute_unprepared("UPDATE tenancy_provider_technical_facts SET byte_count=0")
                    .await?;
            }
            harness
                .fixture
                .service
                .authenticate_live(harness.credential.secret.as_deref().context("credential")?)
                .await
                .map_err(failure)?;
            tokio::time::timeout(TIMEOUT, harness.fixture.service.technical_io.wait(&a)).await??;
            let (logical, physical, fenced) = harness
                .fixture
                .service
                .admission
                .optional_accounting_for_test()
                .map_err(|e| anyhow::anyhow!("accounting unavailable: {e:?}"))?;
            ensure!(
                logical > 0 && physical == 0 && !fenced,
                "actual or possible committed row was refunded/fenced"
            );
            ensure!(harness.receipt(&client, &a).await?.outcome == Outcome::Completed);
            if panic_after_insert {
                ensure!(measured(&harness, &client, &a).await?.is_some());
            } else {
                let response = harness.send(&client).send().await?;
                ensure!(response.status() == StatusCode::OK);
                let b = execution_id(&response)?;
                response.bytes().await?;
                ensure!(harness.receipt(&client, &b).await?.outcome == Outcome::Completed);
                let rows = harness
                    .fixture
                    .db
                    .query_one(sea_orm::Statement::from_string(
                        sea_orm::DatabaseBackend::Sqlite,
                        "SELECT count(*) AS count FROM tenancy_provider_technical_facts",
                    ))
                    .await?
                    .context("actual optional rows")?;
                ensure!(
                    rows.try_get::<i64>("", "count")? == 1,
                    "unresolved optional accounting funded another write"
                );
            }
            Ok(())
        }
        .await;
        let shutdown = harness.fixture.service.shutdown().await;
        checked.and(shutdown)?;
    }
    Ok(())
}
