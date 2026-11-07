use super::*;
use bitrouter_ai::tenancy::{authority::OwnerKind, identity::InstallationId};
use sea_orm::DatabaseConnection;
use serde_json::{Value, json};

mod compiled;

async fn fixture() -> Result<(DatabaseConnection, Resources)> {
    fixture_with_key(rand::random()).await
}

async fn fixture_with_key(key: [u8; 32]) -> Result<(DatabaseConnection, Resources)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let state = crate::tenancy::store::load(&db, chrono::Utc::now()).await?;
    let resources = Resources::with_key(state.installation, key)?;
    Ok((db, resources))
}

fn reference(kind: ResourceKind, id: &str, revision: &str) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::OperatorPool,
        owner_id: "pool".try_into()?,
        resource_kind: kind,
        resource_id: id.try_into()?,
        revision_id: revision.try_into()?,
    })
}

fn profile(revision: &str) -> Result<Value> {
    Ok(
        json!({"reference":reference(ResourceKind::Policy,"profile",revision)?,
        "definition":{"kind":"admission_profile","profile":{"schema_version":1,
            "rate":{"rate_group_id":"shared","member_rpm":0},"health":null,"fallback":"stop"}}}),
    )
}

fn credential() -> Result<Value> {
    Ok(
        json!({"reference":reference(ResourceKind::Credential,"credential","v1")?,
        "definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"synthetic-admission-test"}}}}),
    )
}

fn route(revision: &str, profiled: bool) -> Result<Value> {
    let credential = reference(ResourceKind::Credential, "credential", "v1")?;
    let mut definition = json!({"kind":"route","selector":"fixture:model","policy":null,
        "routing":{"providers":{"fixture":{"api_base":"http://127.0.0.1:1/v1",
            "api_protocol":[{"*":["chat_completions","messages","responses"]}],"models":[{"id":"model"}]}}},
        "credentials":[{"provider":"fixture","account":"default","credential":credential}]});
    if profiled {
        definition["kind"] = json!("profiled_route");
        definition["attachments"] = json!(
            ["chat_completions", "messages", "responses"]
                .into_iter()
                .map(|protocol| Ok(
                    json!({"inbound_protocol":protocol,"candidate_index":0,"credential":credential,
                "profile":reference(ResourceKind::Policy,"profile","v1")?})
                ))
                .collect::<Result<Vec<_>>>()?
        );
    }
    Ok(
        json!({"reference":reference(ResourceKind::Route,"route",revision)?,"definition":definition}),
    )
}

fn manifest(installation: &InstallationId, rows: Vec<Value>) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(
        &json!({"format":1,"installation_id":installation,"resources":rows}),
    )?)
}

async fn import(resources: &Resources, db: &DatabaseConnection, rows: Vec<Value>) -> Result<()> {
    resources
        .import_bytes(db, &manifest(&resources.installation, rows)?)
        .await
}

async fn state(resources: &Resources, db: &DatabaseConnection) -> Result<State> {
    resources.admission_state(db, 32 * 1024 * 1024).await
}

#[tokio::test]
async fn protected_key_survives_old_and_new_legacy_imports_and_restart() -> Result<()> {
    let key = rand::random();
    let (db, resources) = fixture_with_key(key).await?;
    import(
        &resources,
        &db,
        vec![credential()?, route("legacy", false)?],
    )
    .await?;
    let legacy = reference(ResourceKind::Route, "route", "legacy")?;
    ensure!(!state(&resources, &db).await?.protected(&legacy));
    import(
        &resources,
        &db,
        vec![profile("v1")?, route("profiled", true)?],
    )
    .await?;
    let protected = reference(ResourceKind::Route, "route", "profiled")?;
    let observed = state(&resources, &db).await?;
    ensure!(observed.protected(&legacy) && observed.protected(&protected));
    ensure!(
        observed
            .configuration(std::slice::from_ref(&legacy))
            .is_err()
    );
    ensure!(
        observed
            .configuration(std::slice::from_ref(&protected))
            .is_ok()
    );
    import(&resources, &db, vec![route("later-legacy", false)?]).await?;
    let restarted = Resources::with_key(resources.installation.clone(), key)?;
    restarted.authenticate_retained(&db).await?;
    let reloaded = state(&restarted, &db).await?;
    ensure!(reloaded.protected(&reference(ResourceKind::Route, "route", "later-legacy")?));
    let mut different = legacy.clone();
    different.owner_id = "other".try_into()?;
    ensure!(!reloaded.protected(&different));
    different = legacy.clone();
    different.resource_id = "other".try_into()?;
    ensure!(!reloaded.protected(&different));
    Ok(())
}

#[tokio::test]
async fn malformed_associations_roll_back_profiles_credentials_and_protection_together()
-> Result<()> {
    let (db, resources) = fixture().await?;
    for negative in 0..7 {
        let mut invalid = route("profiled", true)?;
        let attachments = invalid["definition"]["attachments"]
            .as_array_mut()
            .context("attachments")?;
        match negative {
            0 => {
                attachments.pop();
            }
            1 => attachments[0]["candidate_index"] = json!(1),
            2 => attachments[0]["credential"]["revision_id"] = json!("wrong"),
            3 => attachments[0]["profile"]["revision_id"] = json!("wrong"),
            4 => attachments[0]["inbound_protocol"] = json!("responses"),
            5 => attachments[0]["profile"]["owner_kind"] = json!("tenant"),
            _ => {
                attachments.push(attachments[0].clone());
            }
        }
        ensure!(
            import(
                &resources,
                &db,
                vec![credential()?, profile("v1")?, invalid]
            )
            .await
            .is_err(),
            "case {negative}"
        );
        ensure!(storage::references(&db).await?.is_empty());
        ensure!(!state(&resources, &db).await?.protected(&reference(
            ResourceKind::Route,
            "route",
            "profiled"
        )?));
    }
    Ok(())
}

#[tokio::test]
async fn initial_profile_head_is_unambiguous_and_reimport_cannot_repair_missing_authority()
-> Result<()> {
    let (db, resources) = fixture().await?;
    ensure!(
        import(&resources, &db, vec![profile("v1")?, profile("v2")?])
            .await
            .is_err()
    );
    ensure!(storage::references(&db).await?.is_empty());
    import(&resources, &db, vec![profile("v1")?]).await?;
    import(&resources, &db, vec![profile("v1")?]).await?;
    let profile_ref = reference(ResourceKind::Policy, "profile", "v1")?;
    let current = lifecycle::head(&db, &resources.installation, &profile_ref.key())
        .await?
        .context("profile head")?;
    ensure!(current.reference == profile_ref);
    ensure!(import(&resources, &db, vec![profile("v2")?]).await.is_err());
    db.execute_unprepared("DELETE FROM tenancy_resource_lifecycle")
        .await?;
    ensure!(state(&resources, &db).await.is_err());
    ensure!(import(&resources, &db, vec![profile("v1")?]).await.is_err());
    Ok(())
}

#[tokio::test]
async fn profile_configuration_authenticates_new_subtype_and_exact_dependencies() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(
        &resources,
        &db,
        vec![credential()?, profile("v1")?, route("profiled", true)?],
    )
    .await?;
    let root = reference(ResourceKind::Route, "route", "profiled")?;
    let document = serde_json::from_value(json!({"routes":[root],"policy":null,
        "generation_defaults":{"max_output_tokens":128,"temperature":0.5},
        "learning":{"mode":"off","max_evaluations":0},
        "limits":{"active_executions":1,"queued_work":1,"learning_jobs":0}}))?;
    let compiled = resources
        .compile(
            &db,
            &"tenant".try_into()?,
            &"config-v1".try_into()?,
            &document,
        )
        .await
        .map_err(|error| anyhow::anyhow!("compile: {error:?}"))?;
    ensure!(compiled.compiler_version == super::super::PROFILED_COMPILER_VERSION);
    ensure!(compiled.dependencies.len() == 3);
    let tenant: bitrouter_ai::tenancy::identity::TenantId = "tenant".try_into()?;
    let revision: bitrouter_ai::tenancy::identity::RevisionId = "config-v1".try_into()?;
    let binding = (
        &resources.installation,
        &tenant,
        &revision,
        compiled.compiler_version.as_str(),
        &document,
        &compiled.dependencies,
    );
    let sealed = serde_json::from_value(compiled.payload["sealed"].clone())?;
    let bytes =
        resources
            .key
            .as_ref()
            .context("key")?
            .open_bound("configuration", &binding, &sealed)?;
    let envelope: super::super::Envelope = serde_json::from_slice(&bytes)?;
    ensure!(envelope.format == 4 && bytes.len() <= super::super::MAX_ARTIFACT_BYTES);
    ensure!(envelope.routes.is_empty());
    let row = crate::tenancy::resources::tests::validation::stored(document, compiled)?;
    resources
        .open_execution(&row)
        .map_err(|code| anyhow::anyhow!("readback: {code:?}"))?;
    Ok(())
}
