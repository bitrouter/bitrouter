//! Only open_profile starts maintenance here. The child receives its operator
//! key through Command.env; no global environment or direct maintenance call.
use super::*;
use crate::{
    storage::operation_budget::OperationBudget,
    tenancy::{
        Service,
        continuation::http::{
            codec::{HttpCrypto, Owner, timestamp},
            store::{self as sql, Row},
        },
    },
};
use base64::Engine;
use sea_orm::{DatabaseConnection, TransactionTrait};
use serde::{Deserialize, Serialize};
use std::path::Path;

const DIRECTORY: &str = "BITROUTER_TEST_HTTP034_RESTART_DIRECTORY";
const KEY: &str = "BITROUTER_TEST_HTTP034_RESTART_KEY";
const ACK: &[u8] = b"http034-open-profile-auto-maintenance-v1";

#[derive(Serialize, Deserialize)]
struct Transfer {
    owner: Owner,
    public: Vec<String>,
    legacy: Value,
}

async fn legacy(db: &DatabaseConnection) -> Result<Value> {
    let row = db.query_one(Statement::from_string(db.get_database_backend(),
        "SELECT continuation_identity,owner_identity,ciphertext,nonce,target_fingerprint,key_id,cipher_version,created_at,expires_at,purge_after,publication_state,publication_generation,publication_instance_id,publication_lease_until,installation_id,tenant_id,creator_principal_id FROM provider_continuations WHERE continuation_identity = 'LegacyExact  '"
    )).await?.context("Legacy fixture")?;
    let mut fields = serde_json::Map::new();
    for name in [
        "continuation_identity",
        "owner_identity",
        "ciphertext",
        "nonce",
        "target_fingerprint",
        "key_id",
        "created_at",
        "expires_at",
        "purge_after",
        "publication_state",
        "publication_generation",
        "publication_instance_id",
        "publication_lease_until",
        "installation_id",
        "tenant_id",
        "creator_principal_id",
    ] {
        fields.insert(
            name.into(),
            serde_json::to_value(row.try_get::<Option<String>>("", name)?)?,
        );
    }
    fields.insert(
        "cipher_version".into(),
        row.try_get::<i32>("", "cipher_version")?.into(),
    );
    Ok(Value::Object(fields))
}

async fn seed(directory: &Path) -> Result<()> {
    let mut provider = Server::start().await?;
    let path = directory.join("state.sqlite");
    let lease = crate::tenancy::ownership::acquire_path(&path, true)?;
    let db = crate::db::connect_tenancy(&format!("sqlite://{}?mode=rwc", path.display()), &lease)
        .await?;
    let fixture = Fixture::with_database(db.clone(), Default::default(), &mut |_| {}).await?;
    let (fixture, route, _) = super::super::provision_protocol_with_fixture(
        fixture,
        &provider.url,
        "responses",
        &mut |_| {},
    )
    .await?;
    let revision = draft(&fixture, &route, "production-restart-draft", 32).await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let issued = fixture.issue("production-restart-caller").await?;
    let headers = headers(
        &fixture,
        issued.secret.as_deref().context("caller")?,
        &revision.revision_id,
    )?;
    let ready = Ready {
        fixture,
        headers,
        route,
        revision,
    };
    let outcome: Result<()> = async {
        let mut published = Vec::new();
        for stream in [false, true, false] {
            let (status, bytes) = send(&ready, ready.headers.clone(), body(stream, None)).await?;
            ensure!(status == StatusCode::OK, "fixture issuance failed: {}", String::from_utf8_lossy(&bytes));
            let public = public_id(&bytes, stream)?;
            provider.wire().await?;
            published.push((public.clone(), store::retained(&ready, &public).await?.1));
        }
        let owner = store::owner(&ready);
        let crypto = ready.fixture.service.http_continuation.state.crypto()
            .map_err(|_| anyhow::anyhow!("pinned HTTP key"))?.clone();
        // Rewrite only authenticated fixture state after the serving writer has
        // joined. The next process must perform every cleanup transition.
        ready.fixture.service.shutdown().await?;
        let check = || Ok(());
        let budget = OperationBudget::new(&check);
        let tx = db.begin().await?;
        for (index, (_, original)) in published.iter().enumerate() {
            let now = chrono::Utc::now();
            let mut metadata = original.metadata.clone();
            metadata.created_at = timestamp(now - chrono::TimeDelta::days(3));
            metadata.publication_lease_until = timestamp(now - chrono::TimeDelta::hours(1));
            match index {
                0 => metadata.publication_state = "provisional".into(),
                1 => {
                    metadata.expires_at = timestamp(now - chrono::TimeDelta::days(1));
                    metadata.purge_after = timestamp(now + chrono::TimeDelta::days(1));
                }
                _ => {
                    metadata.expires_at = timestamp(now - chrono::TimeDelta::days(2));
                    metadata.purge_after = timestamp(now - chrono::TimeDelta::days(1));
                }
            }
            let changed = Row::seal(&crypto, metadata, &original.open(&crypto)?)?;
            sql::replace_in(&tx, original, &changed, &budget).await?;
        }
        tx.execute_unprepared("INSERT INTO provider_continuations (continuation_identity, owner_identity, ciphertext, nonce, target_fingerprint, key_id, cipher_version, created_at, expires_at, purge_after, publication_state, publication_generation, publication_instance_id, publication_lease_until) VALUES ('LegacyExact  ', 'LegacyOwner', 'Legacy opaque bytes  ', 'legacy-nonce', 'legacy-target', 'legacy-key', 1, 'legacy-created', '0001', '0001', 'provisional', 'legacy-generation', 'legacy-producer', '0001')").await?;
        tx.commit().await?;
        ensure!(row_count(&ready).await? == 3);
        let transfer = Transfer { owner, public: published.into_iter().map(|(public, _)| public).collect(), legacy: legacy(&db).await? };
        std::fs::write(directory.join("transfer.json"), serde_json::to_vec(&transfer)?)?;
        Ok(())
    }.await;
    ready.fixture.service.shutdown().await?;
    drop(ready);
    db.close().await?;
    drop(lease);
    provider.close().await?;
    outcome
}

async fn completed(
    db: &DatabaseConnection,
    crypto: &HttpCrypto,
    transfer: &Transfer,
) -> Result<bool> {
    ensure!(transfer.public.len() == 3);
    let check = || Ok(());
    let budget = OperationBudget::new(&check);
    let mut complete = true;
    for (index, public) in transfer.public.iter().enumerate() {
        let row = sql::load(db, crypto, &transfer.owner, public, &budget).await?;
        if index == 1 {
            let row = row.context("expiration must keep its authenticated tombstone")?;
            if row.open(crypto)?.live.is_some() {
                complete = false;
            } else {
                ensure!(row.metadata.publication_state == "active");
                ensure!(matches!(
                    sql::resolve_in(
                        db,
                        crypto,
                        &transfer.owner,
                        public,
                        chrono::Utc::now(),
                        &budget
                    )
                    .await?,
                    sql::Resolution::Expired
                ));
            }
        } else {
            complete &= row.is_none();
        }
    }
    Ok(complete)
}

async fn consume(directory: &Path) -> Result<()> {
    let transfer: Transfer =
        serde_json::from_slice(&std::fs::read(directory.join("transfer.json"))?)?;
    let path = directory.join("state.sqlite");
    let lease = crate::tenancy::ownership::acquire_path(&path, false)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}?mode=rw", path.display()), &lease).await?;
    let before = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT COUNT(*) AS n FROM provider_continuations WHERE installation_id IS NOT NULL",
        ))
        .await?
        .context("persisted fixture")?
        .try_get::<i64>("", "n")?;
    ensure!(before == 3 && legacy(&db).await? == transfer.legacy);
    // Actual production entry: only this call may launch reconciliation. No
    // fixture start_prepared, start_reconciler, maintain or drain call is used.
    let service = Service::open_profile(
        db.clone(),
        Some(lease),
        &crate::config::tenancy::TenancyConfig {
            enabled: true,
            wrapping_key_env: Some(KEY.into()),
            resource_import: None,
            learning_registrations: None,
            ..Default::default()
        },
        &crate::config::ContinuationConfig::default(),
    )
    .await?;
    let outcome: Result<()> = async {
        let crypto = service
            .http_continuation
            .state
            .crypto()
            .map_err(|_| anyhow::anyhow!("production pinned key"))?;
        tokio::time::timeout(Duration::from_secs(15), async {
            while !completed(&db, crypto, &transfer).await? {
                ensure!(service.current().is_ok());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        // Check before shutdown: its explicit drain cannot cause a false pass
        // when the automatic production worker was never started.
        ensure!(legacy(&db).await? == transfer.legacy);
        ensure!(service.current().is_ok());
        Ok(())
    }
    .await;
    service.shutdown().await?;
    service.cleanup.ensure_clean()?;
    drop(service);
    db.close().await?;
    outcome?;
    // Successful shutdown also released the concrete cross-process writer lease.
    drop(crate::tenancy::ownership::acquire_path(&path, false)?);
    std::fs::write(directory.join("automatic-maintenance-passed"), ACK)?;
    Ok(())
}

#[tokio::test]
#[ignore = "child of production_profile_restart_automatically_reconciles_persisted_owned_rows"]
async fn production_profile_restart_child() -> Result<()> {
    let directory =
        std::path::PathBuf::from(std::env::var_os(DIRECTORY).context("restart directory")?);
    ensure!(directory.is_absolute());
    tokio::time::timeout(Duration::from_secs(25), consume(&directory)).await?
}

#[tokio::test]
async fn production_profile_restart_automatically_reconciles_persisted_owned_rows() -> Result<()> {
    let directory = tempfile::tempdir()?;
    tokio::time::timeout(Duration::from_secs(20), seed(directory.path())).await??;
    ensure!(std::fs::metadata(directory.path().join("state.sqlite"))?.len() > 0);
    let status = tokio::time::timeout(Duration::from_secs(30), tokio::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "tenancy::tests::execution::http_continuation::production_restart::production_profile_restart_child", "--ignored", "--nocapture"])
        .env(DIRECTORY, directory.path())
        .env(KEY, base64::engine::general_purpose::STANDARD.encode([29; 32]))
        .kill_on_drop(true).status()).await??;
    ensure!(
        status.success(),
        "production restart child failed: {status}"
    );
    // libtest can return success for zero matches; require the real consumer ACK.
    ensure!(std::fs::read(directory.path().join("automatic-maintenance-passed"))? == ACK);
    Ok(())
}
