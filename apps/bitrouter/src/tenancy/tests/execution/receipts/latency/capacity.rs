//! Real provider, authenticated Service, durable store, and actual OTLP consumers.
use super::*;
use bitrouter_ai::tenancy::execution::ReceiptState;
use sea_orm::Statement;

pub(super) async fn stored_policy_bytes(h: &Harness) -> Result<u64> {
    // Derive this witness from actual stored UTF-8 bytes, independently of
    // Admission, Candidate and restart inventory. Every receipt and retained
    // sidecar carries its own 16 KiB policy overhead, including empty capture.
    let row = h.fixture.db.query_one(Statement::from_string(
        h.fixture.db.get_database_backend(),
        r#"WITH retained AS (
            SELECT 'receipt' AS kind, byte_count,
                length(CAST(receipt_json AS BLOB)) + length(CAST(disposition_json AS BLOB)) AS encoded_bytes
            FROM tenancy_execution_receipts
            UNION ALL
            SELECT 'usage', byte_count, length(CAST(evidence_json AS BLOB))
            FROM tenancy_usage_evidence
            UNION ALL
            SELECT 'auxiliary', length(CAST(evidence_json AS BLOB)), length(CAST(evidence_json AS BLOB))
            FROM tenancy_receipt_auxiliary
        )
        SELECT COUNT(*) AS rows, COALESCE(SUM(encoded_bytes),0) AS bytes,
            COALESCE(SUM(byte_count != encoded_bytes),0) AS mismatched_bytes,
            COALESCE(SUM(kind = 'receipt'),0) AS receipt_rows,
            COALESCE(SUM(kind = 'usage'),0) AS usage_rows,
            (SELECT COUNT(*) FROM tenancy_oauth_slots) AS oauth_slots
        FROM retained"#.to_owned(),
    )).await?.context("stored accounting")?;
    let rows = u64::try_from(row.try_get::<i64>("", "rows")?)?;
    let bytes = u64::try_from(row.try_get::<i64>("", "bytes")?)?;
    let receipts = u64::try_from(row.try_get::<i64>("", "receipt_rows")?)?;
    let usage = u64::try_from(row.try_get::<i64>("", "usage_rows")?)?;
    ensure!(
        receipts > 0 && usage == receipts,
        "new receipts require captured usage sidecars"
    );
    ensure!(
        row.try_get::<i64>("", "mismatched_bytes")? == 0,
        "stored byte counts differ from encoded JSON bytes"
    );
    // These fixtures use static provider credentials. OAuth slot envelopes and
    // pending-refresh obligations require their separate accounting campaign.
    ensure!(
        row.try_get::<i64>("", "oauth_slots")? == 0,
        "unexpected OAuth accounting in static fixture"
    );
    rows.checked_mul(16 * 1024)
        .and_then(|overhead| bytes.checked_add(overhead))
        .context("stored receipt and sidecar accounting overflow")
}
pub(super) fn assert_frozen(
    view: &collector::View,
    initial: u64,
    final_bytes: u64,
    unknown: u64,
) -> Result<()> {
    let frozen = view
        .frozen
        .as_ref()
        .context("single complete initial/final gauge batch")?;
    ensure!(frozen.get("initial:accounted") == Some(&initial));
    ensure!(frozen.get("final:accounted") == Some(&final_bytes));
    ensure!(frozen.get("final:live_unknown") == Some(&unknown));
    for key in ["initial:available", "final:available", "final:closed"] {
        ensure!(frozen.get(key) == Some(&1), "{key}");
    }
    for key in [
        "final:completion_reserved",
        "final:ingress_reserved",
        "final:potential_unknown",
        "final:active",
        "final:queued",
        "final:ingress",
        "final:fenced",
    ] {
        ensure!(frozen.get(key) == Some(&0), "{key}");
    }
    Ok(())
}
pub(super) async fn wait_current(collector: &Collector, field: &str, expected: u64) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if collector.view()?.current.get(field) == Some(&expected) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    Ok(())
}
pub(super) async fn durable_unknown_and_incomplete_restart() -> Result<()> {
    for unknown in [false, true] {
        let collector = Collector::start().await?;
        let (first_exporter, sink) = exporter(&collector, 3_600_000, None)?;
        let body = if unknown {
            super::super::wire::chat_stream(full_usage("chat_completions"), false)?
        } else {
            serde_json::to_string(&json_response(
                "chat_completions",
                json!({"prompt_tokens":11,"completion_tokens":7}),
            )?)?
        };
        let h = Harness::with_observer(
            "chat_completions",
            StatusCode::OK,
            body,
            unknown,
            None,
            false,
            sink.clone(),
        )
        .await?;
        let response = h.send(&Harness::client(false)?).send().await?;
        ensure!(response.status() == StatusCode::OK);
        let id = execution_id(&response)?;
        let _ = response.bytes().await?;
        let receipt = h.receipt(&Harness::client(false)?, &id).await?;
        ensure!(
            receipt.state
                == if unknown {
                    ReceiptState::Unknown
                } else {
                    ReceiptState::Incomplete
                }
        );
        // A truncated stream fails delivery while the provider's completion
        // remains unknown. The durable state and original attempt preserve
        // that uncertainty independently of the caller's terminal outcome.
        ensure!(
            receipt.outcome
                == if unknown {
                    Outcome::Failed
                } else {
                    Outcome::Completed
                }
        );
        ensure!(receipt.attempts.len() == 1);
        ensure!(
            receipt.attempts[0].outcome
                == if unknown {
                    Outcome::Unknown
                } else {
                    Outcome::Completed
                }
        );
        ensure!(
            receipt.usage.input_tokens == Some(11)
                && receipt.usage.output_tokens == Some(7)
                && !receipt.usage.complete
        );
        h.fixture.service.shutdown().await?;
        let bytes = stored_policy_bytes(&h).await?;
        let sealed = sink.seal(true);
        let flushed = flush(&first_exporter).await?;
        ensure!(sealed != unknown && flushed);
        let view = collector.view()?;
        ensure!(view.count("durable:evidence_unknown") == u64::from(unknown));
        ensure!(view.count("durable:terminal_other") == u64::from(!unknown));
        ensure!(view.event("sealed_failed") == u64::from(unknown));
        ensure!(view.event("sealed_clean") == u64::from(!unknown));
        assert_frozen(&view, 0, bytes, u64::from(unknown))?;
        collector.close().await?;
        // A new actual actor reloads this durable inventory; no observation state is copied.
        let second = Collector::start().await?;
        let (exporter2, sink2) = exporter(&second, 3_600_000, None)?;
        let snapshot = crate::tenancy::store::load(&h.fixture.db, chrono::Utc::now()).await?;
        let restarted = crate::tenancy::Service::start_resources_with_continuations(
            h.fixture.db.clone(),
            None,
            Default::default(),
            snapshot,
            Arc::new(crate::tenancy::resources::Resources::with_key(
                h.fixture.installation.clone(),
                [29; 32],
            )?),
            (
                None,
                &crate::config::ContinuationConfig::default(),
                None,
                sink2.clone(),
            ),
            None,
        )
        .await?;
        let observed = restarted
            .admission
            .observed_capacity()
            .map_err(|_| anyhow::anyhow!("restart snapshot"))?;
        ensure!(observed.accounted == bytes && observed.live_unknown == u64::from(unknown));
        super::escalation::restored_unknown_keeps_evidence(&h, &restarted, &receipt, bytes).await?;
        restarted.shutdown().await?;
        // No new selected execution: timing seal must still fail, even with a real clean capacity seal.
        ensure!(!sink2.seal(true) && flush(&exporter2).await?);
        assert_frozen(&second.view()?, bytes, bytes, u64::from(unknown))?;
        ensure!(second.view()?.event("sealed_failed") == 1);
        second.close().await?;
    }
    Ok(())
}
#[cfg(unix)]
pub(super) async fn sealed_gauges_do_not_retain_file_owner() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("authority.db");
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 3_600_000, None)?;
    let h = Harness::with_observed_storage(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        None,
        false,
        (sink.clone(), Some(&path)),
    )
    .await?;
    completed(&h).await?;
    let bytes = stored_policy_bytes(&h).await?;
    ensure!(crate::tenancy::ownership::acquire_path(&path, false).is_err());
    h.fixture.service.shutdown().await?;
    ensure!(sink.seal(true));
    // Await both retained socket owners before dropping the actual Service/database.
    h._gateway.abort();
    h._provider.abort();
    ensure!(h._gateway.await.is_err_and(|e| e.is_cancelled()));
    ensure!(h._provider.await.is_err_and(|e| e.is_cancelled()));
    drop(h.fixture.service);
    h.fixture.db.close().await?;
    let released = crate::tenancy::ownership::acquire_path(&path, false)?;
    // Export only after the strong owner is gone; immutable numeric data remains.
    ensure!(sink.seal(false) && flush(&exporter).await?);
    assert_frozen(&collector.view()?, 0, bytes, 0)?;
    drop(released);
    collector.close().await
}

pub(super) async fn queued_cancel_and_unknown_length_ingress() -> Result<()> {
    use crate::tenancy::{command::Command, store::StoredResponse};
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 100, None)?;
    let provider_gate = Arc::new(Semaphore::new(0));
    let mut h = harness(sink.clone(), Some(provider_gate.clone())).await?;
    let mut input = h.fixture.configuration_input("capacity-queue")?;
    input.document.routes.push(h.route.clone());
    input.document.limits.active_executions = 1;
    input.document.limits.queued_work = 1;
    h.revision = match h
        .fixture
        .service
        .execute(
            h.fixture.operator.clone(),
            Command::Configuration(h.fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(revision) => revision,
        _ => anyhow::bail!("configuration"),
    };
    h.headers = headers(
        &h.fixture,
        h.credential.secret.as_deref().context("token")?,
        &h.revision.revision_id,
    )?;
    let first = h.send(&Harness::client(false)?);
    let first = AbortOnDropHandle::new(tokio::spawn(async move {
        let r = first.send().await?;
        ensure!(r.status() == StatusCode::OK);
        let _ = r.bytes().await?;
        Ok::<_, anyhow::Error>(())
    }));
    tokio::time::timeout(TIMEOUT, h.received.recv())
        .await?
        .context("actual first provider send")?;
    let router = || {
        Router::new()
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(
                h.fixture.service.clone(),
                crate::tenancy::http::dispatch,
            ))
    };
    let request = |body| -> Result<Request<Body>> {
        let mut r = Request::post(h.path)
            .header("content-type", "application/json")
            .body(body)?;
        r.headers_mut().extend(h.headers.clone());
        Ok(r)
    };
    let queued = AbortOnDropHandle::new(tokio::spawn(
        router().oneshot(request(Body::from(serde_json::to_vec(&h.request)?))?),
    ));
    wait_current(&collector, "queued", 1).await?;
    queued.abort();
    ensure!(queued.await.is_err_and(|e| e.is_cancelled()));
    wait_current(&collector, "queued", 0).await?;
    let before = h
        .fixture
        .service
        .admission
        .observed_capacity()
        .map_err(|_| anyhow::anyhow!("snapshot"))?;
    let polled = Arc::new(tokio::sync::Notify::new());
    let signal = polled.clone();
    let body = Body::from_stream(async_stream::stream! {
        signal.notify_one(); std::future::pending::<()>().await;
        yield Ok::<_,std::io::Error>(axum::body::Bytes::new());
    });
    let pending = AbortOnDropHandle::new(tokio::spawn(router().oneshot(request(body)?)));
    tokio::time::timeout(TIMEOUT, polled.notified()).await?;
    wait_current(
        &collector,
        "ingress_reserved",
        before.ingress_reserved + 16 * 1024 * 1024,
    )
    .await?;
    pending.abort();
    ensure!(pending.await.is_err_and(|e| e.is_cancelled()));
    wait_current(&collector, "ingress_reserved", before.ingress_reserved).await?;
    ensure!(h.calls.load(Ordering::SeqCst) == 1);
    provider_gate.add_permits(1);
    tokio::time::timeout(TIMEOUT, first).await???;
    h.fixture.service.shutdown().await?;
    ensure!(sink.seal(true) && flush(&exporter).await?);
    let view = collector.view()?;
    ensure!(view.event("admitted") == 1 && view.event("sql_ack") == 1);
    assert_frozen(&view, 0, stored_policy_bytes(&h).await?, 0)?;
    collector.close().await
}

pub(super) async fn aggregate_cross_tenant_capacity() -> Result<()> {
    use crate::tenancy::{command::Command, store::StoredResponse};
    use bitrouter_ai::tenancy::authority::{
        BindingPut, GrantScope, TenantPut, TenantRole, TenantState,
    };
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 3_600_000, Some("same-correlation"))?;
    let h = Harness::with_observer(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        None,
        true,
        sink.clone(),
    )
    .await?;
    let tenant = "tenant-b".try_into()?;
    h.fixture
        .service
        .execute(
            h.fixture.operator.clone(),
            Command::Tenant(
                tenant,
                TenantPut {
                    operation_id: "capacity-second-tenant".try_into()?,
                    expected_revision: None,
                    state: TenantState::Active,
                    installation_id: h.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    let tenant: bitrouter_ai::tenancy::identity::TenantId = "tenant-b".try_into()?;
    let binding_result = match h
        .fixture
        .service
        .execute(
            h.fixture.operator.clone(),
            Command::Binding(
                tenant.clone(),
                h.fixture.binding.clone(),
                BindingPut {
                    operation_id: "capacity-second-binding".try_into()?,
                    expected_revision: None,
                    principal_id: h.fixture.principal.clone(),
                    role: TenantRole::TenantAdmin,
                    scope: GrantScope::Tenant {
                        operator_pools: vec![],
                    },
                    expires_at: crate::tenancy::tests::expiry()?,
                    expected_management_owner: None,
                    installation_id: h.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Operation(result) => result,
        _ => anyhow::bail!("binding"),
    };
    let second = Fixture {
        service: h.fixture.service.clone(),
        db: h.fixture.db.clone(),
        operator: h.fixture.operator.clone(),
        installation: h.fixture.installation.clone(),
        tenant,
        principal: h.fixture.principal.clone(),
        binding: h.fixture.binding.clone(),
        binding_result,
    };
    let mut route = h.route.clone();
    route.owner_id = second.tenant.as_str().try_into()?;
    let revision = draft(&second, &route, "capacity-second-config", 25).await?;
    let credential = second.issue("capacity-second-client").await?;
    let client = Harness::client(false)?;
    for (fixture, token, revision) in [
        (
            &h.fixture,
            h.credential.secret.as_deref().context("token")?,
            &h.revision.revision_id,
        ),
        (
            &second,
            credential.secret.as_deref().context("second token")?,
            &revision.revision_id,
        ),
    ] {
        let response = client
            .post(format!("{}{}", h.base, h.path))
            .headers(headers(fixture, token, revision)?)
            .json(&h.request)
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let id = execution_id(&response)?;
        let _ = response.bytes().await?;
        let receipt = store::lookup(&fixture.db, &fixture.installation, &fixture.tenant, &id)
            .await?
            .context("actual tenant receipt")?;
        ensure!(
            receipt.tenant_id == fixture.tenant
                && receipt.state == ReceiptState::Committed
                && receipt.outcome == Outcome::Completed
                && receipt.usage.complete
                && receipt.attempts.len() == 1
                && receipt.correlation_id.as_str() == "same-correlation"
        );
    }
    ensure!(h.calls.load(Ordering::SeqCst) == 2);
    h.fixture.service.shutdown().await?;
    ensure!(sink.seal(true) && flush(&exporter).await?);
    let view = collector.view()?;
    ensure!(
        view.event("admitted") == 2
            && view.event("first_queued") == 2
            && view.event("sql_ack") == 2
            && view.count("durable:complete_completed") == 2
            && view.event("sealed_clean") == 1
            && view.event("sealed_failed") == 0
    );
    assert_frozen(&view, 0, stored_policy_bytes(&h).await?, 0)?;
    collector.close().await
}
