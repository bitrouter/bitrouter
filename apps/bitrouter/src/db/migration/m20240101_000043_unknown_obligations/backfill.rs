//! Derive only the added lookup fields. Canonical bytes/digests are untouched;
//! complete semantic validation still precedes format activation/readiness.
use super::*;
use bitrouter_ai::tenancy::identity::{ExecutionId, TenantId};
use sea_orm::{Statement, Value};
fn sql(query: &str, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DatabaseBackend::Sqlite, query, values)
}
pub(super) async fn run(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let mut after = String::new();
    loop {
        let rows = db.query_all(sql("SELECT CASE WHEN typeof(slot_key)='text' AND length(CAST(slot_key AS BLOB)) BETWEEN 1 AND 64 THEN slot_key ELSE NULL END AS slot_key,CASE WHEN typeof(metadata_json)='text' AND length(CAST(metadata_json AS BLOB)) BETWEEN 1 AND 8192 THEN metadata_json ELSE NULL END AS metadata_json,CASE WHEN typeof(state)='text' AND length(CAST(state AS BLOB))<=32 THEN state ELSE NULL END AS state FROM tenancy_oauth_slots WHERE slot_key>? ORDER BY tenancy_oauth_slots.slot_key LIMIT 32", [after.clone().into()])).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let slot: String = row.try_get("", "slot_key")?;
            let json: String = row.try_get("", "metadata_json")?;
            let value: serde_json::Value =
                serde_json::from_str(&json).map_err(|_| incompatible())?;
            let state = value
                .get("state")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(incompatible)?;
            let name = state
                .get("state")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(incompatible)?;
            if name != row.try_get::<String>("", "state")? {
                return Err(incompatible());
            }
            let pending: Option<(TenantId, ExecutionId)> = match name {
                "ready" if !state.contains_key("work") => None,
                "refresh_started" | "quarantined" => {
                    let work = state.get("work").ok_or_else(incompatible)?;
                    Some((
                        serde_json::from_value(
                            work.get("tenant").ok_or_else(incompatible)?.clone(),
                        )
                        .map_err(|_| incompatible())?,
                        serde_json::from_value(
                            work.get("execution").ok_or_else(incompatible)?.clone(),
                        )
                        .map_err(|_| incompatible())?,
                    ))
                }
                _ => return Err(incompatible()),
            };
            let tenant = pending
                .as_ref()
                .map(|(tenant, _)| tenant.as_str().to_owned());
            let execution = pending
                .as_ref()
                .map(|(_, execution)| execution.as_str().to_owned());
            if db.execute(sql("UPDATE tenancy_oauth_slots SET pending_tenant_id=?,pending_execution_id=? WHERE slot_key=?", [tenant.into(), execution.into(), slot.clone().into()])).await?.rows_affected()!=1 { return Err(incompatible()); }
            after = slot;
        }
    }
    // The cursor cannot skip an invalid empty/non-text key as initialized data.
    if db.query_one(sql("SELECT 1 FROM tenancy_oauth_slots WHERE typeof(slot_key)<>'text' OR length(CAST(slot_key AS BLOB)) NOT BETWEEN 1 AND 64 LIMIT 1", [])).await?.is_some() { return Err(incompatible()); }
    Ok(())
}
