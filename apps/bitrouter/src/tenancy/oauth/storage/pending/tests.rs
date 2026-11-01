use super::*;
use sea_orm::{DatabaseConnection, Statement, TransactionTrait};

pub(in crate::tenancy) fn fixture(
    installation: &InstallationId,
    tenant: &TenantId,
    execution: &ExecutionId,
    resource: &str,
    at: &str,
) -> Result<(Crypto, Row)> {
    let reference = ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: tenant.as_str().try_into()?,
        resource_kind: ResourceKind::Credential,
        resource_id: resource.try_into()?,
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
        access_token: "synthetic-access".into(),
        refresh_token: Some("synthetic-refresh".into()),
        expires_at: 1,
    };
    let metadata = Metadata {
        format: 1,
        installation: installation.clone(),
        slot: slot(installation, &reference)?,
        seed: crypto.seed(&reference, &profile, &token)?,
        profile,
        reference,
        generation: 0,
        state: State::RefreshStarted(Work {
            id: uuid::Uuid::new_v4().to_string(),
            execution: execution.clone(),
            tenant: tenant.clone(),
            principal: "creator".try_into()?,
            expected_generation: 0,
            started_at: at.try_into()?,
        }),
    };
    let row = Row::new(&crypto, metadata, &token)?;
    Ok((crypto, row))
}
async fn open() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}
#[tokio::test]
async fn indexed_pending_pages_cover_all_slots_and_replacement_clears_only_its_key() -> Result<()> {
    let db = open().await?;
    let installation = "installation".try_into()?;
    let tenant = "tenant".try_into()?;
    let execution = "execution".try_into()?;
    let mut expected = Vec::new();
    for i in 0..67 {
        let (_, row) = fixture(
            &installation,
            &tenant,
            &execution,
            &format!("slot-{i}"),
            "2026-10-01T00:00:00Z",
        )?;
        insert(&db, &row).await?;
        expected.push(row.metadata.slot);
    }
    let (_, foreign) = fixture(
        &installation,
        &"other".try_into()?,
        &execution,
        "foreign",
        "2026-10-01T00:00:00Z",
    )?;
    insert(&db, &foreign).await?;
    expected.sort();
    let mut actual = Vec::new();
    let mut after = None;
    loop {
        let rows = for_execution(
            &db,
            &installation,
            &tenant,
            &execution,
            after.as_deref(),
            &|_, _| Ok(()),
        )
        .await?;
        if rows.is_empty() {
            break;
        }
        ensure!(rows.len() <= PAGE as usize);
        for row in rows {
            after = Some(row.slot.clone());
            actual.push(row.slot);
        }
    }
    ensure!(actual == expected);
    let (crypto, original) = fixture(
        &installation,
        &tenant,
        &execution,
        "transition",
        "2026-10-01T00:00:00Z",
    )?;
    insert(&db, &original).await?;
    let mut metadata = original.metadata.clone();
    metadata.state = State::Ready;
    let ready = Row::new(&crypto, metadata, &original.token(&crypto)?)?;
    let tx = db.begin().await?;
    replace(&tx, &original, &ready).await?;
    tx.commit().await?;
    let scalar=db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT pending_tenant_id,pending_execution_id FROM tenancy_oauth_slots WHERE slot_key=?", [ready.metadata.slot.clone().into()])).await?.context("slot")?;
    ensure!(
        scalar
            .try_get::<Option<String>>("", "pending_tenant_id")?
            .is_none()
            && scalar
                .try_get::<Option<String>>("", "pending_execution_id")?
                .is_none()
    );
    let mut selected = selection(Some(&installation), Some("0"));
    selected
        .and_where(Expr::col(Alias::new("pending_tenant_id")).eq(tenant.as_str()))
        .and_where(Expr::col(Alias::new("pending_execution_id")).eq(execution.as_str()));
    let mut statement = db.get_database_backend().build(&keys(selected));
    statement.sql = format!("EXPLAIN QUERY PLAN {}", statement.sql);
    let plan = db.query_all(statement).await?;
    let text = plan
        .into_iter()
        .map(|row| row.try_get::<String>("", "detail"))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");
    ensure!(text.contains("idx_tenancy_oauth_execution") && !text.contains("USE TEMP B-TREE"));
    let mut statement = db
        .get_database_backend()
        .build(&keys(selection(None, Some("0"))));
    statement.sql = format!("EXPLAIN QUERY PLAN {}", statement.sql);
    let plan = db.query_all(statement).await?;
    let text = plan
        .into_iter()
        .map(|row| row.try_get::<String>("", "detail"))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");
    ensure!(text.contains("INDEX") && !text.contains("USE TEMP B-TREE"));
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn complete_slot_pages_detect_hidden_and_oversized_pending_projections() -> Result<()> {
    for mutation in [
        "UPDATE tenancy_oauth_slots SET pending_tenant_id=NULL,pending_execution_id=NULL",
        "UPDATE tenancy_oauth_slots SET state='ready',pending_tenant_id=NULL,pending_execution_id=NULL",
        "UPDATE tenancy_oauth_slots SET pending_execution_id='different'",
        "UPDATE tenancy_oauth_slots SET pending_tenant_id=zeroblob(1048576)",
    ] {
        let db = open().await?;
        let installation = "installation".try_into()?;
        let (_, row) = fixture(
            &installation,
            &"tenant".try_into()?,
            &"execution".try_into()?,
            "slot",
            "2026-10-01T00:00:00Z",
        )?;
        insert(&db, &row).await?;
        db.execute_unprepared(mutation).await?;
        ensure!(
            page(&db, &installation, None, &|_, _| Ok(()))
                .await
                .is_err(),
            "accepted {mutation}"
        );
        db.close().await?;
    }
    Ok(())
}

/// Canonical synthetic row in the exact pre043 columns for migration fixtures.
pub(in crate::tenancy) async fn insert_legacy(db: &impl ConnectionTrait, row: &Row) -> Result<()> {
    db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "INSERT INTO tenancy_oauth_slots (slot_key,installation_id,owner_kind,owner_id,credential_resource_id,authority_revision,generation,state,metadata_json,payload_json,content_digest) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        [row.metadata.slot.clone().into(),row.metadata.installation.as_str().into(),owner(&row.metadata.reference)?.into(),row.metadata.reference.owner_id.as_str().into(),row.metadata.reference.resource_id.as_str().into(),row.metadata.reference.revision_id.as_str().into(),i64::try_from(row.metadata.generation)?.into(),row.metadata.state.name().into(),row.json.clone().into(),row.payload.clone().into(),row.digest.clone().into()])).await?;
    Ok(())
}
