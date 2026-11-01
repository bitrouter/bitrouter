//! Ungated production startup, twice, after actual process containment or known ACK.
use super::*;

async fn unknown_first(
    db: &DatabaseConnection,
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
) -> Result<
    std::collections::BTreeMap<
        crate::tenancy::receipts::store::obligations::Key,
        chrono::DateTime<chrono::Utc>,
    >,
> {
    use crate::tenancy::receipts::store::obligations;
    let mut records = std::collections::BTreeMap::new();
    let mut after = None;
    loop {
        let page = obligations::page(db, installation, after.as_ref(), &|_, _| Ok(())).await?;
        if page.is_empty() {
            return Ok(records);
        }
        for record in page {
            after = Some(record.key.clone());
            ensure!(records.insert(record.key, record.first).is_none());
        }
    }
}

#[tokio::test]
#[ignore = "internal normal native paid restart; select the bounded fault parent"]
async fn consumer() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("native repair root")?);
    let original: Value = state::read_json(&root.join("accepted.json"))?;
    let identity = key(&original)?;
    let baseline: state::Snapshot = state::read_json(&root.join("repair-before.json"))?;
    let database = crate::db::connect_owned_tenancy(&url(&root)).await?;
    let db = database.database().clone();
    let checked = async {
        // Remove only this fault campaign's objects after obtaining a new real
        // exclusive claim. No record, receipt, seed, source or proof is changed.
        for sql in [
            "DROP TRIGGER IF EXISTS native_paid_repair_rollback",
            "DROP TRIGGER IF EXISTS native_paid_repair_commit_fault",
            "DROP TABLE IF EXISTS native_paid_repair_fk_child",
            "DROP TABLE IF EXISTS native_paid_repair_fk_parent",
        ] {
            db.execute_unprepared(sql).await?;
        }
        let retained = root.join("repair-after.json");
        let previous: Option<state::Snapshot> = if retained.exists() {
            Some(state::read_json(&retained)?)
        } else {
            None
        };
        let before = state::snapshot(&db).await?;
        if let Some(previous) = &previous {
            ensure!(
                &before == previous,
                "second opener rewrote known terminal before startup"
            );
        } else {
            ensure!(
                before == baseline,
                "unknown fault changed original durable source"
            );
        }
        let inventory_before =
            crate::tenancy::receipts::store::inventory(&db, &identity.installation).await?;
        let original_unknown = unknown_first(&db, &identity.installation).await?;
        let service = Service::open_owned_profile(
            database,
            &config(),
            &crate::config::ContinuationConfig::default(),
        )
        .await?;
        let result = async {
            let after = state::snapshot(&db).await?;
            facts::interrupted(&baseline, &after, &original)?;
            let terminal = facts::terminal(&db, &original).await?;
            if let Some(previous) = &previous {
                ensure!(
                    &after == previous,
                    "second normal startup repaired or rewrote the same receipt twice"
                );
            } else {
                state::write_json(&retained, &after)?;
            }
            let inventory_after =
                crate::tenancy::receipts::store::inventory(&db, &identity.installation).await?;
            let added = u64::from(previous.is_none());
            ensure!(
                inventory_after.rows == inventory_before.rows + added
                    && inventory_after.bytes
                        == inventory_before.bytes + if added == 1 { terminal.bytes() } else { 0 }
            );
            ensure!(inventory_after.unknown_count == inventory_before.unknown_count + added);
            let actual_unknown = unknown_first(&db, &identity.installation).await?;
            ensure!(
                original_unknown
                    .iter()
                    .all(|(key, at)| actual_unknown.get(key) == Some(at))
                    && actual_unknown.contains_key(
                        &crate::tenancy::receipts::store::obligations::Key {
                            tenant: identity.tenant.clone(),
                            execution: identity.execution.clone(),
                        }
                    )
            );
            // Existing production M5 read-only observation, including real repair
            // settlement into accounted bytes; absent in reduced telemetry builds.
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            {
                let expected = inventory_after.live_bytes;
                let snapshot = tokio::time::timeout(TIMEOUT, async {
                    loop {
                        match service.admission.observed_capacity() {
                            Ok(value) => break Ok::<_, anyhow::Error>(value),
                            Err(
                                crate::telemetry::receipt_latency::capacity::ReadError::Unavailable,
                            ) => tokio::task::yield_now().await,
                            Err(error) => {
                                anyhow::bail!("invalid native repair M5 observation: {error:?}")
                            }
                        }
                    }
                })
                .await??;
                ensure!(
                    snapshot.accounted == expected
                        && snapshot.live_unknown == inventory_after.unknown_count
                        && snapshot.completion_reserved == 0
                        && snapshot.potential_unknown == 0
                        && snapshot.active == 0
                        && snapshot.queued == 0
                        && !snapshot.fenced
                );
            }
            service.cleanup.ensure_clean()?;
            ensure!(
                service.current().is_ok()
                    && service
                        .admission
                        .receipt_capacity_for_test()
                        .map_err(|e| anyhow::anyhow!("{e:?}"))?
                        .0
                        == 0
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let stopped = service.shutdown().await;
        result.and(stopped)
    }
    .await;
    let closed = db.close().await;
    checked.and(closed.map_err(Into::into))?;
    released(&root).await?;
    signal("native-paid-repair-fresh-clean")
}
