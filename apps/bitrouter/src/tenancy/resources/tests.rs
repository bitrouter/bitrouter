use anyhow::{Context, Result};
use bitrouter_ai::tenancy::{authority::ResourceKind, configuration::ResourceRef};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::{Value, json};

use super::*;

mod bounds;
mod execution;
mod routing;
mod validation;

fn reference(owner: &str, kind: ResourceKind, name: &str) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: owner.to_owned().try_into()?,
        resource_kind: kind,
        resource_id: name.to_owned().try_into()?,
        revision_id: "v1".to_owned().try_into()?,
    })
}

async fn fixture() -> Result<(DatabaseConnection, Resources)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let resources = Resources::with_key("installation".to_owned().try_into()?, [7; 32])?;
    Ok((db, resources))
}

fn credential(owner: &str) -> Result<Value> {
    Ok(
        json!({"reference": reference(owner, ResourceKind::Credential, "slot")?,
        "definition": {"kind": "credential", "credential": {"type": "api_key", "data": {"value": "PRIVATE_STATIC_TOKEN"}}}}),
    )
}

fn route(owner: &str, name: &str, model: &str) -> Result<Value> {
    Ok(
        json!({"reference": reference(owner, ResourceKind::Route, name)?, "definition": {
            "kind": "route", "selector": format!("fixture:{model}"), "policy": null,
            "routing": {"providers": {"fixture": {"api_base": "https://example.com/v1",
                "headers": {"x-private": "PRIVATE_HEADER_VALUE"}, "models": [{"id":model}]}}},
            "credentials": [{"provider":"fixture", "account":"default", "credential": reference(owner, ResourceKind::Credential, "slot")?}]
        }}),
    )
}

async fn import(resources: &Resources, db: &DatabaseConnection, rows: Vec<Value>) -> Result<()> {
    resources
        .provision_bytes(
            db,
            &serde_json::to_vec(&json!({
                "format":1, "installation_id":resources.installation, "resources":rows
            }))?,
        )
        .await
}

fn document(routes: Vec<ResourceRef>, policy: Option<ResourceRef>) -> Result<Configuration> {
    Ok(serde_json::from_value(
        json!({"routes":routes, "policy":policy,
        "generation_defaults":{"max_output_tokens":128,"temperature":0.5},
        "learning":{"mode":"off","max_evaluations":0},
        "limits":{"active_executions":1,"queued_work":1,"learning_jobs":0}}),
    )?)
}

async fn compile(
    resources: &Resources,
    db: &DatabaseConnection,
    document: &Configuration,
) -> Result<CompiledConfiguration> {
    resources
        .compile(
            db,
            &"tenant".to_owned().try_into()?,
            &"config-v1".to_owned().try_into()?,
            document,
        )
        .await
        .map_err(|code| anyhow::anyhow!("compile failed: {code:?}"))
}

fn decrypt(
    resources: &Resources,
    document: &Configuration,
    compiled: &CompiledConfiguration,
) -> Result<Envelope> {
    let tenant: TenantId = "tenant".to_owned().try_into()?;
    let revision: RevisionId = "config-v1".to_owned().try_into()?;
    let binding = (
        &resources.installation,
        &tenant,
        &revision,
        COMPILER_VERSION,
        document,
        &compiled.dependencies,
    );
    let sealed = serde_json::from_value(compiled.payload["sealed"].clone())?;
    Ok(serde_json::from_slice(
        &resources
            .key
            .as_ref()
            .context("missing fixture key")?
            .open_bound("configuration", &binding, &sealed)?,
    )?)
}

#[tokio::test]
async fn retained_source_is_encrypted_immutable_and_survives_restart_without_manifest() -> Result<()>
{
    let (db, resources) = fixture().await?;
    let input = vec![
        route("tenant", "visible", "model-a")?,
        credential("tenant")?,
    ];
    import(&resources, &db, input.clone()).await?;
    let reference = reference("tenant", ResourceKind::Route, "visible")?;
    let first = super::super::configuration_store::resource(&db, &reference)
        .await?
        .context("missing artifact")?;
    import(&resources, &db, input).await?;
    let second = super::super::configuration_store::resource(&db, &reference)
        .await?
        .context("missing artifact")?;
    assert_eq!(first.payload, second.payload);
    assert_eq!(first.created_at, second.created_at);
    assert!(
        import(
            &resources,
            &db,
            vec![route("tenant", "visible", "model-b")?]
        )
        .await
        .is_err()
    );
    let reopened = Resources::with_key(resources.installation.clone(), [7; 32])?;
    let document = document(vec![reference], None)?;
    let compiled = compile(&reopened, &db, &document).await?;
    assert_eq!(compiled.dependencies.len(), 2);
    let private = decrypt(&reopened, &document, &compiled)?;
    assert_eq!(
        private.routes[0].chains[0].targets[0].target["model"],
        "model-a"
    );
    let serialized = serde_json::to_string(&compiled.payload)?;
    assert!(!serialized.contains("PRIVATE_HEADER_VALUE"));
    assert!(!serialized.contains("PRIVATE_STATIC_TOKEN"));
    let rows = db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT payload_json, dependency_json, content_digest FROM tenancy_resource_revisions"
                .to_owned(),
        ))
        .await?;
    for row in rows {
        for column in ["payload_json", "dependency_json", "content_digest"] {
            let value: String = row.try_get("", column)?;
            assert!(
                !value.contains("PRIVATE_HEADER_VALUE") && !value.contains("PRIVATE_STATIC_TOKEN")
            );
        }
    }
    // Credentials stay separately owned; the compiled copy carries references.
    assert!(!serde_json::to_string(&private)?.contains("PRIVATE_STATIC_TOKEN"));
    Ok(())
}

#[tokio::test]
async fn empty_catalog_has_no_ambient_fallback_and_requires_key_when_populated() -> Result<()> {
    let (db, resources) = fixture().await?;
    let empty = Resources::open(&db, &resources.installation, &TenancyConfig::default()).await?;
    let compiled = compile(&empty, &db, &document(vec![], None)?).await?;
    assert_eq!(compiled.payload["kind"], "empty");
    assert!(compiled.dependencies.is_empty());
    let nonempty = document(
        vec![reference("tenant", ResourceKind::Route, "fixture:model")?],
        None,
    )?;
    assert!(matches!(
        empty
            .compile(
                &db,
                &"tenant".to_owned().try_into()?,
                &"r".to_owned().try_into()?,
                &nonempty
            )
            .await,
        Err(ErrorCode::NotFound)
    ));
    import(&resources, &db, vec![credential("tenant")?]).await?;
    assert!(
        Resources::open(&db, &resources.installation, &TenancyConfig::default())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn route_envelopes_and_equal_names_never_merge_between_owners() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(
        &resources,
        &db,
        vec![
            credential("tenant")?,
            credential("other")?,
            route("tenant", "one", "model-a")?,
            route("tenant", "two", "model-b")?,
            route("other", "one", "model-c")?,
        ],
    )
    .await?;
    let doc = document(
        vec![
            reference("tenant", ResourceKind::Route, "one")?,
            reference("tenant", ResourceKind::Route, "two")?,
        ],
        None,
    )?;
    let compiled = compile(&resources, &db, &doc).await?;
    let envelope = decrypt(&resources, &doc, &compiled)?;
    assert_eq!(envelope.routes.len(), 2);
    for route in &envelope.routes {
        let expected = if route.reference.resource_id.as_str() == "one" {
            "model-a"
        } else {
            "model-b"
        };
        assert!(
            route
                .chains
                .iter()
                .flat_map(|chain| &chain.targets)
                .all(|target| target.target["model"] == expected)
        );
    }
    let duplicate = document(
        vec![
            reference("tenant", ResourceKind::Route, "one")?,
            reference("other", ResourceKind::Route, "one")?,
        ],
        None,
    )?;
    assert!(matches!(
        resources
            .compile(
                &db,
                &"tenant".to_owned().try_into()?,
                &"r".to_owned().try_into()?,
                &duplicate
            )
            .await,
        Err(ErrorCode::InvalidRequest)
    ));
    let foreign = document(vec![reference("other", ResourceKind::Route, "one")?], None)?;
    assert!(matches!(
        resources
            .compile(
                &db,
                &"tenant".to_owned().try_into()?,
                &"r".to_owned().try_into()?,
                &foreign
            )
            .await,
        Err(ErrorCode::Forbidden)
    ));
    Ok(())
}

#[tokio::test]
async fn bad_dependency_and_oauth_imports_roll_back_without_retained_rows() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut bad = route("tenant", "visible", "model")?;
    bad["definition"]["credentials"][0]["credential"] =
        serde_json::to_value(reference("other", ResourceKind::Credential, "slot")?)?;
    assert!(
        import(&resources, &db, vec![credential("tenant")?, bad])
            .await
            .is_err()
    );
    assert!(!storage::has_resources(&db).await?);
    assert!(
        import(&resources, &db, vec![route("tenant", "visible", "model")?])
            .await
            .is_err()
    );
    assert!(!storage::has_resources(&db).await?);
    let mut oauth = credential("tenant")?;
    oauth["definition"]["credential"] = json!({"type":"oauth","data":{"access_token":"secret","refresh_token":"refresh","expires_at":0}});
    assert!(import(&resources, &db, vec![oauth]).await.is_err());
    assert!(!storage::has_resources(&db).await?);
    Ok(())
}

#[tokio::test]
async fn authenticated_metadata_and_storage_failures_have_distinct_error_classes() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(
        &resources,
        &db,
        vec![credential("tenant")?, route("tenant", "visible", "model")?],
    )
    .await?;
    let doc = document(
        vec![reference("tenant", ResourceKind::Route, "visible")?],
        None,
    )?;
    let tenant: TenantId = "tenant".to_owned().try_into()?;
    let revision: RevisionId = "r".to_owned().try_into()?;
    let wrong = Resources::with_key(resources.installation.clone(), [8; 32])?;
    assert!(matches!(
        wrong.compile(&db, &tenant, &revision, &doc).await,
        Err(ErrorCode::AuthorityUnavailable)
    ));
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_resource_revisions SET dependency_json='[]' WHERE resource_kind='route'"
            .to_owned(),
    ))
    .await?;
    assert!(matches!(
        resources.compile(&db, &tenant, &revision, &doc).await,
        Err(ErrorCode::AuthorityUnavailable)
    ));
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DROP TABLE tenancy_resource_revisions".to_owned(),
    ))
    .await?;
    assert!(matches!(
        resources.compile(&db, &tenant, &revision, &doc).await,
        Err(ErrorCode::StorageUnavailable)
    ));
    Ok(())
}

#[tokio::test]
async fn configuration_ciphertext_binds_full_ids_revision_document_and_dependencies() -> Result<()>
{
    let (db, resources) = fixture().await?;
    import(
        &resources,
        &db,
        vec![credential("tenant")?, route("tenant", "visible", "model")?],
    )
    .await?;
    let doc = document(
        vec![reference("tenant", ResourceKind::Route, "visible")?],
        None,
    )?;
    let compiled = compile(&resources, &db, &doc).await?;
    let mut changed = doc.clone();
    changed.generation_defaults.temperature = 1.0;
    assert!(decrypt(&resources, &changed, &compiled).is_err());
    let tenant: TenantId = "tenant".to_owned().try_into()?;
    let revision: RevisionId = "different-revision".to_owned().try_into()?;
    let sealed = serde_json::from_value(compiled.payload["sealed"].clone())?;
    assert!(
        resources
            .key
            .as_ref()
            .context("missing key")?
            .open_bound(
                "configuration",
                &(
                    &resources.installation,
                    &tenant,
                    &revision,
                    COMPILER_VERSION,
                    &doc,
                    &compiled.dependencies
                ),
                &sealed
            )
            .is_err()
    );
    let mut changed = compiled;
    changed.dependencies.clear();
    assert!(decrypt(&resources, &doc, &changed).is_err());
    Ok(())
}

fn policy(owner: &str, id: &str) -> Result<Value> {
    let mut document = crate::policy_lock::PolicyLock::default();
    document.policies.insert(
        "selected".into(),
        crate::policy_lock::PolicyDefinition {
            tiers: [
                (
                    "economy".into(),
                    bitrouter_ai::routing::PolicyModelTarget::model_only("fixture:cheap".into()),
                ),
                (
                    "strong".into(),
                    bitrouter_ai::routing::PolicyModelTarget::model_only("fixture:strong".into()),
                ),
            ]
            .into(),
            default_tier: Some("economy".into()),
            ..Default::default()
        },
    );
    Ok(
        json!({"reference":reference(owner, ResourceKind::Policy, id)?,
        "definition":{"kind":"policy", "name":"selected", "document":document}}),
    )
}

#[tokio::test]
async fn policy_binding_uses_full_reference_and_retains_base_tiers_and_defaults() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut selected = route("tenant", "visible", "base")?;
    let policy_ref = reference("tenant", ResourceKind::Policy, "policy-a")?;
    selected["definition"]["selector"] = json!("bitrouter/choice");
    selected["definition"]["policy"] = serde_json::to_value(&policy_ref)?;
    selected["definition"]["routing"]["routers"] = json!({"choice": {
        "selection":{"kind":"policy", "policy":"selected", "base_model":"fixture:base"},
        "defaults":{"system_prompt":"PRIVATE_SYSTEM", "params":{"temperature":0.9,"top_p":0.8}}
    }});
    import(
        &resources,
        &db,
        vec![
            credential("tenant")?,
            selected,
            policy("tenant", "policy-a")?,
            policy("tenant", "policy-b")?,
        ],
    )
    .await?;
    let roots = vec![reference("tenant", ResourceKind::Route, "visible")?];
    let doc = document(roots.clone(), Some(policy_ref))?;
    let compiled = compile(&resources, &db, &doc).await?;
    let decoded = decrypt(&resources, &doc, &compiled)?;
    let envelope = &decoded.routes[0];
    let models: HashSet<_> = envelope
        .chains
        .iter()
        .flat_map(|c| &c.targets)
        .filter_map(|t| t.target["model"].as_str())
        .collect();
    assert_eq!(models, HashSet::from(["base", "cheap", "strong"]));
    assert_eq!(envelope.defaults["temperature"], 0.5);
    assert_eq!(envelope.defaults["top_p"], 0.8);
    assert_eq!(envelope.defaults["max_output_tokens"], 128);
    assert_eq!(envelope.system_prompt.as_deref(), Some("PRIVATE_SYSTEM"));
    for wrong in [
        None,
        Some(reference("tenant", ResourceKind::Policy, "policy-b")?),
    ] {
        let doc = document(roots.clone(), wrong)?;
        assert!(matches!(
            resources
                .compile(
                    &db,
                    &"tenant".to_owned().try_into()?,
                    &"r".to_owned().try_into()?,
                    &doc
                )
                .await,
            Err(ErrorCode::InvalidRequest)
        ));
    }
    // An authorized but unused policy does not turn a fixed route into a policy route.
    import(
        &resources,
        &db,
        vec![route("tenant", "fixed", "fixed-model")?],
    )
    .await?;
    let fixed = document(
        vec![reference("tenant", ResourceKind::Route, "fixed")?],
        Some(reference("tenant", ResourceKind::Policy, "policy-a")?),
    )?;
    let compiled = compile(&resources, &db, &fixed).await?;
    let decoded = decrypt(&resources, &fixed, &compiled)?;
    assert!(decoded.routes[0].policy.is_none());
    assert!(
        decoded.routes[0]
            .chains
            .iter()
            .flat_map(|c| &c.targets)
            .all(|t| t.target["model"] == "fixed-model")
    );
    Ok(())
}

#[tokio::test]
async fn decisions_are_retained_and_custom_protocols_fail_explicitly() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut decisions = route("tenant", "decide", "judge")?;
    decisions["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
        json!([{ "*": "decisions" }]);
    import(&resources, &db, vec![credential("tenant")?, decisions]).await?;
    let doc = document(
        vec![reference("tenant", ResourceKind::Route, "decide")?],
        None,
    )?;
    let compiled = compile(&resources, &db, &doc).await?;
    let decoded = decrypt(&resources, &doc, &compiled)?;
    assert_eq!(decoded.routes[0].chains.len(), 1);
    assert_eq!(
        decoded.routes[0].chains[0].inbound,
        bitrouter_ai::types::ApiProtocol::Decisions
    );
    let mut custom = route("tenant", "custom", "model")?;
    custom["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
        json!([{ "*": "unqualified-custom" }]);
    let parsed: manifest::Import = serde_json::from_value(custom.clone())?;
    let error = compiler::prepare_route(&parsed, None)
        .err()
        .context("custom protocol accepted")?;
    assert_eq!(
        error.to_string(),
        "custom protocol imports are not qualified"
    );
    assert!(import(&resources, &db, vec![custom]).await.is_err());
    Ok(())
}

#[tokio::test]
async fn catalog_capacity_is_checked_before_committing_an_import() -> Result<()> {
    let (db, resources) = fixture().await?;
    db.execute(Statement::from_string(DatabaseBackend::Sqlite, format!(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<{MAX_CLOSURE}) \
         INSERT INTO tenancy_resource_revisions (owner_kind,owner_id,resource_kind,resource_id,revision_id,compiler_version,payload_json,dependency_json,content_digest,created_at) \
         SELECT 'tenant','other','credential','slot-'||x,'v1','fixture','{{}}','[]','{}','2026-01-01T00:00:00Z' FROM n", "0".repeat(64)))).await?;
    assert!(
        import(&resources, &db, vec![credential("tenant")?])
            .await
            .is_err()
    );
    assert!(
        super::super::configuration_store::resource(
            &db,
            &reference("tenant", ResourceKind::Credential, "slot")?
        )
        .await?
        .is_none()
    );
    assert_eq!(storage::references(&db).await?.len(), MAX_CLOSURE);
    Ok(())
}

#[tokio::test]
async fn wrong_startup_key_cannot_import_rows_before_rejecting_the_old_catalog() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(&resources, &db, vec![credential("tenant")?]).await?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("import.json");
    tokio::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "format":1,"installation_id":"installation","resources":[credential("other")?]
        }))?,
    )
    .await?;
    let wrong = Resources::with_key(resources.installation.clone(), [8; 32])?;
    assert!(wrong.initialize_catalog(&db, Some(&path)).await.is_err());
    assert_eq!(storage::references(&db).await?.len(), 1);
    resources.initialize_catalog(&db, None).await?;
    Ok(())
}
