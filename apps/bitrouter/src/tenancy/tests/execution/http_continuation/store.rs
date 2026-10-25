use super::*;
use crate::{
    storage::operation_budget::OperationBudget,
    tenancy::continuation::http::{
        codec::{HttpCrypto, Metadata, Owner},
        store::{self, Row},
    },
};
use sea_orm::TransactionTrait;

fn change_tag(value: &mut String) {
    let replacement = if value.starts_with('0') { "1" } else { "0" };
    value.replace_range(..1, replacement);
}
pub(super) fn owner(ready: &Ready) -> Owner {
    Owner {
        installation: ready.fixture.installation.clone(),
        tenant: ready.fixture.tenant.clone(),
        principal: ready.fixture.principal.clone(),
        route: ready.route.clone(),
    }
}
pub(super) async fn retained(ready: &Ready, public: &str) -> Result<(HttpCrypto, Row)> {
    let crypto = ready
        .fixture
        .service
        .http_continuation
        .state
        .crypto()
        .map_err(|_| anyhow::anyhow!("pinned HTTP crypto"))?
        .clone();
    let check = || Ok(());
    let budget = OperationBudget::new(&check);
    let row = store::load(&ready.fixture.db, &crypto, &owner(ready), public, &budget)
        .await?
        .context("published HTTP row")?;
    Ok((crypto, row))
}

#[tokio::test]
async fn authenticated_metadata_and_ciphertext_cannot_be_transplanted() -> Result<()> {
    let mut server = Server::start().await?;
    let ready = ready(&server).await?;
    let result: Result<()> = async {
        let (_, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
        let public = public_id(&bytes, false)?;
        server.wire().await?;
        let (crypto, row) = retained(&ready, &public).await?;
        let payload = row.open(&crypto)?;
        ensure!(payload.owner == owner(&ready));
        ensure!(
            payload
                .live
                .as_ref()
                .context("live binding")?
                .provider_response_id
                == "resp-http-private-1"
        );
        // Mutations remain syntactically valid where possible: authentication
        // must reject them, rather than relying on a malformed JSON parser.
        let edits: [fn(&mut Metadata); 15] = [
            |m| m.installation_id = "another-installation".into(),
            |m| m.tenant_id = "another-tenant".into(),
            |m| m.creator_principal_id = "another-principal".into(),
            |m| change_tag(&mut m.continuation_identity),
            |m| change_tag(&mut m.owner_identity),
            |m| change_tag(&mut m.target_fingerprint),
            |m| change_tag(&mut m.key_id),
            |m| m.cipher_version = 1,
            |m| m.created_at = "2000-01-01T00:00:00.000000Z".into(),
            |m| m.expires_at = "2100-01-01T00:00:00.000000Z".into(),
            |m| m.purge_after = "2200-01-01T00:00:00.000000Z".into(),
            |m| m.publication_state = "delivering".into(),
            |m| m.publication_generation = uuid::Uuid::new_v4().to_string(),
            |m| m.publication_instance_id = uuid::Uuid::new_v4().to_string(),
            |m| m.publication_lease_until = "2000-01-01T00:00:00.000000Z".into(),
        ];
        for edit in edits {
            let mut metadata = row.metadata.clone();
            edit(&mut metadata);
            ensure!(metadata != row.metadata);
            ensure!(crypto.open(&metadata, &row.ciphertext).is_err());
        }
        let mut ciphertext: Value = serde_json::from_str(&row.ciphertext)?;
        ciphertext["ciphertext"] = "not-an-authenticated-ciphertext".into();
        ensure!(
            crypto
                .open(&row.metadata, &serde_json::to_string(&ciphertext)?)
                .is_err()
        );
        let base = owner(&ready);
        for variant in ["tenant", "principal", "route", "revision"] {
            let mut other = base.clone();
            match variant {
                "tenant" => other.tenant = "another-tenant".try_into()?,
                "principal" => other.principal = "another-principal".try_into()?,
                "route" => other.route.resource_id = "another-route".try_into()?,
                _ => other.route.revision_id = "r2".try_into()?,
            }
            ensure!(crypto.identity(&other, &public)? != row.metadata.continuation_identity);
        }
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}

#[tokio::test]
async fn store_cas_rejects_changed_bytes_and_bounds_corrupt_projection() -> Result<()> {
    let mut server = Server::start().await?;
    let ready = ready(&server).await?;
    let (_, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
    let public = public_id(&bytes, false)?;
    server.wire().await?;
    let (crypto, row) = retained(&ready, &public).await?;
    // Direct store probes intentionally run after producer shutdown. They
    // qualify SQL byte comparisons, not transaction custody or send authority.
    ready.fixture.service.shutdown().await?;
    let result: Result<()> = async {
        let check = || Ok(());
        let budget = OperationBudget::new(&check);
        let tx = ready.fixture.db.begin().await?;
        let mut stale = row.clone();
        stale.metadata.owner_identity = stale.metadata.owner_identity.to_uppercase();
        if stale.metadata.owner_identity == row.metadata.owner_identity {
            stale.metadata.owner_identity.push(' ');
        }
        ensure!(matches!(
            store::delete_in(&tx, &stale, &budget).await,
            Err(store::Error::Integrity)
        ));
        tx.rollback().await?;
        ensure!(
            store::load(&ready.fixture.db, &crypto, &owner(&ready), &public, &budget)
                .await?
                .is_some()
        );
        let tx = ready.fixture.db.begin().await?;
        ensure!(matches!(
            store::insert_in(&tx, &row, &budget).await,
            Err(store::Error::Backend)
        ));
        tx.rollback().await?;
        ensure!(row_count(&ready).await? == 1);
        let tx = ready.fixture.db.begin().await?;
        let large = "x".repeat(crate::tenancy::continuation::http::codec::CELL_LIMIT + 1);
        tx.execute(Statement::from_sql_and_values(
            tx.get_database_backend(),
            "UPDATE provider_continuations SET ciphertext = ? WHERE continuation_identity = ?",
            [
                large.into(),
                row.metadata.continuation_identity.clone().into(),
            ],
        ))
        .await?;
        ensure!(matches!(
            store::load(&tx, &crypto, &owner(&ready), &public, &budget).await,
            Err(store::Error::Integrity)
        ));
        tx.rollback().await?;
        let recovered = store::load(&ready.fixture.db, &crypto, &owner(&ready), &public, &budget)
            .await?
            .context("retained after probes")?;
        ensure!(recovered == row);
        Ok(())
    }
    .await;
    server.close().await?;
    result
}

#[tokio::test]
async fn restart_uses_acknowledged_pin_and_retained_row_without_import_source() -> Result<()> {
    let mut server = Server::start().await?;
    let mut ready = ready(&server).await?;
    let issued = ready.fixture.issue("restart-http-caller").await?;
    let old_credential = issued.credential_id.clone();
    let secret = issued.secret.context("restart credential")?;
    ready.headers = headers(&ready.fixture, &secret, &ready.revision.revision_id)?;
    let (_, bytes) = send(&ready, ready.headers.clone(), body(true, None)).await?;
    let public = public_id(&bytes, true)?;
    server.wire().await?;
    // Rotation changes the request credential, not the stable owner principal.
    let mut rotation = ready.fixture.issuance("rotate-http-caller")?;
    rotation.rotate_credential_id = Some(old_credential);
    let response = ready
        .fixture
        .service
        .execute(
            ready.fixture.operator.clone(),
            crate::tenancy::command::Command::Issue(ready.fixture.tenant.clone(), rotation),
        )
        .await
        .map_err(failure)?;
    let crate::tenancy::store::StoredResponse::Issued(rotated) = response else {
        anyhow::bail!("caller rotation failed");
    };
    let secret = rotated.secret.context("rotated caller secret")?;
    let before = server.state.calls.load(Ordering::SeqCst);
    let (status, _) = send(&ready, ready.headers.clone(), body(false, Some(&public))).await?;
    ensure!(!status.is_success());
    ensure!(server.state.calls.load(Ordering::SeqCst) == before);
    ready.fixture.service.shutdown().await?;
    let initial = crate::tenancy::store::load(&ready.fixture.db, chrono::Utc::now()).await?;
    let resources = crate::tenancy::resources::Resources::with_key(
        ready.fixture.installation.clone(),
        [29; 32],
    )?;
    ready.fixture.service = crate::tenancy::Service::start_prepared(
        ready.fixture.db.clone(),
        None,
        Default::default(),
        initial,
        Arc::new(resources),
    )
    .await?;
    ready
        .fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    ready.headers = headers(&ready.fixture, &secret, &ready.revision.revision_id)?;
    let result: Result<()> = async {
        let (status, bytes) =
            send(&ready, ready.headers.clone(), body(false, Some(&public))).await?;
        ensure!(status == StatusCode::OK);
        public_id(&bytes, false)?;
        ensure!(server.wire().await?["previous_response_id"] == "resp-http-private-1");
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}

#[tokio::test]
async fn retained_http_row_rejects_replacement_epoch_before_catalog_import() -> Result<()> {
    let mut server = Server::start().await?;
    let ready = ready(&server).await?;
    let (_, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
    let public = public_id(&bytes, false)?;
    server.wire().await?;
    let (crypto, original) = retained(&ready, &public).await?;
    ready.fixture.service.shutdown().await?;
    ready
        .fixture
        .db
        .execute_unprepared("DELETE FROM tenancy_resource_revisions")
        .await?;
    let initial = crate::tenancy::store::load(&ready.fixture.db, chrono::Utc::now()).await?;
    let import = serde_json::to_vec(
        &json!({"format":1,"installation_id":ready.fixture.installation,"resources":[{
            "reference":{"owner_kind":"tenant","owner_id":ready.fixture.tenant,"resource_kind":"credential","resource_id":"wrong-key-slot","revision_id":"r1"},
            "definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"must-not-be-imported"}}}
        }]}),
    )?;
    let wrong = crate::tenancy::resources::Resources::with_key(
        ready.fixture.installation.clone(),
        [31; 32],
    )?;
    ensure!(
        crate::tenancy::Service::start_resources(
            ready.fixture.db.clone(),
            None,
            Default::default(),
            initial,
            Arc::new(wrong),
            Some(import),
            None
        )
        .await
        .is_err()
    );
    let row = ready
        .fixture
        .db
        .query_one(Statement::from_string(
            ready.fixture.db.get_database_backend(),
            "SELECT COUNT(*) AS n FROM tenancy_resource_revisions".to_owned(),
        ))
        .await?
        .context("catalog count")?;
    ensure!(row.try_get::<i64>("", "n")? == 0);
    let check = || Ok(());
    let budget = OperationBudget::new(&check);
    ensure!(
        store::load(&ready.fixture.db, &crypto, &owner(&ready), &public, &budget)
            .await?
            .is_some_and(|row| row == original)
    );
    let initial = crate::tenancy::store::load(&ready.fixture.db, chrono::Utc::now()).await?;
    let restored = crate::tenancy::Service::start_prepared(
        ready.fixture.db.clone(),
        None,
        Default::default(),
        initial,
        Arc::new(crate::tenancy::resources::Resources::with_key(
            ready.fixture.installation.clone(),
            [29; 32],
        )?),
    )
    .await?;
    ensure!(restored.current().is_ok());
    restored.shutdown().await?;
    ready
        .fixture
        .db
        .execute_unprepared("DELETE FROM tenancy_continuation_epoch")
        .await?;
    let initial = crate::tenancy::store::load(&ready.fixture.db, chrono::Utc::now()).await?;
    ensure!(
        crate::tenancy::Service::start_prepared(
            ready.fixture.db.clone(),
            None,
            Default::default(),
            initial,
            Arc::new(crate::tenancy::resources::Resources::with_key(
                ready.fixture.installation.clone(),
                [29; 32]
            )?)
        )
        .await
        .is_err()
    );
    let row = ready
        .fixture
        .db
        .query_one(Statement::from_string(
            ready.fixture.db.get_database_backend(),
            "SELECT COUNT(*) AS n FROM tenancy_continuation_epoch".to_owned(),
        ))
        .await?
        .context("epoch count")?;
    ensure!(
        row.try_get::<i64>("", "n")? == 0,
        "missing epoch was silently regenerated around retained HTTP state"
    );
    server.close().await?;
    Ok(())
}
