use super::*;
use anyhow::ensure;
use bitrouter_ai::tenancy::operation::ErrorCode;
use futures::FutureExt;
use sea_orm::TransactionTrait;

#[tokio::test]
async fn shared_cell_refuses_before_initialization_then_serves_hooks_and_daemon_clone() -> Result<()>
{
    let cell = Arc::new(OnceLock::new());
    let fixture = fixture_with_storage(Failure::None, Some(cell.clone())).await?;
    let inventory = fixture.tracker.inventory();
    let held = fixture.db.begin().await?;
    let error = inventory
        .begin("scope", "controller", "not-ready")
        .now_or_never()
        .context("waited for SQL before budget")?
        .err()
        .context("unpublished budget accepted")?;
    ensure!(error.to_string() == "internal error: local storage budget not ready");
    held.rollback().await?;
    ensure!(
        fixture
            .pipeline
            .execute(request("not-ready", true)?)
            .await
            .is_err()
    );
    ensure!(inventory.request("scope", "not-ready").await?.is_none());
    let service = crate::tenancy::Service::open(fixture.db.clone(), None).await?;
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget set twice"))?;
    fixture.pipeline.execute(request("budgeted", true)?).await?;
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    ensure!(
        inventory
            .request("scope", "budgeted")
            .await?
            .context("missing")?
            .settlement
            .context("terminal")?
            .outcome
            == Some(ExecutionOutcome::Completed)
    );
    ensure!(
        fixture
            .pipeline
            .execute(request("budgeted", false)?)
            .await
            .is_err()
    );
    service
        .storage_budget()
        .reserve(0, false)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?
        .complete()
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn reservation_insert_failure_fences_storage_before_any_provider_request() -> Result<()> {
    let cell = Arc::new(OnceLock::new());
    let fixture = fixture_with_storage(Failure::None, Some(cell.clone())).await?;
    let service = crate::tenancy::Service::open(fixture.db.clone(), None).await?;
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget set twice"))?;
    fixture.db.execute_unprepared("CREATE TRIGGER test_inventory_insert BEFORE INSERT ON gateway_request_records WHEN NEW.kind='gateway_request' BEGIN SELECT RAISE(ABORT,'private-observation-payload'); END").await?;
    let error = fixture
        .pipeline
        .execute(request("fault", true)?)
        .await
        .err()
        .context("insert error accepted")?;
    ensure!(!error.to_string().contains("private-observation-payload"));
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable)
    ));
    ensure!(
        fixture
            .tracker
            .inventory()
            .request("scope", "fault")
            .await?
            .is_none()
    );
    ensure!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("mock")?
            .is_empty()
    );
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn settlement_sql_failure_preserves_original_reservation_and_fences_storage() -> Result<()> {
    let cell = Arc::new(OnceLock::new());
    let fixture = fixture_with_storage(Failure::None, Some(cell.clone())).await?;
    let service = crate::tenancy::Service::open(fixture.db.clone(), None).await?;
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget set twice"))?;
    let inventory = fixture.tracker.inventory();
    let BeginResult::Tracked(observed) = inventory.begin("scope", "controller", "fault").await?
    else {
        anyhow::bail!("missing admission");
    };
    let before = serde_json::to_value(&observed)?;
    fixture.db.execute_unprepared("CREATE TRIGGER test_inventory_update BEFORE UPDATE ON gateway_request_records WHEN OLD.kind='gateway_request' BEGIN SELECT RAISE(ABORT,'private-settlement-payload'); END").await?;
    let settlement = ExecutionSettlement {
        settled_at: chrono::Utc::now().to_rfc3339(),
        hops: Vec::new(),
        final_model: "model".into(),
        final_provider: "fixture".into(),
        error_code: None,
        duration_ms: 1,
        outcome: None,
        final_metered_cost_micro_usd: None,
        total_cost_micro_usd: None,
    };
    let error = inventory
        .settle(&observed, settlement)
        .await
        .err()
        .context("settlement accepted")?;
    ensure!(error.to_string() == "internal error: local storage SQL failed");
    assert_eq!(
        serde_json::to_value(saved(&fixture, "fault").await?)?,
        before
    );
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable)
    ));
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_requests_do_not_need_the_inventory_or_its_budget() -> Result<()> {
    let fixture = fixture_with_storage(Failure::None, Some(Arc::new(OnceLock::new()))).await?;
    fixture
        .db
        .execute_unprepared("DROP TABLE gateway_request_records")
        .await?;
    let mut ordinary = request("ordinary", true)?;
    let id = crate::gateway::pipeline::types::new_request_id();
    ordinary.request_id = id.clone();
    ordinary.extensions.insert(Arc::new(
        crate::gateway::pipeline::types::AllocatedRequestId::new(id.clone()),
    ));
    ordinary.headers.remove("x-bitrouter-controller-id");
    fixture.pipeline.execute(ordinary).await?;
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("mock")?
            .len(),
        1
    );
    ensure!(
        crate::metering::entities::requests::Entity::find_by_id(&id)
            .one(&fixture.db)
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn a_controller_claim_without_authenticated_scope_cannot_reserve_a_paid_id() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    let mut unsigned = PipelineContext::new(request("spoofed", true)?);
    let mut normalized = PipelineContext::new(request("other", true)?);
    NormalizedScope.check(&mut normalized).await?;
    unsigned.insert_extension(
        normalized
            .extension::<RequestSessionContext>()
            .context("fixture session")?,
    );
    ensure!(fixture.tracker.check(&mut unsigned).await.is_err());
    ensure!(
        unsigned
            .get_event::<CanonicalRequestReplayRejected>()
            .is_some()
    );
    ensure!(
        fixture
            .tracker
            .inventory()
            .request("scope", "spoofed")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn an_invalid_controller_header_cannot_fall_through_to_untracked_dispatch() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    let inventory = fixture.tracker.inventory();
    for (id, header) in [("blank", &b" "[..]), ("invalid", &[0xff][..])] {
        let mut tagged = request(id, true)?;
        tagged.headers.insert(
            "x-bitrouter-controller-id",
            http::HeaderValue::from_bytes(header)?,
        );
        let error = fixture
            .pipeline
            .execute(tagged)
            .await
            .err()
            .context("invalid claim admitted")?;
        ensure!(error.status() == 400);
        ensure!(inventory.request("scope", id).await?.is_none());
        ensure!(
            crate::metering::entities::requests::Entity::find_by_id(id)
                .one(&fixture.db)
                .await?
                .is_none()
        );
    }
    ensure!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("mock")?
            .is_empty()
    );
    Ok(())
}
