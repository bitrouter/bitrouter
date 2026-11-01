use super::*;

#[tokio::test]
async fn row_pages_and_byte_inventory_cross_former_catalog_limit_without_collecting_rows()
-> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let installation: InstallationId = "installation".try_into()?;
    let tenant = "tenant".try_into()?;
    let execution = "execution".try_into()?;
    // A regression checkpoint beyond the retired catalog quota, not a new maximum.
    let count = 273_u64;
    let tx = db.begin().await?;
    let mut expected = Vec::new();
    for i in 0..count {
        let (_, row) = pending::tests::fixture(
            &installation,
            &tenant,
            &execution,
            &format!("slot-{i}"),
            "2026-10-01T00:00:00Z",
        )?;
        insert(&tx, &row).await?;
        expected.push(row.metadata.slot);
    }
    tx.commit().await?;
    expected.sort();
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let batch = super::super::page(&db, &installation, cursor.as_deref()).await?;
        if batch.is_empty() {
            break;
        }
        ensure!(batch.len() <= 32);
        for row in batch {
            cursor = Some(row.metadata.slot.clone());
            seen.push(row.metadata.slot);
        }
    }
    ensure!(seen == expected);
    ensure!(
        inventory(&db, &installation).await?
            == count * (profile::PAYLOAD_BYTES + META_BYTES + 32 * 1024) as u64
    );
    ensure!(
        super::super::page(&db, &installation, cursor.as_deref())
            .await?
            .is_empty()
    );
    // The test-only witness also traverses complete pages beyond the former quota.
    ensure!(pending_unknown(&db, &installation).await?.len() as u64 == count);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn row_page_rejects_foreign_installation_and_oversized_payload() -> Result<()> {
    for foreign in [false, true] {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let installation: InstallationId = "installation".try_into()?;
        let stored = if foreign {
            "foreign".try_into()?
        } else {
            installation.clone()
        };
        let (_, row) = pending::tests::fixture(
            &stored,
            &"tenant".try_into()?,
            &"execution".try_into()?,
            "slot",
            "2026-10-01T00:00:00Z",
        )?;
        insert(&db, &row).await?;
        if !foreign {
            db.execute_unprepared(&format!(
                "UPDATE tenancy_oauth_slots SET payload_json=zeroblob({})",
                profile::PAYLOAD_BYTES + 1
            ))
            .await?;
        }
        ensure!(super::super::page(&db, &installation, None).await.is_err());
        db.close().await?;
    }
    Ok(())
}
