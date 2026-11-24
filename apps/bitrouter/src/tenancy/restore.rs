//! Authenticate a stopped copy under its original installation and wrapping key.

mod connection;
mod validation;

use self::connection::ReadConnection;
use crate::offline::{ValidationBudget, large::Workspace};
use crate::tenancy::resources::Resources;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::identity::InstallationId;
use sea_orm::{ConnectionTrait, Statement, sqlx::SqliteConnection};

/// Restore preserves stored authority and unresolved custody. This does not
/// run feature recovery, clear a fence, initialize storage or admit dispatch.
pub(crate) async fn inspect_restore_in(
    db: &mut SqliteConnection,
    marker: &str,
    expected: &InstallationId,
    wrapping_key_env: &str,
    validation: ValidationBudget,
    workspace: &Workspace,
) -> Result<()> {
    workspace.check()?;
    let reader = ReadConnection::new(db)
        .with_row_limit(validation.rows.max(128))
        .with_workspace(workspace);
    let bytes = validation.bytes.min(workspace.available());
    let _charge = workspace.reserve(bytes)?;
    let state = validation::load(
        &reader,
        ValidationBudget {
            rows: validation.rows,
            bytes,
        },
        marker,
    )
    .await?
    .header;
    ensure!(
        &state.installation == expected,
        "restore installation mismatch"
    );
    let settings = crate::config::tenancy::TenancyConfig {
        enabled: true,
        wrapping_key_env: Some(wrapping_key_env.to_owned()),
        ..Default::default()
    };
    let resources = Resources::load_key(expected, &settings)?;
    resources.inspect_key_epoch_in(&reader).await?;
    // Key metadata and the 32-reference page are covered separately from the
    // one original authenticated decoder. Neither charge accumulates by row.
    let body = bytes
        .checked_sub(512 * 1024)
        .context("restore metadata working capacity")?;
    super::continuation::audit_in(&reader, &state, &resources, body, &|| workspace.check()).await?;
    inspect_oauth_in(&reader, expected, &resources, body, workspace).await?;
    workspace.check()
}

async fn inspect_oauth_in(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    resources: &Resources,
    bytes: usize,
    workspace: &Workspace,
) -> Result<()> {
    use crate::tenancy::oauth::storage;
    let crypto = resources.oauth_crypto()?;
    let mut after = String::new();
    loop {
        workspace.check()?;
        // Read one bounded metadata row through the slot PK, then invoke the
        // original decoder/token authentication on that exact current revision.
        let row = db.query_one(Statement::from_sql_and_values(db.get_database_backend(), "SELECT CASE WHEN length(CAST(slot_key AS BLOB))<=64 THEN slot_key ELSE NULL END AS slot, CASE WHEN length(CAST(metadata_json AS BLOB))<=8192 THEN metadata_json ELSE NULL END AS metadata FROM tenancy_oauth_slots WHERE slot_key>? ORDER BY tenancy_oauth_slots.slot_key LIMIT 1", [after.as_str().into()])).await?;
        let Some(row) = row else {
            break;
        };
        ensure!(
            bytes >= 8 * 1024 * 1024,
            crate::offline::large::CapacityExceeded
        );
        let slot: String = row.try_get("", "slot")?;
        let metadata: storage::Metadata =
            serde_json::from_str(&row.try_get::<String>("", "metadata")?)?;
        metadata.validate()?;
        ensure!(
            &metadata.installation == installation && metadata.slot == slot && slot > after,
            "restore OAuth identity/order mismatch"
        );
        let retained = storage::load(db, installation, &metadata.reference)
            .await?
            .context("restore OAuth slot disappeared")?;
        ensure!(
            retained.metadata == metadata,
            "restore OAuth metadata changed"
        );
        retained.token(&crypto)?;
        after = slot;
    }
    Ok(())
}
