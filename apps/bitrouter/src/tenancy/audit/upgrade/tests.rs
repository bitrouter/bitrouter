use super::*;
use crate::tenancy::{
    model::ReadContext,
    store::{self, Operation, StoredResponse},
};
use bitrouter_ai::tenancy::{
    identity::PrincipalId,
    operation::{OperationResult, OperationStatus},
};
use sea_orm::{
    TransactionTrait,
    sqlx::{Connection, Row},
};

async fn fixture() -> Result<(tempfile::TempDir, std::path::PathBuf, Request)> {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("authority.sqlite");
    std::fs::write(&path, [])?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let db = crate::db::connect(&format!("sqlite://{}", path.display())).await?;
    crate::db::run_migrations(&db).await?;
    let mut state = store::initial_header(&db, chrono::Utc::now()).await?;
    state.operation_ids = 1;
    state.revision = 1_u64.try_into()?;
    let result = OperationResult {
        operation_id: "seed".try_into()?,
        status: OperationStatus::Applied,
        authority_revision: Some(state.revision),
        resource_revision: Some("r1".try_into()?),
        resource_id: Some("fixture".try_into()?),
        error: None,
    };
    let operation = Operation {
        principal: PrincipalId::for_control_credential("fixture")?,
        id: result.operation_id.clone(),
        context: ReadContext::Installation,
        digest: "a".repeat(64),
        response: StoredResponse::Operation(result.clone()),
        result,
        created_at: chrono::Utc::now(),
        safety: false,
    };
    let tx = db.begin().await?;
    store::write_in(&tx, &state, &[], &operation).await?;
    tx.execute_unprepared(r#"WITH RECURSIVE numbers(n) AS (SELECT 2 UNION ALL SELECT n+1 FROM numbers WHERE n<99488)
        INSERT INTO tenancy_operations SELECT installation_id,principal_id,printf('op-%06d',n),tenant_id,authority_class,read_context_json,hash_version,command_digest,
        replace(result_json,'"seed"','"'||printf('op-%06d',n)||'"'),replace(response_json,'"seed"','"'||printf('op-%06d',n)||'"'),created_at,safety
        FROM tenancy_operations CROSS JOIN numbers WHERE operation_id='seed'"#).await?;
    tx.execute_unprepared(
        "UPDATE tenancy_installation SET schema_version=1,operation_ids=99488,saturated=1",
    )
    .await?;
    {
        use crate::tenancy::receipts::store::{Candidate, Disposition};
        use bitrouter_ai::tenancy::{
            execution::{
                ExecutionReceipt, Outcome as ReceiptOutcome, Provenance, ReceiptState, Usage,
            },
            operation::DispatchState,
        };
        let receipt = Candidate::new(
            ExecutionReceipt {
                installation_id: state.installation.clone(),
                tenant_id: "tenant".try_into()?,
                principal_id: "principal".try_into()?,
                execution_id: "unknown-execution".try_into()?,
                correlation_id: "correlation".try_into()?,
                execution_revision: "r1".try_into()?,
                state: ReceiptState::Unknown,
                outcome: ReceiptOutcome::Unknown,
                attempts: Vec::new(),
                usage: Usage {
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    reasoning_tokens: None,
                    provenance: Provenance::Unknown,
                    complete: false,
                },
                recorded_at: "2026-10-25T00:00:00Z".try_into()?,
            },
            Disposition {
                dispatch: DispatchState::Unknown,
                statuses: Vec::new(),
                auxiliary_digest: None,
            },
        )?;
        assert!(crate::tenancy::receipts::store::insert(&tx, &receipt).await?);
        use crate::tenancy::receipts::store::obligations;
        let change = obligations::reconcile(
            &tx,
            &receipt.receipt.installation_id,
            &obligations::Key {
                tenant: receipt.receipt.tenant_id.clone(),
                execution: receipt.receipt.execution_id.clone(),
            },
            &|_, _| Ok(()),
        )
        .await?;
        drop(change);
    }
    tx.commit().await?;
    let request = Request {
        installation: state.installation,
        revision: state.revision,
        operation: "upgrade-once".try_into()?,
        acknowledge_legacy_count_saturation: true,
    };
    db.close().await?;
    Ok((directory, path, request))
}

async fn open(path: &std::path::Path) -> Result<SqliteConnection> {
    use sea_orm::sqlx::ConnectOptions;
    Ok(sea_orm::sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .connect()
        .await?)
}
async fn transaction(
    db: &mut SqliteConnection,
    request: &Request,
    commit: bool,
) -> Result<Outcome> {
    transaction_with(db, request, commit, || Ok(())).await
}
async fn transaction_with(
    db: &mut SqliteConnection,
    request: &Request,
    commit: bool,
    before_write: impl FnOnce() -> Result<()>,
) -> Result<Outcome> {
    sea_orm::sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *db)
        .await?;
    let result = apply_in(db, request, ValidationBudget::default(), before_write).await;
    sea_orm::sqlx::query(if result.is_ok() && commit {
        "COMMIT"
    } else {
        "ROLLBACK"
    })
    .execute(&mut *db)
    .await?;
    result
}
async fn history(db: &mut SqliteConnection) -> Result<[u8; 32]> {
    use futures::TryStreamExt;
    let mut hash = Sha256::new();
    let mut rows = sea_orm::sqlx::query("SELECT installation_id,principal_id,operation_id,command_digest,result_json,response_json,read_context_json,created_at,hash_version,safety FROM tenancy_operations ORDER BY installation_id,principal_id,operation_id").fetch(&mut *db);
    while let Some(row) = rows.try_next().await? {
        for column in [
            "installation_id",
            "principal_id",
            "operation_id",
            "command_digest",
            "result_json",
            "response_json",
            "read_context_json",
            "created_at",
        ] {
            let bytes: String = row.try_get(column)?;
            hash.update(u64::try_from(bytes.len())?.to_be_bytes());
            hash.update(bytes.as_bytes());
        }
        for column in ["hash_version", "safety"] {
            hash.update(row.try_get::<i64, _>(column)?.to_be_bytes());
        }
    }
    Ok(hash.finalize().into())
}

#[tokio::test]
async fn reset_is_atomic_replay_bound_and_never_clears_a_later_fence() -> Result<()> {
    let (_directory, path, mut request) = fixture().await?;
    let mut db = open(&path).await?;
    let before = history(&mut db).await?;
    let evidence: (String, String, i64) = sea_orm::sqlx::query_as(
        "SELECT receipt_json,content_digest,unknown_outcome FROM tenancy_execution_receipts",
    )
    .fetch_one(&mut db)
    .await?;
    request.acknowledge_legacy_count_saturation = false;
    let probes = std::cell::Cell::new(0);
    assert!(
        transaction_with(&mut db, &request, true, || {
            probes.set(probes.get() + 1);
            Ok(())
        })
        .await
        .is_err()
    );
    assert_eq!(
        probes.get(),
        0,
        "invalid requests must not reach write preflight"
    );
    request.acknowledge_legacy_count_saturation = true;
    assert!(
        transaction_with(&mut db, &request, true, || {
            probes.set(probes.get() + 1);
            anyhow::bail!("injected insufficient free space")
        })
        .await
        .is_err()
    );
    assert_eq!(probes.get(), 1);
    let untouched: (i64, i64, i64) = sea_orm::sqlx::query_as(
        "SELECT schema_version,saturated,(SELECT COUNT(*) FROM tenancy_capacity_upgrades) FROM tenancy_installation"
    ).fetch_one(&mut db).await?;
    assert_eq!(untouched, (1, 1, 0));
    assert_eq!(before, history(&mut db).await?);
    // The valid prerequisite-only profile also needs preflight before CREATE
    // TABLE or migration-ledger insertion, not just before existing-table DML.
    sea_orm::sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("DROP TABLE tenancy_capacity_upgrades")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("DELETE FROM seaql_migrations WHERE version=?")
        .bind(MIGRATION)
        .execute(&mut db)
        .await?;
    let ddl_probes = std::cell::Cell::new(0);
    assert!(
        apply_in(&mut db, &request, ValidationBudget::default(), || {
            ddl_probes.set(ddl_probes.get() + 1);
            anyhow::bail!("injected insufficient free space before DDL")
        })
        .await
        .is_err()
    );
    assert_eq!(
        ddl_probes.get(),
        1,
        "prerequisite-only profile must validate"
    );
    let absent: (i64, i64) = sea_orm::sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM sqlite_schema WHERE name='tenancy_capacity_upgrades'),(SELECT COUNT(*) FROM seaql_migrations WHERE version=?)"
    ).bind(MIGRATION).fetch_one(&mut db).await?;
    assert_eq!(absent, (0, 0));
    let header: (i64, i64, i64) = sea_orm::sqlx::query_as(
        "SELECT schema_version,saturated,authority_revision FROM tenancy_installation",
    )
    .fetch_one(&mut db)
    .await?;
    assert_eq!(header, (1, 1, i64::try_from(request.revision.get())?));
    assert_eq!(before, history(&mut db).await?);
    // Restore the fixture's original schema for the remaining COMMIT/replay cases.
    sea_orm::sqlx::query("ROLLBACK").execute(&mut db).await?;
    let staged = transaction(&mut db, &request, false).await?;
    let row = sea_orm::sqlx::query("SELECT schema_version,saturated FROM tenancy_installation")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(row.try_get::<i64, _>("schema_version")?, 1);
    assert_eq!(row.try_get::<i64, _>("saturated")?, 1);
    let committed = transaction(&mut db, &request, true).await?;
    assert_eq!(
        committed.new_authority_revision,
        staged.new_authority_revision
    );
    assert_eq!(before, history(&mut db).await?);
    let preserved: (String, String, i64) = sea_orm::sqlx::query_as(
        "SELECT receipt_json,content_digest,unknown_outcome FROM tenancy_execution_receipts",
    )
    .fetch_one(&mut db)
    .await?;
    assert_eq!(evidence, preserved);
    // Simulate loss of the successful ACK followed by a separate later stop.
    sea_orm::sqlx::query(
        "UPDATE tenancy_installation SET saturated=1,authority_revision=authority_revision+1",
    )
    .execute(&mut db)
    .await?;
    let changes: (i64,) = sea_orm::sqlx::query_as("SELECT total_changes()")
        .fetch_one(&mut db)
        .await?;
    let replayed = transaction_with(&mut db, &request, true, || {
        probes.set(probes.get() + 1);
        anyhow::bail!("injected insufficient free space")
    })
    .await?;
    assert_eq!(
        probes.get(),
        1,
        "exact replay is read-only even under low space"
    );
    let after_changes: (i64,) = sea_orm::sqlx::query_as("SELECT total_changes()")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(changes, after_changes);
    assert_eq!(committed, replayed);
    let row = sea_orm::sqlx::query(
        "SELECT schema_version,saturated,authority_revision FROM tenancy_installation",
    )
    .fetch_one(&mut db)
    .await?;
    assert_eq!(
        row.try_get::<i64, _>("schema_version")?,
        i64::from(crate::tenancy::AUTHORITY_SCHEMA_VERSION)
    );
    assert_eq!(row.try_get::<i64, _>("saturated")?, 1);
    assert_eq!(row.try_get::<i64, _>("authority_revision")?, 3);
    request.revision = 2_u64.try_into()?;
    assert!(transaction(&mut db, &request, true).await.is_err());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn reset_refuses_late_history_corruption_and_unknown_stop_cause() -> Result<()> {
    let (_directory, path, request) = fixture().await?;
    let mut db = open(&path).await?;
    sea_orm::sqlx::query("UPDATE tenancy_operations SET hash_version=2 WHERE operation_id='seed'")
        .execute(&mut db)
        .await?;
    assert!(transaction(&mut db, &request, true).await.is_err());
    sea_orm::sqlx::query("DELETE FROM tenancy_operations WHERE operation_id<>'seed'")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("UPDATE tenancy_operations SET hash_version=1")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("UPDATE tenancy_installation SET operation_ids=1")
        .execute(&mut db)
        .await?;
    assert!(transaction(&mut db, &request, true).await.is_err());
    let row = sea_orm::sqlx::query("SELECT saturated,schema_version FROM tenancy_installation")
        .fetch_one(&mut db)
        .await?;
    assert_eq!(row.try_get::<i64, _>("saturated")?, 1);
    assert_eq!(row.try_get::<i64, _>("schema_version")?, 1);
    db.close().await?;
    Ok(())
}
