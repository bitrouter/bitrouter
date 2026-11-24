//! Explicit audit and selected reads validate isolated retained Eval rows.
use super::*;
use crate::tenancy::resources::Resources;
use bitrouter_ai::{
    eval::EvalSubjectIdentity,
    tenancy::{
        authority::{OwnerKind, ResourceKey, ResourceKind},
        configuration::ResourceRef,
        resources::{CredentialBinding, Definition, ResourcePut},
    },
};
use sea_orm::{ConnectionTrait, Statement, TransactionTrait};
use std::sync::Arc;
use std::{os::unix::fs::PermissionsExt, path::Path};

const KEY_ENV: &str = "BITROUTER_TEST_RECOVERY_KEY";

async fn open_keyed(
    database: crate::db::OwnedDatabase,
    resources: Arc<Resources>,
) -> Result<Service> {
    let (db, owner, claim) = database
        .claim()
        .context("claiming retained Eval database")?;
    let initial = super::super::super::store::initial_header(&db, Utc::now())
        .await
        .context("reading retained Eval startup header")?;
    Service::start_resources_with_continuations(
        db,
        Some(owner),
        initial,
        resources,
        crate::tenancy::Provisioning {
            import: None,
            continuations: &crate::config::ContinuationConfig::default(),
            claim: Some(claim),
            receipt_observer: crate::telemetry::receipt_latency::Sink::default(),
            config: &crate::config::tenancy::TenancyConfig::default(),
        },
        None,
    )
    .await
    .context("starting retained Eval Service")
}

async fn model_resource(
    fixture: &Fixture,
    kind: ResourceKind,
    id: &str,
    definition: Definition,
) -> Result<ResourceRef> {
    let key = ResourceKey {
        owner_kind: OwnerKind::Tenant,
        owner_id: fixture.tenant.as_str().try_into()?,
        resource_kind: kind,
        resource_id: id.try_into()?,
    };
    let put = ResourcePut {
        format: 1,
        operation_id: format!("retained-{id}").try_into()?,
        installation_id: fixture.installation.clone(),
        key: key.clone(),
        expected_revision: None,
        definition,
    };
    let applied = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Resource(fixture.tenant.clone(), put),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok(ResourceRef {
        owner_kind: key.owner_kind,
        owner_id: key.owner_id,
        resource_kind: key.resource_kind,
        resource_id: key.resource_id,
        revision_id: applied
            .resource_revision
            .context("retained resource revision")?,
    })
}

async fn seed_model_policy(fixture: &Fixture) -> Result<()> {
    let credential = model_resource(
        fixture,
        ResourceKind::Credential,
        "slot",
        Definition::Credential {
            secret: "retained-test-secret".into(),
        },
    )
    .await?;
    let policy = model_resource(
        fixture,
        ResourceKind::Policy,
        "policy",
        Definition::Policy {
            document: serde_json::from_value(
                serde_json::json!({"spec_version":"policy/1.0.0-beta.1",
            "policies":{"auto":{"tiers":{"safe":{"model":"mock:model-a"}},"default_tier":"safe"}}}),
            )?,
            name: "auto".into(),
            dependencies: Vec::new(),
        },
    )
    .await?;
    let route = model_resource(fixture, ResourceKind::Route, "auto", Definition::Route {
        routing: serde_json::json!({"providers":{"mock":{"api_base":"https://example.com/v1","models":[{"id":"model-a"}]}},
            "routers":{"auto":{"selection":{"kind":"policy","policy":"auto","base_model":"mock:model-a"}}}}),
        selector: "bitrouter/auto".into(), policy: Some(policy.clone()),
        credentials: vec![CredentialBinding { provider:"mock".into(), account:"default".into(), credential }],
    }).await?;
    let mut input = fixture.configuration_input("retained-model-configuration")?;
    input.document.routes = vec![route];
    input.document.policy = Some(policy.clone());
    let stored = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?;
    let StoredResponse::Configuration(revision) = stored else {
        anyhow::bail!("expected retained model configuration");
    };
    let mut subject = subject(fixture)?;
    subject.eval_id = "model-policy-subject".into();
    subject.identity = EvalSubjectIdentity::TenantModelPolicy {
        installation_id: fixture.installation.clone(),
        tenant_id: fixture.tenant.clone(),
        configuration_revision: revision.revision_id,
        policy,
        name: "auto".into(),
        source_digest: None,
        classifier_digest: None,
        execution: None,
    };
    let issued = fixture.issue("retained-model-key").await?;
    let _: PutSubjectReport = json(
        router(fixture)?
            .oneshot(request(
                fixture,
                issued.secret.as_deref().context("model Eval credential")?,
                "POST",
                "subjects",
                serde_json::to_vec(&subject)?,
            )?)
            .await?,
    )
    .await?;
    Ok(())
}

struct Seeded {
    installation: InstallationId,
    tenant: TenantId,
    secret: String,
    resources: Arc<Resources>,
    gateway: EvalSubject,
}

async fn seeded(path: &Path, key_env: Option<&str>) -> Result<Seeded> {
    std::fs::File::create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let database =
        crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rwc", path.display())).await?;
    database
        .migrate()
        .await
        .context("migrating retained Eval seed database")?;
    let db = database.database().clone();
    let mut fixture = Fixture::with_database(db, &mut |_| {})
        .await
        .context("initial retained Eval authority fixture")?;
    fixture
        .service
        .shutdown()
        .await
        .context("stopping unkeyed retained Eval seed Service")?;
    let resources = Arc::new(Resources::fixture_key(&fixture.installation, key_env)?);
    fixture.service = open_keyed(database, resources.clone())
        .await
        .context("opening keyed retained Eval seed Service")?;
    seed_generic(&fixture)
        .await
        .context("seeding generic tenant Eval subjects")?;
    seed_model_policy(&fixture)
        .await
        .context("seeding model-policy tenant Eval subject")?;
    seed_gateway(&fixture)
        .await
        .context("seeding independent gateway Eval subject")?;
    let reader = fixture
        .issue("retained-subject-reader")
        .await
        .context("issuing retained Eval reader")?;
    let seeded = Seeded {
        installation: fixture.installation.clone(),
        tenant: fixture.tenant.clone(),
        secret: reader.secret.context("retained read credential secret")?,
        resources,
        gateway: gateway_subject(&fixture)?,
    };
    fixture
        .service
        .shutdown()
        .await
        .context("stopping fully seeded retained Eval Service")?;
    let db = fixture.db.clone();
    // Both the Service claim and the final pool clone retain the acquisition.
    // Return only facts so stopped-file readers can acquire a fresh owner.
    drop(fixture);
    finish_fixture_image(db)
        .await
        .context("closing retained Eval seed image")?;
    Ok(seeded)
}

async fn finish_fixture_image(db: sea_orm::DatabaseConnection) -> Result<()> {
    // These tests copy and compare the main file directly. Make the stopped
    // image self-contained before handing it to the next reader.
    let backend = db.get_database_backend();
    let checkpoint = db
        .query_one(Statement::from_string(
            backend,
            "PRAGMA wal_checkpoint(TRUNCATE)",
        ))
        .await?
        .context("checkpointing retained Eval fixture")?;
    ensure!(
        checkpoint.try_get_by_index::<i64>(0)? == 0,
        "fixture checkpoint was busy"
    );
    ensure!(checkpoint.try_get_by_index::<i64>(1)? == checkpoint.try_get_by_index::<i64>(2)?);
    db.close().await?;
    Ok(())
}

fn gateway_router(db: sea_orm::DatabaseConnection) -> Router {
    crate::eval::api::router(
        crate::eval::EvalService::new(
            crate::eval::store::EvalStore::new(db.clone()),
            crate::config::EvalConfig::default(),
        ),
        db,
        true,
    )
}

fn gateway_subject(fixture: &Fixture) -> Result<EvalSubject> {
    let mut subject = subject(fixture)?;
    subject.eval_id = "global-gateway-subject".into();
    subject.identity = EvalSubjectIdentity::GatewayGeneric {};
    Ok(subject)
}

async fn gateway_json<T: serde::de::DeserializeOwned>(
    response: axum::response::Response,
) -> Result<T> {
    ensure!(
        response.status() == StatusCode::OK,
        "unexpected gateway Eval status: {}",
        response.status()
    );
    ensure!(
        response
            .headers()
            .get("content-type")
            .is_some_and(|value| value == "application/json")
    );
    let body = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024),
    )
    .await??;
    Ok(serde_json::from_slice(&body)?)
}

async fn seed_gateway(fixture: &Fixture) -> Result<()> {
    // Assembly mounts this exact independent API/store on the tenancy database.
    let _: PutSubjectReport = gateway_json(
        gateway_router(fixture.db.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/evals/subjects")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&gateway_subject(fixture)?)?))?,
            )
            .await?,
    )
    .await?;
    let row = fixture.db.query_one(Statement::from_string(fixture.db.get_database_backend(),
        "SELECT installation_id,tenant_id,creator_principal_id FROM eval_subjects WHERE eval_id='global-gateway-subject'"))
        .await?.context("actual gateway subject ownership")?;
    for column in ["installation_id", "tenant_id", "creator_principal_id"] {
        ensure!(row.try_get::<Option<String>>("", column)?.is_none());
    }
    Ok(())
}

async fn read_gateway(db: sea_orm::DatabaseConnection, expected: &EvalSubject) -> Result<()> {
    let found: EvalSubject = gateway_json(
        gateway_router(db)
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/evals/subjects/{}", expected.eval_id))
                    .body(Body::empty())?,
            )
            .await?,
    )
    .await?;
    ensure!(&found == expected);
    Ok(())
}

async fn seed_generic(fixture: &Fixture) -> Result<()> {
    let issued = fixture.issue("retained-eval-key").await?;
    let secret = issued.secret.as_deref().context("Eval credential")?;
    let router = router(fixture)?;
    for index in 0..35 {
        let mut input = subject(fixture)?;
        input.eval_id = format!("retained-{index:03}");
        let _: PutSubjectReport = json(
            router
                .clone()
                .oneshot(request(
                    fixture,
                    secret,
                    "POST",
                    "subjects",
                    serde_json::to_vec(&input)?,
                )?)
                .await?,
        )
        .await?;
    }
    Ok(())
}

fn options(home: &Path) -> crate::offline::large::Options {
    crate::offline::large::Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.to_owned(),
        min_free_bytes: 1,
    }
}

async fn corrupt(path: &Path, gateway: bool, expression: &str) -> Result<String> {
    let database =
        crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rw", path.display())).await?;
    let db = database.database().clone();
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            if gateway {
                "SELECT eval_id,subject_json FROM eval_subjects WHERE installation_id IS NULL ORDER BY eval_id DESC LIMIT 1"
            } else {
                "SELECT eval_id,subject_json FROM eval_subjects WHERE installation_id IS NOT NULL ORDER BY eval_id DESC LIMIT 1"
            },
        ))
        .await?
        .context("last isolated subject")?;
    let eval_id: String = row.try_get("", "eval_id")?;
    let logical_id =
        serde_json::from_str::<EvalSubject>(&row.try_get::<String>("", "subject_json")?)?.eval_id;
    let result = db
        .execute(Statement::from_sql_and_values(
            db.get_database_backend(),
            format!("UPDATE eval_subjects SET {expression} WHERE eval_id=?"),
            [eval_id.clone().into()],
        ))
        .await;
    ensure!(result?.rows_affected() == 1);
    if expression.starts_with("subject_json") {
        let row = db
            .query_one(Statement::from_sql_and_values(
                db.get_database_backend(),
                "SELECT eval_id,subject_json FROM eval_subjects WHERE eval_id=?",
                [eval_id.into()],
            ))
            .await?
            .context("corrupted subject")?;
        let value: serde_json::Value =
            serde_json::from_str(&row.try_get::<String>("", "subject_json")?)?;
        if value["spec_version"] == bitrouter_ai::eval::SPEC_VERSION {
            // Namespace/profile negatives carry consistent canonical indexes;
            // they must fail the endpoint check rather than an incidental hash.
            let subject: EvalSubject = serde_json::from_value(value)?;
            db.execute(Statement::from_sql_and_values(
                db.get_database_backend(),
                "UPDATE eval_subjects SET content_digest=?,policy_digest=? WHERE eval_id=?",
                [
                    crate::storage::evaluation::canonical_digest(&subject)?.into(),
                    crate::storage::evaluation::identity_digest(&subject.identity)?.into(),
                    row.try_get::<String>("", "eval_id")?.into(),
                ],
            ))
            .await?;
        }
    }
    drop(database);
    finish_fixture_image(db).await?;
    Ok(logical_id)
}

async fn owned_subject(
    service: &Service,
    fixture: &Seeded,
    id: &str,
) -> Result<axum::response::Response> {
    Ok(crate::tenancy::eval::http::router(service.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/control/v1/tenants/{}/evals/subjects/{id}",
                    fixture.tenant
                ))
                .header("authorization", format!("Bearer {}", fixture.secret))
                .header("bitrouter-installation-id", fixture.installation.as_str())
                .body(Body::empty())?,
        )
        .await?)
}

#[tokio::test]
async fn isolated_retained_subjects_fail_read_only_audit_and_selected_reads_after_startup()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), async {
        let home = tempfile::tempdir()?;
        let path = home.path().join("current.sqlite");
        let fixture = seeded(&path, None).await.context("building shared valid retained Eval image before corruption loop")?;
        for (index, (gateway, expression)) in [
            (false, "subject_json=json_remove(subject_json,'$.spec_version')"),
            (false, "subject_json=json_set(subject_json,'$.spec_version','eval/1.0.0-beta.0')"),
            (false, "subject_json=json_set(subject_json,'$.identity',json('{\"profile\":\"gateway_generic\"}'))"),
            (false, "subject_json=json_set(subject_json,'$.identity.tenant_id','other-tenant')"),
            (false, "subject_json=json_set(subject_json,'$.identity.installation_id','other-installation')"),
            (false, "subject_json=json_set(subject_json,'$.evidence_digest','sha256:0000000000000000000000000000000000000000000000000000000000000000')"),
            (false, "policy_digest='wrong-identity-index'"),
            (false, "content_digest='wrong-content-index'"),
            (false, "creator_principal_id=NULL"),
            (true, "subject_json=json_remove(subject_json,'$.spec_version')"),
            (true, "subject_json=json_set(subject_json,'$.spec_version','eval/1.0.0-beta.0')"),
            (true, "subject_json=json_set(subject_json,'$.identity',json('{\"profile\":\"tenant_generic\",\"installation_id\":\"forged-installation\",\"tenant_id\":\"forged-tenant\"}'))"),
            (true, "creator_principal_id='partial-owner'"),
            (true, "tenant_id='partial-tenant'"),
        ].iter().enumerate() {
            let source = home.path().join(format!("invalid-{index}.sqlite"));
            std::fs::copy(&path, &source)?;
            let damaged = corrupt(&source, *gateway, expression).await.with_context(|| format!("injecting retained Eval corruption: {expression}"))?;
            let original = std::fs::read(&source)?;
            let database = crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rw", source.display())).await?;
            let mut db = database.database().clone();
            let audit_writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let writes = audit_writes.clone();
            db.set_metric_callback(move |info| {
                if matches!(info.statement.sql.split_whitespace().next(),Some("INSERT" | "UPDATE" | "DELETE" | "REPLACE")) {
                    writes.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
                }
            });
            let tx = db.begin().await?;
            let audited = crate::eval::store::retained::validate_retained_subjects_in(
                &tx,&fixture.installation,64 * 1024 * 1024,&|| Ok(())
            ).await;
            tx.rollback().await?;
            ensure!(audited.is_err(),"read-only audit accepted malformed Eval: {expression}");
            ensure!(audit_writes.load(std::sync::atomic::Ordering::Relaxed) == 0,"integrity audit attempted to mutate malformed Eval");
            let service = open_keyed(database, fixture.resources.clone()).await.with_context(|| format!("opening Service with retained Eval corruption: {expression}"))?;
            let outcome: Result<()> = async {
                ensure!(service.current().is_ok());
                service.authenticate_live(&fixture.secret).await.map_err(failure)?;
                if *gateway {
                    let healthy: EvalSubject = json(owned_subject(&service,&fixture,"retained-000").await?).await?;
                    ensure!(healthy.eval_id == "retained-000");
                } else {
                    read_gateway(db.clone(),&fixture.gateway).await?;
                }
                let selected = if *gateway {
                    gateway_router(db.clone()).oneshot(Request::builder().uri(format!("/v1/evals/subjects/{damaged}")).body(Body::empty())?).await?
                } else {
                    owned_subject(&service,&fixture,&damaged).await?
                };
                ensure!(!selected.status().is_success(),"selected malformed Eval was accepted: {expression}");
                ensure!(service.current().is_ok(),"selected malformed Eval fenced unrelated authority: {expression}");
                service.authenticate_live(&fixture.secret).await.map_err(failure)?;
                Ok(())
            }.await;
            let stopped = service.shutdown().await.with_context(|| format!("stopping Service after selected retained Eval read: {expression}"));
            drop(service);
            let closed = finish_fixture_image(db).await;
            outcome.and(stopped).and(closed).with_context(|| format!("selected retained Eval: {expression}"))?;
            ensure!(std::fs::read(&source)? == original, "audit or selected read mutated original {expression}");
        }
        let database = crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rw", path.display())).await?;
        let db = database.database().clone();
        let service = open_keyed(database, fixture.resources.clone()).await.context("opening intact mixed-namespace Eval image after corruption cases")?;
        let tx = db.begin().await?;
        let valid = crate::eval::store::retained::validate_retained_subjects_in(&tx,&fixture.installation,64 * 1024 * 1024,&|| Ok(())).await;
        tx.rollback().await?;
        valid.context("auditing intact mixed-namespace Eval image")?;
        read_gateway(db.clone(), &fixture.gateway).await.context("reading intact gateway Eval after corruption cases")?;
        service.shutdown().await.context("stopping intact mixed-namespace Eval Service after corruption cases")?;
        drop(service);
        db.close().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
#[ignore = "requires explicit BITROUTER_TEST_RECOVERY_KEY; real retained Eval restore"]
async fn actual_restore_rejects_isolated_old_subject_and_preserves_current_mixed_namespaces()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(120), async {
        crate::runtime_crypto::DeploymentKey::from_environment(KEY_ENV)?;
        let home = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let source = home.path().join("current.sqlite");
        let fixture = seeded(&source, Some(KEY_ENV)).await?;
        let original = std::fs::read(&source)?;
        let bundle = home.path().join("current-bundle");
        let report =
            crate::offline::snapshot_with_options(&source, &bundle, options(home.path())).await?;
        let report = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
        let digest = report["evidence_digest"]
            .as_str()
            .context("snapshot anchor")?;
        let restored = home.path().join("restored.sqlite");
        crate::offline::restore_with_options(
            &bundle,
            digest,
            &restored,
            fixture.installation.as_str(),
            KEY_ENV,
            32,
            options(home.path()),
        )
        .await?;
        ensure!(std::fs::read(&source)? == original);
        ensure!(std::fs::read(&restored)? == original);
        let database =
            crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rw", restored.display()))
                .await?;
        let db = database.database().clone();
        let service = open_keyed(database, fixture.resources.clone()).await?;
        read_gateway(db.clone(), &fixture.gateway).await?;
        service.shutdown().await?;
        drop(service);
        db.close().await?;
        let bad = home.path().join("old.sqlite");
        std::fs::copy(&source, &bad)?;
        corrupt(
            &bad,
            false,
            "subject_json=json_remove(subject_json,'$.spec_version')",
        )
        .await?;
        let bad_original = std::fs::read(&bad)?;
        let bad_bundle = home.path().join("old-bundle");
        // Snapshot is an opaque evidence utility and remains able to retain old bytes.
        let report =
            crate::offline::snapshot_with_options(&bad, &bad_bundle, options(home.path())).await?;
        let report = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
        let digest = report["evidence_digest"]
            .as_str()
            .context("old snapshot anchor")?;
        let destination = home.path().join("refused.sqlite");
        ensure!(
            crate::offline::restore_with_options(
                &bad_bundle,
                digest,
                &destination,
                fixture.installation.as_str(),
                KEY_ENV,
                32,
                options(home.path())
            )
            .await
            .is_err()
        );
        ensure!(!destination.exists());
        ensure!(std::fs::read(&bad)? == bad_original);
        ensure!(std::fs::read(bad_bundle.join("snapshot.sqlite"))? == bad_original);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
