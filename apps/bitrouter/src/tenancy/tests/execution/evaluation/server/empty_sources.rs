//! Empty-source compatibility cannot hide even one retained server-side row.
use super::*;
use crate::tenancy::receipts::store::{archive::Charge, obligations};
use sea_orm::{
    ConnectionTrait, Value,
    sea_query::{Alias, Query},
};

const FREE: &Charge<'static> = &|_, _| Ok(());

pub(super) async fn check(database: &mut database::Fixture) -> Result<()> {
    let db = database.connect().await?;
    crate::db::run_migrations(&db).await?;
    for table in [
        "tenancy_execution_receipts",
        "tenancy_receipt_auxiliary",
        "tenancy_oauth_slots",
    ] {
        let slot = database.start(&db).await?;
        let result = async {
            let tx = database.tx(slot)?;
            empty(tx).await?;
            rejected(
                tx,
                &|_, _| anyhow::bail!("charge refused"),
                "charge refused",
            )
            .await?;
            populate(tx, table).await?;
            rejected(
                tx,
                FREE,
                &format!("server test source must be empty: {table}"),
            )
            .await
        }
        .await;
        database.rollback(slot).await?;
        result?;
    }
    let slot = database.start(&db).await?;
    let result = empty(database.tx(slot)?).await;
    database.rollback(slot).await?;
    result
}

async fn empty(tx: &DatabaseTransaction) -> Result<()> {
    let installation = "installation".try_into()?;
    let tenant = "tenant".try_into()?;
    ensure!(
        obligations::validate(tx, &installation, FREE).await? == obligations::Fold::new().finish()
    );
    ensure!(
        obligations::next_due(tx, &installation, FREE)
            .await?
            .is_none()
    );
    ensure!(obligations::tenant_count(tx, &installation, &tenant, FREE).await? == 0);
    Ok(())
}

async fn rejected(tx: &DatabaseTransaction, charge: &Charge<'_>, expected: &str) -> Result<()> {
    let installation = "installation".try_into()?;
    let tenant = "tenant".try_into()?;
    for (boundary, result) in [
        (
            "inventory",
            obligations::validate(tx, &installation, charge)
                .await
                .map(|_| ()),
        ),
        (
            "next due",
            obligations::next_due(tx, &installation, charge)
                .await
                .map(|_| ()),
        ),
        (
            "tenant count",
            obligations::tenant_count(tx, &installation, &tenant, charge)
                .await
                .map(|_| ()),
        ),
    ] {
        let error = result
            .err()
            .with_context(|| format!("{boundary} falsely accepted {expected}"))?;
        ensure!(error.to_string() == expected, "{boundary}: {error:#}");
    }
    Ok(())
}

async fn populate(tx: &DatabaseTransaction, table: &str) -> Result<()> {
    // Opaque payloads are intentional: the compatibility path must refuse ANY
    // retained row, including known-state rows belonging to another namespace.
    // These are stored test cells, never an execution scope or admission proof.
    let mut fields: Vec<(&str, Value)> = vec![("installation_id", "other-installation".into())];
    if table == "tenancy_oauth_slots" {
        fields.extend([
            ("slot_key", "a".repeat(64).into()),
            ("owner_kind", "tenant".into()),
            ("owner_id", "other-tenant".into()),
            ("credential_resource_id", "credential".into()),
            ("authority_revision", "revision".into()),
            ("generation", 1_i64.into()),
            ("state", "ready".into()),
            ("metadata_json", "{}".into()),
            ("payload_json", "{}".into()),
            ("content_digest", "b".repeat(64).into()),
        ]);
    } else {
        fields.extend([
            ("tenant_id", "other-tenant".into()),
            ("execution_id", "execution".into()),
            ("unknown_outcome", false.into()),
        ]);
        if table == "tenancy_execution_receipts" {
            fields.extend([
                ("principal_id", "creator".into()),
                ("correlation_id", "correlation".into()),
                ("receipt_json", "{}".into()),
                ("content_digest", "b".repeat(64).into()),
                ("disposition_json", "{}".into()),
                ("byte_count", 2_i64.into()),
                ("recorded_at", "2026-10-25T00:00:00Z".into()),
            ]);
        } else {
            ensure!(table == "tenancy_receipt_auxiliary");
            fields.extend([
                ("receipt_digest", "b".repeat(64).into()),
                ("evidence_json", "{}".into()),
                ("evidence_digest", "c".repeat(64).into()),
            ]);
        }
    }
    let mut query = Query::insert();
    query.into_table(Alias::new(table));
    query.columns(fields.iter().map(|(name, _)| Alias::new(*name)));
    query.values(fields.into_iter().map(|(_, value)| value.into()))?;
    ensure!(
        tx.execute(tx.get_database_backend().build(&query))
            .await?
            .rows_affected()
            == 1
    );
    Ok(())
}
