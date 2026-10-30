//! Separately built reduced binaries must fence an interrupted paid draft.
//! The full producer uses leased physical SQLite from its first connection.
use super::*;
use crate::tenancy::{actor::OperationLimits, ownership, resources::Resources, store};
use bitrouter_ai::tenancy::identity::RevisionId;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::Duration};

#[cfg(all(unix, feature = "core", feature = "harness"))]
mod driver;

pub(in crate::tenancy::tests) const ROOT: &str = "BITROUTER_PAID_STARTUP_ROOT";
#[cfg(all(unix, feature = "core", feature = "harness"))]
pub(in crate::tenancy::tests) const MODE: &str = "BITROUTER_PAID_STARTUP_MODE";
#[cfg(all(unix, feature = "core", feature = "harness"))]
pub(in crate::tenancy::tests) const WITNESS: &str = "paid-startup-physical-file-ready";
const PROFILE: &str = "BITROUTER_PAID_STARTUP_PROFILE";

#[derive(Serialize, Deserialize)]
pub(in crate::tenancy::tests) struct Transfer {
    pub installation: InstallationId,
    pub tenant: TenantId,
    pub revision: RevisionId,
}

async fn retained(db: &DatabaseConnection) -> Result<Vec<Vec<serde_json::Value>>> {
    // Fixture observations only: inspect complete paid rows, not just counts.
    // Check lengths before decoding bodies, then cap the actual projection.
    let lengths = db.query_all(Statement::from_string(db.get_database_backend(),
        "SELECT length(body) AS bytes FROM checkpoint_evolution WHERE kind IN ('owned_judge_job','owned_judge_active_v3','owned_judge_budget','owned_judge_pending') LIMIT 4")).await?;
    ensure!(lengths.len() <= 3);
    for row in lengths {
        ensure!(row.try_get::<i64>("", "bytes")? <= 1024 * 1024);
    }
    let rows = db.query_all(Statement::from_string(db.get_database_backend(),
        "SELECT record_id, scope_id, kind, session_key, revision, body, installation_id, tenant_id, creator_principal_id FROM checkpoint_evolution WHERE kind IN ('owned_judge_job','owned_judge_active_v3','owned_judge_budget','owned_judge_pending') ORDER BY record_id LIMIT 4")).await?;
    ensure!(rows.len() <= 3);
    rows.into_iter()
        .map(|row| {
            let mut fields = Vec::new();
            for column in [
                "record_id",
                "scope_id",
                "kind",
                "session_key",
                "body",
                "installation_id",
                "tenant_id",
                "creator_principal_id",
            ] {
                fields.push(serde_json::to_value(
                    row.try_get::<Option<String>>("", column)?,
                )?);
            }
            fields.push(row.try_get::<i64>("", "revision")?.into());
            Ok(fields)
        })
        .collect()
}

async fn consume(path: &Path, pending: bool, native: bool) -> Result<()> {
    let transfer: Transfer = serde_json::from_slice(&std::fs::read(path.with_extension("json"))?)?;
    let owner = ownership::acquire_path(path, false)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}?mode=rw", path.display()), &owner).await?;
    let checked = async {
        let before = retained(&db).await?;
        ensure!(before.len() == if pending { 3 } else { 0 });
        if native {
            check_native_pending(&db, &before, pending).await?;
        }
        let state = store::load(&db, Utc::now()).await?;
        ensure!(state.installation == transfer.installation);
        ensure!(
            state.active_configurations.is_empty(),
            "an active profile could mask the pending fence"
        );
        let resources = Resources::with_key(transfer.installation.clone(), [29; 32])?;
        let draft = crate::tenancy::configuration_store::configuration(
            &db,
            &transfer.tenant,
            &transfer.revision,
        )
        .await?
        .context("actual inactive assessed draft")?;
        resources
            .validate_compiled(&draft)
            .map_err(|code| anyhow::anyhow!("draft integrity failed: {code:?}"))?;
        ensure!(resources.validate_executable(&draft) == Err(ErrorCode::CapabilityMismatch));
        let started = Service::start_prepared(
            db.clone(),
            Some(owner.clone()),
            OperationLimits::default(),
            state,
            Arc::new(resources),
        )
        .await;
        match started {
            Ok(service) => {
                let healthy = service.current().is_ok();
                service.shutdown().await?;
                ensure!(
                    !pending && healthy,
                    "pending accepted work published readiness"
                );
            }
            Err(error) => ensure!(pending, "clean inactive draft failed startup: {error:#}"),
        }
        ensure!(
            retained(&db).await? == before,
            "startup changed accepted job/budget/pending bytes or metadata"
        );
        Ok(())
    }
    .await;
    let closed = db.close().await;
    drop(owner);
    checked?;
    closed?;
    // A confirmed refusal must retire the bootstrap owner; ambiguity must not
    // be counted as a passing rejection just because the waiter saw an error.
    let lease = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ownership::acquire_path(path, false) {
                Ok(lease) => break Ok(lease),
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => break Err(error),
            }
        }
    })
    .await??;
    drop(lease);
    Ok(())
}

#[tokio::test]
#[ignore = "only the full producer driver supplies these real files to a separately built reduced libtest"]
async fn reduced_consumer() -> Result<()> {
    let expected = std::env::var(PROFILE)?;
    let actual = match (cfg!(feature = "core"), cfg!(feature = "harness")) {
        (false, false) => "gateway",
        (true, false) => "core-only",
        (false, true) => "harness-only",
        (true, true) => "full",
    };
    ensure!(
        expected == actual && actual != "full",
        "wrong consumer feature profile"
    );
    let root = std::path::PathBuf::from(std::env::var_os(ROOT).context("transfer directory")?);
    tokio::time::timeout(Duration::from_secs(30), async {
        consume(&root.join("clean.db"), false, false).await?;
        consume(&root.join("pending.db"), true, false).await
    })
    .await??;
    std::fs::write(
        root.join("consumer-ack"),
        format!("paid-pending-refused:{actual}"),
    )?;
    Ok(())
}

async fn check_native_pending(
    db: &DatabaseConnection,
    retained: &[Vec<serde_json::Value>],
    pending: bool,
) -> Result<()> {
    // A different pending family must not mask the paid-v4 startup fence.
    ensure!(
        db.query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT 1 FROM checkpoint_evolution WHERE kind='native_experiment_unit_pending_v1' LIMIT 1",
        ))
        .await?
        .is_none(),
        "native Unit cleanup must precede the reduced paid consumer"
    );
    if !pending {
        return Ok(());
    }
    let mut active = None;
    let mut mirror = None;
    let mut budget = None;
    for fields in retained {
        let body: serde_json::Value = serde_json::from_str(
            fields
                .get(4)
                .and_then(serde_json::Value::as_str)
                .context("retained body")?,
        )?;
        match fields.get(2).and_then(serde_json::Value::as_str) {
            Some("owned_judge_active_v3") => active = Some(body),
            Some("owned_judge_pending") => mirror = Some(body),
            Some("owned_judge_budget") => budget = Some(body),
            _ => anyhow::bail!("native pending fixture contains an unexpected paid record"),
        }
    }
    let active = active.context("real native active record")?;
    ensure!(
        active["version"] == 4
            && active["status"] == "accepted"
            && active["recovery"]["native_cleanup"].is_object()
            && active["recovery"]["stopped"].is_object()
            && active["recovery"]["observed"].is_object()
            && active["native_evidence"].is_object()
            && mirror.as_ref() == Some(&active)
            && budget.context("original paid counter")?["accepted"] == 1,
        "reduced consumer did not receive the original observed stopped paid-v4 job"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "real native-v4 producer and separately built reduced libtest required"]
async fn reduced_native_consumer() -> Result<()> {
    let expected = std::env::var(PROFILE)?;
    let actual = match (cfg!(feature = "core"), cfg!(feature = "harness")) {
        (false, false) => "gateway",
        (true, false) => "core-only",
        (false, true) => "harness-only",
        (true, true) => "full",
    };
    ensure!(
        expected == actual && actual != "full",
        "wrong reduced native profile"
    );
    let root =
        std::path::PathBuf::from(std::env::var_os(ROOT).context("native transfer directory")?);
    tokio::time::timeout(Duration::from_secs(30), async {
        consume(&root.join("clean/native-paid.db"), false, true).await?;
        consume(&root.join("pending/native-paid.db"), true, true).await
    })
    .await??;
    std::fs::write(
        root.join("native-consumer-ack"),
        format!("native-v4-refused:{actual}"),
    )?;
    Ok(())
}
