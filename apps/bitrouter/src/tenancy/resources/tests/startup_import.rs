//! New startup imports are Tenant-only; old sealed platform data stays readable.

use super::*;
use bitrouter_ai::tenancy::ordered::GenerationProtocol;
use sea_orm::{ConnectionTrait, TransactionTrait};

async fn startup_fixture() -> Result<(DatabaseConnection, Resources, [u8; 32])> {
    let (db, initial) = fixture().await?;
    let key = rand::random();
    let resources = Resources::with_key(initial.installation, key)?;
    Ok((db, resources, key))
}

fn configure(resources: &Resources, enabled: bool) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    let secret = uuid::Uuid::new_v4().to_string();
    let config = crate::config::parse(&format!(
        r#"
inherit_defaults: false
server:
  skip_auth: false
control:
  enabled: true
  credentials:
    - id: fixture-operator
      token_env: STARTUP_IMPORT_OPERATOR_TOKEN
      scopes: [control:tenancy]
providers:
  fixture:
    api_base: https://fixture.invalid/v1
    api_key: {secret:?}
    api_protocol: [{{"*": chat_completions}}]
    models: [{{id: acme/model, provider_model_id: model}}]
models:
  acme/model:
    endpoints: [{{provider: fixture, service_id: model}}]
tenancy:
  enabled: true
  wrapping_key_env: STARTUP_IMPORT_WRAPPING_KEY
  catalog:
    pool_id: platform
    routes: {{model.acme.model: acme/model}}
    profiles:
      fixture/default/acme/model:
        schema_version: 2
        rate: null
        health: null
        health_group_id: null
        fallback: stop
"#
    ))?;
    resources.configure(&config)
}

fn pool_reference(kind: ResourceKind, id: &str) -> Result<ResourceRef> {
    let mut reference = reference("old-pool", kind, id)?;
    reference.owner_kind = OwnerKind::OperatorPool;
    Ok(reference)
}

fn profile(version: u8) -> Result<Value> {
    let mut profile = json!({"schema_version":version,"rate":null,"health":null,"fallback":"stop"});
    if version == 2 {
        profile["health_group_id"] = Value::Null;
    }
    Ok(
        json!({"reference":pool_reference(ResourceKind::Policy,"profile")?,"definition":{
        "kind":if version==1 {"admission_profile"} else {"admission_profile_v2"},"profile":profile}}),
    )
}

fn profiled(native: bool, pool_owned: bool) -> Result<Value> {
    let mut row = route("tenant", "profiled", "model")?;
    let credential = if pool_owned {
        pool_reference(ResourceKind::Credential, "slot")?
    } else {
        reference("tenant", ResourceKind::Credential, "slot")?
    };
    if pool_owned {
        row["reference"] = serde_json::to_value(pool_reference(ResourceKind::Route, "profiled")?)?;
    }
    row["definition"]["kind"] = json!(if native {
        "native_profiled_route_v1"
    } else {
        "profiled_route"
    });
    row["definition"]["credentials"][0]["credential"] = serde_json::to_value(&credential)?;
    row["definition"]["attachments"] = json!([{
        "inbound_protocol":if native {"generate_content"} else {"chat_completions"},
        "candidate_index":0,"credential":credential,
        "profile":pool_reference(ResourceKind::Policy,"profile")?,
    }]);
    Ok(row)
}

fn forbidden_rows() -> Result<Vec<Value>> {
    let mut credential = credential("old-pool")?;
    credential["reference"] =
        serde_json::to_value(pool_reference(ResourceKind::Credential, "slot")?)?;
    let mut route = route("old-pool", "route", "model")?;
    route["reference"] = serde_json::to_value(pool_reference(ResourceKind::Route, "route")?)?;
    route["definition"]["credentials"][0]["credential"] =
        serde_json::to_value(pool_reference(ResourceKind::Credential, "slot")?)?;
    let mut policy = policy("old-pool", "policy")?;
    policy["reference"] = serde_json::to_value(pool_reference(ResourceKind::Policy, "policy")?)?;
    Ok(vec![
        credential,
        route,
        policy,
        profile(1)?,
        profile(2)?,
        profiled(false, false)?,
        profiled(false, true)?,
        profiled(true, false)?,
        profiled(true, true)?,
    ])
}

fn manifest_bytes(resources: &Resources, rows: Vec<Value>) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &json!({"format":3,"installation_id":resources.installation,"resources":rows}),
    )?)
}

async fn counts(db: &impl ConnectionTrait) -> Result<Vec<i64>> {
    let mut counts = Vec::new();
    for table in [
        "tenancy_resource_revisions",
        "tenancy_resource_lifecycle",
        "tenancy_admission_index",
    ] {
        counts.push(
            db.query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT count(*) AS n FROM {table}"),
            ))
            .await?
            .context("resource count")?
            .try_get("", "n")?,
        );
    }
    Ok(counts)
}

#[tokio::test]
async fn platform_startup_import_is_rejected_before_any_batch_write_with_or_without_catalog()
-> Result<()> {
    for enabled in [false, true] {
        let (db, resources, _) = startup_fixture().await?;
        configure(&resources, enabled)?;
        let before = counts(&db).await?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("startup-import.json");
        for forbidden in forbidden_rows()? {
            // Include an allowed first entry: rejection must precede every write.
            let bytes = manifest_bytes(&resources, vec![credential("tenant")?, forbidden])?;
            let manifest: manifest::Manifest = serde_json::from_slice(&bytes)?;
            for resource in &manifest.resources {
                resource.validate()?;
            }
            tokio::fs::write(&path, &bytes).await?;
            let error = Resources::read_import(Some(&path))
                .await
                .err()
                .context("platform file accepted")?;
            ensure!(
                error
                    .to_string()
                    .contains("tenant-owned unprofiled definitions")
            );
            let transaction = db.begin().await?;
            let error = resources
                .bootstrap(&transaction, Some(&bytes))
                .await
                .err()
                .context("platform bytes accepted")?;
            ensure!(
                error
                    .to_string()
                    .contains("tenant-owned unprofiled definitions")
            );
            ensure!(
                counts(&transaction).await? == before,
                "forbidden batch wrote rows before refusal"
            );
            transaction.rollback().await?;
            ensure!(
                counts(&db).await? == before,
                "forbidden batch changed retained authority"
            );
            ensure!(resources.configured().is_some() == enabled);
        }
    }
    Ok(())
}

#[tokio::test]
async fn tenant_route_policy_and_byok_still_import_and_compile_with_or_without_catalog()
-> Result<()> {
    for enabled in [false, true] {
        let (db, resources, _) = startup_fixture().await?;
        configure(&resources, enabled)?;
        let bytes = manifest_bytes(
            &resources,
            vec![
                credential("tenant")?,
                policy("tenant", "policy")?,
                route("tenant", "route", "model")?,
            ],
        )?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("tenant-import.json");
        tokio::fs::write(&path, &bytes).await?;
        let read = Resources::read_import(Some(&path))
            .await?
            .context("tenant manifest missing")?;
        ensure!(read == bytes);
        let transaction = db.begin().await?;
        resources.bootstrap(&transaction, Some(&read)).await?;
        transaction.commit().await?;
        resources.authenticate_retained(&db).await?;
        let document = document(
            vec![reference("tenant", ResourceKind::Route, "route")?],
            None,
        )?;
        let compiled = compile(&resources, &db, &document).await?;
        ensure!(decrypt(&resources, &document, &compiled)?.routes.len() == 1);
        ensure!(counts(&db).await?[0] == 3);
    }
    Ok(())
}

async fn stored_rows(db: &DatabaseConnection, references: &[ResourceRef]) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    for reference in references {
        let row = crate::tenancy::configuration_store::resource(db, reference)
            .await?
            .context("historical row")?;
        rows.push(json!({"reference":row.reference,"compiler":row.compiler_version,"payload":row.payload,
            "dependencies":row.dependencies,"digest":row.content_digest,"created_at":row.created_at}));
    }
    Ok(rows)
}

#[tokio::test]
async fn old_platform_native_artifacts_authenticate_without_reenabling_the_startup_writer()
-> Result<()> {
    let (db, resources, key) = startup_fixture().await?;
    let slot = pool_reference(ResourceKind::Credential, "slot")?;
    let rules = pool_reference(ResourceKind::Policy, "profile")?;
    let route = pool_reference(ResourceKind::Route, "native")?;
    let mut credential = credential("old-pool")?;
    credential["reference"] = serde_json::to_value(&slot)?;
    let attachments = [GenerationProtocol::ChatCompletions,GenerationProtocol::Messages,
        GenerationProtocol::Responses,GenerationProtocol::GenerateContent].into_iter()
        .map(|protocol| json!({"inbound_protocol":protocol,"candidate_index":0,"credential":slot,"profile":rules})).collect::<Vec<_>>();
    let native = json!({"reference":route,"definition":{"kind":"native_profiled_route_v1",
        "selector":"fixture:model","policy":null,
        "routing":{"providers":{"fixture":{"api_base":"https://fixture.invalid/v1",
            "api_protocol":[{"*":["ordered_generate_content","chat_completions","messages","responses"]}],
            "models":[{"id":"model"}]}}},
        "credentials":[{"provider":"fixture","account":"default","credential":slot}],"attachments":attachments}});
    let bytes = manifest_bytes(&resources, vec![credential, profile(2)?, native])?;
    resources.seed_history_bytes(&db, &bytes).await?;
    let references = vec![slot, rules, route];
    let before = stored_rows(&db, &references).await?;
    let before_counts = counts(&db).await?;
    for enabled in [false, true] {
        let reopened = Resources::with_key(resources.installation.clone(), key)?;
        configure(&reopened, enabled)?;
        let transaction = db.begin().await?;
        reopened.bootstrap(&transaction, None).await?;
        transaction.commit().await?;
        for reference in &references {
            reopened
                .load(&db, reference)
                .await
                .map_err(|_| anyhow::anyhow!("old artifact failed authentication"))?;
        }
        let transaction = db.begin().await?;
        ensure!(
            reopened
                .bootstrap(&transaction, Some(&bytes))
                .await
                .is_err()
        );
        transaction.rollback().await?;
        ensure!(stored_rows(&db, &references).await? == before);
        ensure!(counts(&db).await? == before_counts);
    }
    let wrong = Resources::with_key(resources.installation.clone(), rand::random())?;
    let transaction = db.begin().await?;
    ensure!(wrong.bootstrap(&transaction, None).await.is_err());
    transaction.rollback().await?;
    ensure!(stored_rows(&db, &references).await? == before);
    Ok(())
}
