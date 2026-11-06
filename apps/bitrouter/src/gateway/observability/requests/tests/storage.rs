use super::*;
use crate::gateway::observability::inventory::{
    COVERAGE_KIND, CoverageRegistration, INVENTORY_KIND,
};
use anyhow::ensure;
use bitrouter_ai::tenancy::operation::ErrorCode;
use futures::FutureExt;
use sea_orm::{ConnectionTrait, TransactionTrait};

#[tokio::test]
async fn shared_cell_refuses_before_initialization_then_serves_hooks_and_daemon_clone() -> Result<()>
{
    let cell = Arc::new(OnceLock::new());
    let fixture = fixture_with_storage(Failure::None, Some(cell.clone())).await?;
    let daemon_inventory = fixture.tracker.inventory();
    // No initialized Budget means no mutating BEGIN, even for a cloned port.
    let held = fixture.db.begin().await?;
    ensure!(
        daemon_inventory
            .register_capture("absent", "scope", "controller")
            .now_or_never()
            .context("budget refusal waited for the occupied SQL pool")?
            .err()
            .context("uninitialized registration accepted")?
            .to_string()
            == "internal error: local storage budget not ready"
    );
    held.rollback().await?;
    ensure!(
        fixture
            .pipeline
            .execute(request("not-ready", true)?)
            .await
            .is_err()
    );
    ensure!(
        daemon_inventory
            .scoped_store("scope", "controller")?
            .get::<GatewayRequest>(INVENTORY_KIND, "not-ready")
            .await?
            .is_none()
    );

    let service = crate::tenancy::Service::open(fixture.db.clone(), None).await?;
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget already initialized"))?;
    // register_capture uses the same clone that existed before publication.
    connections::ActiveModel {
        connection_id: Set("new-capture".into()),
        owner: Set("owner".into()),
        source: Set("fixture".into()),
        controller_instance_id: Set(Some("new-controller".into())),
        route_scope_id: Set(Some("scope".into())),
        state: Set("recording".into()),
        head: Set(0),
        started_at: Set("fixture".into()),
        metadata_json: Set("{}".into()),
    }
    .insert(&fixture.db)
    .await?;
    daemon_inventory
        .register_capture("new-capture", "scope", "new-controller")
        .await?;
    fixture.pipeline.execute(request("budgeted", true)?).await?;
    fixture
        .pipeline
        .drain_required_pending_settlements()
        .await?;
    let inventory = saved(&fixture, "budgeted").await?;
    let bound = execution(&fixture, "budgeted").await?;
    ensure!(inventory.attempt_id == bound.attempt_id && inventory.settlement == bound.settlement);
    ensure!(
        bound.settlement.context("terminal missing")?.outcome == Some(ExecutionOutcome::Completed)
    );
    ensure!(
        fixture
            .pipeline
            .execute(request("budgeted", true)?)
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
async fn canonical_binding_sql_failure_fences_the_shared_tenant_budget() -> Result<()> {
    let cell = Arc::new(OnceLock::new());
    let fixture = fixture_with_storage(Failure::None, Some(cell.clone())).await?;
    let service = crate::tenancy::Service::open(fixture.db.clone(), None).await?;
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget already initialized"))?;
    fixture.db.execute_unprepared(
        "CREATE TRIGGER binding_write_fault BEFORE INSERT ON checkpoint_evolution WHEN NEW.kind = 'execution' BEGIN SELECT RAISE(ABORT, 'private-observation-payload'); END"
    ).await?;
    let error = fixture
        .pipeline
        .execute(request("binding-failed", true)?)
        .await
        .err()
        .context("binding write unexpectedly succeeded")?;
    ensure!(!error.to_string().contains("private-observation-payload"));
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable)
    ));
    ensure!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests absent")?
            .is_empty()
    );
    ensure!(
        EvolutionStore::new(fixture.db.clone(), &fixture.identity.owner)?
            .get::<ExecutionRecord>(EXECUTION_KIND, "binding-failed")
            .await?
            .is_none()
    );
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn canonical_initialization_requires_the_shared_budget_before_sql() -> Result<()> {
    let fixture = fixture(Failure::None).await?;
    let binding = fixture
        .binding
        .clone()
        .with_storage_budget(Arc::new(OnceLock::new()));
    let mut context = PipelineContext::new(request("binding-not-ready", true)?);
    NormalizedScope.check(&mut context).await?;
    fixture.tracker.check(&mut context).await?;
    let error = binding
        .check(&mut context)
        .await
        .err()
        .context("binding accepted without budget")?;
    ensure!(error.to_string() == "internal error: local storage budget not ready");
    ensure!(
        EvolutionStore::new(fixture.db.clone(), &fixture.identity.owner)?
            .get::<ExecutionRecord>(EXECUTION_KIND, "binding-not-ready")
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn settlement_sql_failure_rolls_back_coverage_and_fences_storage() -> Result<()> {
    let cell = Arc::new(OnceLock::new());
    let fixture = fixture_with_storage(Failure::None, Some(cell.clone())).await?;
    let service = crate::tenancy::Service::open(fixture.db.clone(), None).await?;
    cell.set(service.storage_budget())
        .map_err(|_| anyhow::anyhow!("budget already initialized"))?;
    let inventory = fixture.tracker.inventory();
    let observed = match inventory
        .begin("scope", "controller", "settlement-failed", None)
        .await?
    {
        BeginResult::Tracked(value) => value,
        _ => anyhow::bail!("request was not admitted"),
    };
    let store = EvolutionStore::new(fixture.db.clone(), &fixture.identity.owner)?;
    let before = store
        .get::<CoverageRegistration>(COVERAGE_KIND, "capture")
        .await?
        .context("coverage missing")?;
    fixture.db.execute_unprepared(
        "CREATE TRIGGER settlement_write_fault BEFORE UPDATE ON checkpoint_evolution WHEN OLD.kind = 'gateway_request' BEGIN SELECT RAISE(ABORT, 'private-settlement-payload'); END"
    ).await?;
    let settlement = ExecutionSettlement {
        settled_at: chrono::Utc::now().to_rfc3339(),
        hops: Vec::new(),
        final_model: "fixture-model".into(),
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
    let after = store
        .get::<CoverageRegistration>(COVERAGE_KIND, "capture")
        .await?
        .context("coverage disappeared")?;
    ensure!(before.0 == after.0, "failed transaction advanced coverage");
    ensure!(
        saved(&fixture, "settlement-failed")
            .await?
            .settlement
            .is_none()
    );
    ensure!(matches!(
        service.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable)
    ));
    service.shutdown().await?;
    Ok(())
}
