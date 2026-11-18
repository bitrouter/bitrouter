use super::*;
use crate::db::mixed_store::Owner;
use crate::offline::format::{LedgerEntry, Table};

#[tokio::test]
async fn fresh_gateway_requires_its_own_ledger_and_excludes_runtime_history() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let installation = "installation".try_into()?;
    let mut inventory = Inventory {
        encoding: "UTF-8".into(),
        schema_digest: String::new(),
        tables: Vec::new(),
        rows: 0,
        value_bytes: 0,
        installation_id: Some("installation".into()),
        ledger: Some(vec![LedgerEntry {
            version: Owner::BitRouter.marker().into(),
            applied_at: 1,
        }]),
    };
    recovery(&db, &inventory, &installation).await?;
    for marker in [
        crate::db::store_format::BASELINE,
        Owner::Bro.marker(),
        "bitrouter_split_pending_gateway",
    ] {
        inventory.ledger = Some(vec![LedgerEntry {
            version: marker.into(),
            applied_at: 1,
        }]);
        ensure!(recovery(&db, &inventory, &installation).await.is_err());
    }
    inventory.ledger = Some(vec![LedgerEntry {
        version: Owner::BitRouter.marker().into(),
        applied_at: 1,
    }]);
    inventory.tables.push(Table {
        name: "bro_runtime_ownership".into(),
        columns: Vec::new(),
        rows: 1,
        value_bytes: 8,
        digest: String::new(),
    });
    ensure!(recovery(&db, &inventory, &installation).await.is_err());
    Ok(())
}
