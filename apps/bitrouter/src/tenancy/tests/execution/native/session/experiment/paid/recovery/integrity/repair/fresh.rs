//! Ungated production startup, twice, after actual process containment or known ACK.
use super::*;

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
            ensure!(
                inventory_after.unknown.len()
                    == inventory_before.unknown.len() + usize::try_from(added)?
            );
            let original_unknown: std::collections::BTreeMap<_, _> = inventory_before
                .unknown
                .iter()
                .map(|(t, e, at)| ((t.clone(), e.clone()), *at))
                .collect();
            let actual_unknown: std::collections::BTreeMap<_, _> = inventory_after
                .unknown
                .iter()
                .map(|(t, e, at)| ((t.clone(), e.clone()), *at))
                .collect();
            ensure!(
                original_unknown
                    .iter()
                    .all(|(key, at)| actual_unknown.get(key) == Some(at))
                    && actual_unknown
                        .contains_key(&(identity.tenant.clone(), identity.execution.clone()))
            );
            // Existing production M5 read-only observation, including real repair
            // settlement into accounted bytes; absent in reduced telemetry builds.
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            {
                let expected = inventory_after
                    .rows
                    .checked_mul(16 * 1024)
                    .and_then(|rows| rows.checked_add(inventory_after.bytes))
                    .context("native repair accounting overflow")?;
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
                        && snapshot.live_unknown == u64::try_from(inventory_after.unknown.len())?
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
