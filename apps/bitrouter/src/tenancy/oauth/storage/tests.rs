use super::*;
use crate::tenancy::receipts::{
    auxiliary::{Fact, Outcome as AuxiliaryOutcome},
    auxiliary_store,
    store::{self as receipt_store, Candidate, Disposition},
};
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome, Provenance, ReceiptState, Usage},
    operation::DispatchState,
};
use sea_orm::{Database, DatabaseConnection, TransactionTrait};
use std::time::Duration;
mod page;
mod server;

struct Case {
    crypto: Crypto,
    initial: Row,
    replacement: Row,
    receipt: Candidate,
}
fn case() -> Result<Case> {
    let installation: InstallationId = "oauth-installation".try_into()?;
    let reference = ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "tenant".try_into()?,
        resource_kind: ResourceKind::Credential,
        resource_id: "slot".try_into()?,
        revision_id: "r1".try_into()?,
    };
    let registration =
        crate::gateway::providers::login::find("anthropic").context("registration")?;
    let profile = Profile {
        format: 1,
        mechanism: "claude_code_refresh_v1".into(),
        provider: "claude-code".into(),
        account: "default".into(),
        registration: "anthropic".into(),
        token_endpoint: registration.auth.token_endpoint,
        client_id: registration.auth.client_id,
    };
    let crypto = Crypto::new(
        installation.clone(),
        crate::runtime_crypto::DeploymentKey::from_bytes([71; 32])?,
    );
    let token = OAuthToken {
        access_token: "seed-access".into(),
        refresh_token: Some("seed-refresh".into()),
        expires_at: 1,
    };
    let metadata = Metadata {
        format: 1,
        installation: installation.clone(),
        slot: slot(&installation, &reference)?,
        seed: crypto.seed(&reference, &profile, &token)?,
        profile,
        reference: reference.clone(),
        generation: 0,
        state: State::Ready,
    };
    let initial = Row::new(&crypto, metadata.clone(), &token)?;
    let replacement = Row::new(
        &crypto,
        Metadata {
            generation: 1,
            ..metadata
        },
        &OAuthToken {
            access_token: "replacement-access".into(),
            refresh_token: Some("replacement-refresh".into()),
            expires_at: 9999999999,
        },
    )?;
    // No model ran in this storage test. Auxiliary refresh evidence must not
    // manufacture a model attempt or billed model counter.
    let receipt = Candidate::new(
        ExecutionReceipt {
            installation_id: installation,
            tenant_id: "tenant".try_into()?,
            principal_id: "creator".try_into()?,
            execution_id: "execution".try_into()?,
            correlation_id: "correlation".try_into()?,
            execution_revision: "r1".try_into()?,
            state: ReceiptState::Incomplete,
            outcome: Outcome::Failed,
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
            recorded_at: "2026-10-01T00:00:00Z".try_into()?,
        },
        Disposition {
            dispatch: DispatchState::NotStarted,
            statuses: Vec::new(),
            auxiliary_digest: None,
        },
    )?
    .with_auxiliary(vec![Fact {
        credential: reference,
        slot: initial.metadata.slot.clone(),
        generation: 0,
        work: uuid::Uuid::new_v4().to_string(),
        request_digest: Some("a".repeat(64)),
        http_status: Some(200),
        outcome: AuxiliaryOutcome::Refreshed,
    }])?;
    Ok(Case {
        crypto,
        initial,
        replacement,
        receipt,
    })
}
async fn read_receipt(db: &impl ConnectionTrait, case: &Case) -> Result<Option<ExecutionReceipt>> {
    let receipt = &case.receipt.receipt;
    receipt_store::lookup(
        db,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
    )
    .await
}
async fn committed_generation_and_receipt(db: &DatabaseConnection, case: &Case) -> Result<()> {
    let reference = &case.initial.metadata.reference;
    let installation = &case.initial.metadata.installation;
    let row = load(db, installation, reference)
        .await?
        .context("persisted slot")?;
    ensure!(
        row.same(&case.replacement)
            && row.token(&case.crypto)?.access_token == "replacement-access"
    );
    let receipt = read_receipt(db, case)
        .await?
        .context("persisted receipt and private evidence")?;
    ensure!(
        receipt.principal_id == case.receipt.receipt.principal_id
            && receipt.attempts.is_empty()
            && receipt.usage == case.receipt.receipt.usage
    );
    ensure!(serde_json::to_vec(&receipt)? == serde_json::to_vec(&case.receipt.receipt)?);
    // This raw server-storage fixture does not start the SQLite Service profile.
    if matches!(
        db.get_database_backend(),
        DatabaseBackend::Postgres | DatabaseBackend::MySql
    ) {
        return server_receipt_storage(db, case, &row).await;
    }
    let bytes = receipt_store::inventory(db, installation).await?;
    let canonical = case.receipt.bytes() + case.receipt.auxiliary_bytes();
    ensure!(bytes.rows == 1 && bytes.bytes == canonical);
    ensure!(
        bytes.live_bytes == canonical + crate::tenancy::receipts::capacity::ROW_OVERHEAD
            && bytes.history_bytes == 0
            && bytes.history_rows == 0
    );
    // OAuth reserves one maximum slot envelope independently of canonical
    // receipt bytes; rotation changes its payload without adding another slot.
    let slot_reserved = inventory(db, installation).await?;
    ensure!(slot_reserved == INVENTORY_BYTES && bytes.oauth_bytes == slot_reserved);
    for slot in [&case.initial, &row] {
        ensure!(u64::try_from(slot.json.len() + slot.payload.len())? <= slot_reserved);
    }
    Ok(())
}
// Canonical rows and accounted bytes, not Service projection or archive accounting.
async fn server_receipt_storage(db: &DatabaseConnection, case: &Case, row: &Row) -> Result<()> {
    let receipt = &case.receipt.receipt;
    let saved = receipt_store::lookup_candidate(
        db,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
    )
    .await?
    .context("persisted server receipt and private evidence")?;
    ensure!(saved.digest == case.receipt.digest);
    ensure!(
        saved.bytes() == case.receipt.bytes()
            && saved.auxiliary_bytes() == case.receipt.auxiliary_bytes()
    );
    // Together with the canonical point read, whole-table counts exclude any
    // additional foreign/orphan data in this fixed one-receipt storage fixture.
    let backend = db.get_database_backend();
    let count = if backend == DatabaseBackend::MySql {
        "CAST(COUNT(*) AS SIGNED)"
    } else {
        "CAST(COUNT(*) AS BIGINT)"
    };
    for (table, expected) in [
        ("tenancy_execution_receipts", 1_i64),
        ("tenancy_receipt_auxiliary", 1),
        ("tenancy_usage_evidence", 0),
    ] {
        let query = Query::select()
            .from(Alias::new(table))
            .expr_as(Expr::cust(count), Alias::new("rows"))
            .to_owned();
        let actual = db
            .query_one(backend.build(&query))
            .await?
            .context("server storage row count")?;
        ensure!(
            actual.try_get::<i64>("", "rows")? == expected,
            "unexpected server storage count: {table}"
        );
    }
    let slot_reserved = inventory(db, &receipt.installation_id).await?;
    ensure!(slot_reserved == INVENTORY_BYTES);
    for slot in [&case.initial, row] {
        ensure!(u64::try_from(slot.json.len() + slot.payload.len())? <= slot_reserved);
    }
    Ok(())
}
async fn write_and_verify(db: &DatabaseConnection, case: &Case) -> Result<()> {
    crate::db::run_migrations(db).await?;
    let transaction = db.begin().await?;
    match insert(&transaction, &case.initial).await {
        Ok(()) => transaction.commit().await?,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error);
        }
    }
    for commit in [false, true] {
        let transaction = db.begin().await?;
        let result = async {
            replace(&transaction, &case.initial, &case.replacement).await?;
            ensure!(receipt_store::insert(&transaction, &case.receipt).await?);
            auxiliary_store::insert(&transaction, &case.receipt).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_ok() && commit {
            transaction.commit().await?;
        } else {
            transaction.rollback().await?;
        }
        result?;
        if !commit {
            ensure!(
                load(
                    db,
                    &case.initial.metadata.installation,
                    &case.initial.metadata.reference
                )
                .await?
                .context("rolled back slot")?
                .same(&case.initial)
            );
            ensure!(read_receipt(db, case).await?.is_none());
            let count = db
                .query_one(sea_orm::Statement::from_string(
                    db.get_database_backend(),
                    "SELECT COUNT(*) AS n FROM tenancy_receipt_auxiliary".to_owned(),
                ))
                .await?
                .context("aux count")?;
            ensure!(count.try_get::<i64>("", "n")? == 0);
        }
    }
    committed_generation_and_receipt(db, case).await?;
    let transaction = db.begin().await?;
    let replay = async {
        ensure!(!receipt_store::insert(&transaction, &case.receipt).await?);
        auxiliary_store::insert(&transaction, &case.receipt).await
    }
    .await;
    if replay.is_ok() {
        transaction.commit().await?;
    } else {
        transaction.rollback().await?;
    }
    replay?;
    let mut foreign = case.initial.metadata.reference.clone();
    foreign.owner_id = "foreign".try_into()?;
    ensure!(
        load(db, &case.initial.metadata.installation, &foreign)
            .await?
            .is_none()
    );
    ensure!(
        load(
            db,
            &"other-installation".try_into()?,
            &case.initial.metadata.reference
        )
        .await?
        .is_none()
    );
    Ok(())
}
#[tokio::test]
async fn sqlite_real_slot_and_private_receipt_commit_rollback_exact_replay() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    let case = case()?;
    let result = tokio::time::timeout(Duration::from_secs(10), write_and_verify(&db, &case)).await;
    db.close().await?;
    result?
}
