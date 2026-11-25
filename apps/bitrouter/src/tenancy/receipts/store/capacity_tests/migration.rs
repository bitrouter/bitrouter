use super::*;

pub(super) async fn upgrade(fixture: &mut Fixture) -> Result<()> {
    crate::db::migration::provider_technical::install(&fixture.db).await?;
    let tx = fixture.legacy_seed().await?;
    let expected = policy::mixed_sources(&tx).await?;
    fixture.finish_legacy_seed(tx).await?;
    // The old catalog, old marker and retained sources survive a fresh actual
    // owner before the production migrator publishes the new native format.
    fixture.reopen().await?;
    ensure!(store_format::inspect(&fixture.db).await? == store_format::State::Current);
    ensure!(expected == quantities(&matches_audit(&fixture.db).await?));
    let original = unknown("pending")?;
    let backend = fixture.db.get_database_backend();
    let damaged = Query::update()
        .table(Alias::new(TABLE))
        .value(Alias::new("receipt_json"), "{")
        .and_where(Expr::col(Alias::new("execution_id")).eq("pending"))
        .to_owned();
    fixture.db.execute(backend.build(&damaged)).await?;
    for _ in 0..2 {
        fixture.reopen().await?;
        ensure!(
            expected == quantities(&capacity::load(&fixture.db).await?),
            "mature restart refilled or changed policy quantities"
        );
        ensure!(
            inventory(&fixture.db, &"installation".try_into()?)
                .await
                .is_err(),
            "explicit audit ignored damaged retained history"
        );
    }
    let restore = Query::update()
        .table(Alias::new(TABLE))
        .value(Alias::new("receipt_json"), original.json)
        .and_where(Expr::col(Alias::new("execution_id")).eq("pending"))
        .to_owned();
    fixture.db.execute(backend.build(&restore)).await?;
    ensure!(expected == quantities(&matches_audit(&fixture.db).await?));
    Ok(())
}

pub(super) async fn failed_upgrade(fixture: &mut Fixture) -> Result<()> {
    let value = auxiliary_candidate("execution")?;
    let tx = fixture.legacy_seed().await?;
    ensure!(persist(&tx, &value).await?);
    fixture.finish_legacy_seed(tx).await?;
    update(&fixture.db, TABLE, "content_digest", "bad").await?;
    // Failure occurs after the migrator has started its real DDL/data
    // transaction. Rollback must leave the exact old catalog/marker retryable;
    // this is an interrupted upgrade contract, not a killed-process claim.
    ensure!(fixture.migrate().await.is_err());
    ensure!(
        !sea_orm_migration::SchemaManager::new(&fixture.db)
            .has_table(capacity::TABLE)
            .await?
    );
    ensure!(store_format::inspect(&fixture.db).await? == store_format::State::Legacy);
    if fixture.db.get_database_backend() == DatabaseBackend::Postgres {
        crate::db::current_schema::validate_legacy(&fixture.db).await?;
    }
    update(&fixture.db, TABLE, "content_digest", value.digest.clone()).await?;
    fixture.reopen().await?;
    matches_audit(&fixture.db).await?;
    let saved = lookup_candidate(
        &fixture.db,
        &value.receipt.installation_id,
        &value.receipt.tenant_id,
        &value.receipt.execution_id,
    )
    .await?
    .context("source after retried upgrade")?;
    same_completion(&saved, &value)?;
    Ok(())
}

pub(super) async fn foreign_sources(fixture: &mut Fixture) -> Result<()> {
    let tx = fixture.legacy_seed().await?;
    let value = auxiliary_candidate("execution")?;
    ensure!(persist(&tx, &value).await?);
    let prepared = archive::Prepared::new(&value, &|_, _| Ok(()))?;
    ensure!(archive::archive_one(&tx, &prepared, &|_, _| Ok(())).await?);
    fixture.finish_legacy_seed(tx).await?;
    for foreign in [false, true] {
        if foreign {
            update(
                &fixture.db,
                "tenancy_installation",
                "installation_id",
                "foreign",
            )
            .await?;
        } else {
            let delete = Query::delete()
                .from_table(Alias::new("tenancy_installation"))
                .to_owned();
            fixture
                .db
                .execute(fixture.db.get_database_backend().build(&delete))
                .await?;
        }
        ensure!(
            fixture.migrate().await.is_err(),
            "legacy data was zeroed without its original installation"
        );
        ensure!(
            !sea_orm_migration::SchemaManager::new(&fixture.db)
                .has_table(capacity::TABLE)
                .await?
        );
        ensure!(store_format::inspect(&fixture.db).await? == store_format::State::Legacy);
        if foreign {
            update(
                &fixture.db,
                "tenancy_installation",
                "installation_id",
                "installation",
            )
            .await?;
        } else {
            Fixture::header(&fixture.db).await?;
        }
    }
    fixture.reopen().await?;
    ensure!(matches_audit(&fixture.db).await?.history_rows == 1);
    Ok(())
}

pub(super) async fn epoch(fixture: &mut Fixture) -> Result<()> {
    let tx = fixture.legacy_seed().await?;
    ensure!(!capacity::epoch_required(&tx).await?);
    capacity::mark_epoch_required(&tx).await?;
    ensure!(capacity::epoch_required(&tx).await?);
    tx.rollback().await?;
    ensure!(
        !sea_orm_migration::SchemaManager::new(&fixture.db)
            .has_table(capacity::TABLE)
            .await?
    );
    let tx = fixture.legacy_seed().await?;
    ensure!(!capacity::epoch_required(&tx).await?);
    let epoch = Query::insert()
        .into_table(Alias::new("tenancy_continuation_epoch"))
        .columns(["singleton", "installation_id", "key_id", "created_at"].map(Alias::new))
        .values([
            1_i32.into(),
            "installation".into(),
            "key".into(),
            "2026-10-25T00:00:00Z".into(),
        ])?
        .to_owned();
    tx.execute(tx.get_database_backend().build(&epoch)).await?;
    fixture.finish_legacy_seed(tx).await?;
    fixture.reopen().await?;
    ensure!(capacity::epoch_required(&fixture.db).await?);
    let delete = Query::delete()
        .from_table(Alias::new("tenancy_continuation_epoch"))
        .to_owned();
    fixture
        .db
        .execute(fixture.db.get_database_backend().build(&delete))
        .await?;
    for _ in 0..2 {
        fixture.reopen().await?;
        ensure!(
            capacity::epoch_required(&fixture.db).await?,
            "mature reopen cleared the required-key epoch projection"
        );
    }
    Ok(())
}
