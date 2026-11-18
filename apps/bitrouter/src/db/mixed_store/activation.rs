use super::*;
use sea_orm::sqlx::{ConnectOptions, Connection};

/// The new BRO scheduler consumes its own new checkpoints. Retained work is
/// inspectable, but cannot authorize a new effect until that scheduler supports
/// the original grant, tombstones, budget and recovery contract.
pub(super) async fn blockers(db: &DatabaseConnection) -> Result<Vec<String>> {
    let objects = catalog::source(db).await?;
    let mut blockers = Vec::new();
    for object in objects.iter().filter(|o| o.kind == "table") {
        let table = object.name.as_str();
        if table == "seaql_migrations" || !catalog::belongs(table, Owner::Bro)? {
            continue;
        }
        let query = match table {
            "bro_runtime_ownership" => "SELECT 1 WHERE (SELECT count(*) FROM bro_runtime_ownership)<>1 OR NOT EXISTS (SELECT 1 FROM bro_runtime_ownership WHERE id=1 AND generation=0 AND server_instance_id IS NULL AND stopped_at_ms IS NULL)".into(),
            "checkpoint_evolution" => format!("SELECT 1 FROM checkpoint_evolution WHERE {} LIMIT 1", catalog::evolution(Owner::Bro)),
            _ => format!("SELECT 1 FROM {table} LIMIT 1"),
        };
        if db.query_one(sql(query)).await?.is_some() {
            blockers.push(format!(
                "retained BRO state in {table} requires inspection-only recovery"
            ));
        }
    }
    for (reason, query) in [
        (
            "unresolved execution obligations",
            "SELECT 1 FROM tenancy_unknown_obligations",
        ),
        (
            "unsettled dispatch intent",
            "SELECT 1 FROM tenancy_dispatch_intents",
        ),
        (
            "unknown execution receipt",
            "SELECT 1 FROM tenancy_execution_receipts WHERE unknown_outcome<>0",
        ),
        (
            "unknown auxiliary receipt",
            "SELECT 1 FROM tenancy_receipt_auxiliary WHERE unknown_outcome<>0",
        ),
        (
            "unresolved provider credential refresh",
            "SELECT 1 FROM tenancy_oauth_slots WHERE state<>'ready' OR pending_execution_id IS NOT NULL OR pending_tenant_id IS NOT NULL",
        ),
        (
            "unsettled model charge",
            "SELECT 1 FROM requests WHERE charge_status NOT IN ('computed','not_charged') OR reconciliation_status NOT IN ('computed','not_charged','not_applicable')",
        ),
        (
            "unpublished provider continuation",
            "SELECT 1 FROM provider_continuations WHERE publication_state<>'active'",
        ),
        (
            "undelivered trajectory event",
            "SELECT 1 FROM trajectory_outbox WHERE delivered_at IS NULL",
        ),
        (
            "unfinished trajectory request",
            "SELECT 1 FROM trajectory_requests WHERE settlement_event_id IS NULL",
        ),
        (
            "gateway execution history requires independent settlement review",
            "SELECT 1 FROM checkpoint_evolution WHERE kind IN ('gateway_request','execution','judge_attempt','judge_request_owner')",
        ),
    ] {
        if db
            .query_one(sql(format!("{query} LIMIT 1")))
            .await?
            .is_some()
        {
            blockers.push(reason.into());
        }
    }
    if db
        .query_one(sql("SELECT 1 FROM tenancy_installation"))
        .await?
        .is_none()
    {
        for object in objects.iter().filter(|o| {
            o.kind == "table"
                && o.name.starts_with("tenancy_")
                && o.name != "tenancy_provider_technical_schema"
        }) {
            if db
                .query_one(sql(format!("SELECT 1 FROM {} LIMIT 1", object.name)))
                .await?
                .is_some()
            {
                blockers.push("tenant state without an installation identity".into());
                break;
            }
        }
    }
    Ok(blockers)
}

/// Reuse the ordinary stopped-authority validator on the completed gateway
/// projection before publishing. This includes canonical receipts, Unknown
/// source coverage, directory state, archive and retained Eval validation.
pub(super) async fn verify_service(path: &Path) -> Result<()> {
    let mut reader = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .immutable(true)
        .pragma("trusted_schema", "OFF")
        .connect()
        .await?;
    let result: Result<()> = async {
        let id: Option<String> =
            sea_orm::sqlx::query_scalar("SELECT installation_id FROM tenancy_installation")
                .fetch_optional(&mut reader)
                .await?;
        if let Some(id) = id {
            let id = id.try_into()?;
            crate::tenancy::audit::recovery::inspect_source(
                &mut reader,
                &id,
                &|| Ok(()),
                crate::offline::ValidationBudget::default(),
            )
            .await?;
        }
        Ok(())
    }
    .await;
    reader.close().await?;
    result.context("gateway recovery validation failed before activation")
}
