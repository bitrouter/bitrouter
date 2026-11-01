use super::*;
use bitrouter_ai::tenancy::operation::ErrorCode;
use sea_orm::ConnectionTrait;
use std::sync::{Arc, OnceLock};

#[tokio::test]
async fn shared_budget_reaches_control_block_selection_and_checkpoint_owners() -> Result<()> {
    let (service, canonical) = setup().await?;
    let identity = new_session(&canonical, "physical-budget").await?;
    let head = canonical.transcript(&identity).await?.session.head;
    let db = service.store.db.clone();
    let cell = Arc::new(OnceLock::new());
    let service = service.with_storage_budget(Some(cell.clone()));
    let canonical = canonical.with_storage_budget(Some(cell.clone()));
    let error = service
        .set_mode(EvolutionMode::Manual, None)
        .await
        .err()
        .context("uninitialized control budget accepted")?;
    assert!(error.to_string().contains("local storage budget not ready"));
    assert!(canonical.freeze_checkpoint(&identity, head).await.is_err());
    assert!(canonical.checkpoints(&identity).await?.is_empty());
    assert!(
        service
            .store
            .get::<ControlState>(CONTROL_KIND, CONTROL_KEY)
            .await?
            .is_none()
    );

    let tenancy = crate::tenancy::Service::open(db, None).await?;
    cell.set(tenancy.storage_budget())
        .map_err(|_| anyhow::anyhow!("cell already set"))?;
    service
        .register(definition("a", "primary"), "routes-a".into())
        .await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let first = service
        .select(
            &identity,
            "budget-request",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("selection missing")?;
    let replay = service
        .select(
            &identity,
            "budget-request",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("selection replay missing")?;
    assert_eq!(serde_json::to_value(first)?, serde_json::to_value(replay)?);
    let checkpoint = canonical.freeze_checkpoint(&identity, head).await?;
    assert_eq!(
        canonical
            .freeze_checkpoint(&identity, head)
            .await?
            .checkpoint_id,
        checkpoint.checkpoint_id
    );
    // A rejected replay explicitly rolls back its lock writes without fencing.
    assert!(
        service
            .select(
                &identity,
                "budget-request",
                context("different"),
                &dependencies()
            )
            .await
            .is_err()
    );
    assert!(tenancy.storage_budget().reserve(0, false).is_ok());
    tenancy.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn control_sql_failure_rolls_back_and_fences_shared_storage() -> Result<()> {
    let (service, _) = setup().await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let db = service.store.db.clone();
    let tenancy = crate::tenancy::Service::open(db.clone(), None).await?;
    let cell = Arc::new(OnceLock::new());
    cell.set(tenancy.storage_budget())
        .map_err(|_| anyhow::anyhow!("cell already set"))?;
    let service = service.with_storage_budget(Some(cell));
    db.execute_unprepared("CREATE TRIGGER reject_control_update BEFORE UPDATE ON checkpoint_evolution BEGIN SELECT RAISE(ABORT, 'fixture storage failure'); END").await?;
    let error = service
        .set_mode(EvolutionMode::Off, None)
        .await
        .err()
        .context("SQL failure accepted")?;
    assert!(error.to_string().contains("local storage SQL failed"));
    assert_eq!(service.state().await?.mode, EvolutionMode::Manual);
    assert!(matches!(
        tenancy.storage_budget().reserve(0, false),
        Err(ErrorCode::StorageUnavailable)
    ));
    tenancy.shutdown().await?;
    Ok(())
}
