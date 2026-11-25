//! Identical owned-session startup and recovery cases for SQLite and native PG.
use super::*;
use crate::tenancy::receipts::ordered::intent::{self, Intent};
use bitrouter_ai::tenancy::{authority::ResourceKind, configuration::ResourceRef};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Case {
    History,
    Recovery,
    Isolation,
}

async fn prepare(url: &str, rows: usize) -> Result<(Fixture, crate::tenancy::ownership::Lease)> {
    let owned = crate::db::connect_owned_tenancy(url).await?;
    owned.migrate().await?;
    let owner = owned.owner();
    let db = owned.database().clone();
    let service =
        Service::open_owned_profile(owned, &Default::default(), &Default::default()).await?;
    let mut fixture = Fixture::with_service(db, service, &mut |_| {}).await?;
    fixture.service.shutdown().await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        Some(owner.clone()),
        store::initial_header(&fixture.db, Utc::now()).await?,
        Arc::new(Resources::with_key(
            fixture.installation.clone(),
            fixture.wrapping_key,
        )?),
    )
    .await?;
    // Reuse the canonical source writers; no test-only inventory is substituted.
    history::seed_prepared(&mut fixture, rows).await?;
    Ok((fixture, owner))
}

pub(super) async fn dispose(fixture: Fixture) -> Result<()> {
    let stopped = fixture.service.shutdown().await;
    let db = fixture.db.clone();
    drop(fixture);
    let closed = db.close().await;
    stopped.and(closed.map_err(Into::into))
}

fn observed_ready(service: &Service, queries: &Queries) -> Result<Vec<String>> {
    ensure!(service.current().is_ok() && service.admission.receipts_ready());
    let statements = queries.require_bounded()?;
    ensure!(
        statements
            .iter()
            .any(|s| s.contains("from tenancy_receipt_capacity")),
        "owned startup did not read durable capacity"
    );
    // PostgreSQL's per-statement timeout values vary with remaining deadlines;
    // compare actual reads, retaining the full observation for boundary checks.
    Ok(statements
        .into_iter()
        .filter(|s| s.starts_with("select ") || s.starts_with("with "))
        .collect())
}

async fn history_case(url: &str, rows: usize, baseline: &mut Option<Vec<String>>) -> Result<()> {
    let (fixture, owner) = prepare(url, rows).await?;
    let key = fixture.wrapping_key;
    let installation = fixture.installation.clone();
    let initial = agreement(&fixture.db, &installation).await?;
    ensure!(initial[6] == u64::try_from(rows)?);
    dispose(fixture).await?;
    drop(owner);
    let queries = Arc::new(Queries::default());
    close(open_url(url, &queries, key).await?).await?;
    for _ in 0..2 {
        let service = open_url(url, &queries, key).await?;
        let checked: Result<()> = async {
            let reads = observed_ready(&service, &queries)?;
            queries.require_no_history_writes()?;
            if service.db.get_database_backend() == sea_orm::DatabaseBackend::Sqlite {
                queries.require_indexed_retained_reads(&service.db).await?;
            }
            if let Some(expected) = baseline {
                ensure!(
                    *expected == reads,
                    "mature startup reads changed with completed history: {rows}"
                );
            } else {
                *baseline = Some(reads);
            }
            ensure!(
                agreement(&service.db, &installation).await? == initial,
                "mature restart changed canonical sources or capacity"
            );
            Ok(())
        }
        .await;
        let stopped = close(service).await;
        checked.and(stopped)?;
    }
    Ok(())
}

async fn recovery_case(url: &str, rows: usize) -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider_url = format!("http://{}/v1", listener.local_addr()?);
    let router = axum::Router::new().route("/v1/chat/completions", axum::routing::post(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async { axum::Json(serde_json::json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}})) }
    }));
    let _provider = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, router).await
    }));
    let (fixture, owner) = prepare(url, rows).await?;
    let key = fixture.wrapping_key;
    let route = ResourceRef {
        resource_kind: ResourceKind::Route,
        resource_id: "owned-recovery-route".try_into()?,
        ..history::reference(&fixture, "retained-slot-00000")?
    };
    let slot = history::reference(&fixture, "retained-slot-00000")?;
    fixture.service.resources.provision_bytes(&fixture.db, &serde_json::to_vec(&serde_json::json!({
        "format":1,"installation_id":fixture.installation,"resources":[{
            "reference":route,"definition":{"kind":"route","selector":"mock:model","policy":null,
                "routing":{"providers":{"mock":{"api_base":provider_url,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model"}]}}},
                "credentials":[{"provider":"mock","account":"default","credential":slot}]}
        }]
    }))?).await?;
    let identities = [
        recovery::identity(&fixture, route.clone(), "owned-unfinished-a")?,
        recovery::identity(&fixture, route, "owned-unfinished-b")?,
    ];
    let installation = fixture.installation.clone();
    let tenant = fixture.tenant.clone();
    dispose(fixture).await?;
    drop(owner);
    let queries = Arc::new(Queries::default());
    let previous = open_url(url, &queries, key).await?;
    previous.shutdown().await?;
    let origin = previous
        .startup
        .as_ref()
        .context("real owned startup origin")?
        .bootstrap_stamp(&previous.cleanup)?;
    let tx = previous.db.begin().await?;
    let mut intents = Vec::new();
    for identity in &identities {
        let intent = Intent::new(identity, origin.clone())?;
        intent::insert(&tx, &intent).await?;
        intents.push(intent);
    }
    tx.commit().await?;
    close(previous).await?;

    let mut receipts_before = None;
    let mut capacity_before = None;
    for restart in 0..2 {
        let service = open_url(url, &queries, key).await?;
        let checked: Result<()> = async {
            observed_ready(&service, &queries)?;
            if service.db.get_database_backend() == sea_orm::DatabaseBackend::Sqlite {
                queries.require_indexed_retained_reads(&service.db).await?;
            }
            let mut actual = Vec::new();
            for identity in &identities {
                actual.push(recovery::recovered(&service, &tenant, &identity.execution).await?);
            }
            if let Some(expected) = &receipts_before {
                ensure!(
                    *expected == actual,
                    "second recovery changed terminal receipts/evidence"
                );
            }
            receipts_before = Some(actual);
            let pending = service
                .db
                .query_one(sql(
                    &service.db,
                    "SELECT COUNT(*) AS n FROM tenancy_dispatch_intents",
                ))
                .await?
                .context("pending intents")?;
            ensure!(pending.try_get::<i64>("", "n")? == 0);
            let capacity = agreement(&service.db, &installation).await?;
            ensure!(capacity[5] == 2 && capacity[6] == u64::try_from(rows)? + 2);
            if let Some(expected) = capacity_before {
                ensure!(
                    expected == capacity,
                    "second recovery duplicated financial quantities"
                );
            }
            capacity_before = Some(capacity);
            if restart == 1 {
                queries.require_no_history_writes()?;
            }
            for intent in &intents {
                let tx = service.db.begin().await?;
                let denied = intent::insert(&tx, intent).await;
                tx.rollback().await?;
                ensure!(
                    denied.is_err(),
                    "retired intent regained dispatch authority"
                );
            }
            ensure!(
                calls.load(Ordering::SeqCst) == 0,
                "owned startup replayed a provider call"
            );
            Ok(())
        }
        .await;
        let stopped = close(service).await;
        checked.and(stopped)?;
    }
    Ok(())
}

async fn isolation_case(url: &str, rows: usize) -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider_url = format!("http://{}/v1", listener.local_addr()?);
    let router = axum::Router::new().route("/v1/chat/completions", axum::routing::post(
        move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
            counted.fetch_add(1, Ordering::SeqCst);
            async move {
                if !headers.get("authorization").is_some_and(|value| value == "Bearer healthy-owned-secret")
                    || body["model"] != "model" || body["messages"][0]["content"] != "healthy-owned-request" {
                    return (axum::http::StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"error":"wrong tenant request"})));
                }
                (axum::http::StatusCode::OK, axum::Json(serde_json::json!({
                    "id":"chatcmpl-owned-isolation","object":"chat.completion","created":0,"model":"model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":"healthy owned reply"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}
                })))
            }
        },
    ));
    let _provider = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, router).await
    }));
    let (mut fixture, owner) = prepare(url, rows).await?;
    let healthy_tenant: TenantId = "healthy-tenant".try_into()?;
    let slot = ResourceRef {
        owner_id: healthy_tenant.as_str().try_into()?,
        ..history::reference(&fixture, "healthy-owned-key")?
    };
    let route = ResourceRef {
        resource_kind: ResourceKind::Route,
        resource_id: "owned-healthy-model".try_into()?,
        ..slot.clone()
    };
    fixture.service.resources.provision_bytes(&fixture.db, &serde_json::to_vec(&serde_json::json!({
        "format":1,"installation_id":fixture.installation,"resources":[
            {"reference":slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"healthy-owned-secret"}}}},
            {"reference":route,"definition":{"kind":"route","selector":"mock:model","policy":null,
                "routing":{"providers":{"mock":{"api_base":provider_url,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model"}]}}},
                "credentials":[{"provider":"mock","account":"default","credential":slot}]}}
        ]
    }))?).await?;
    // Seed ends with the writer stopped; retain the same session while creating
    // normal authority/configuration records before injecting immutable damage.
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        Some(owner.clone()),
        store::initial_header(&fixture.db, Utc::now()).await?,
        Arc::new(Resources::with_key(
            fixture.installation.clone(),
            fixture.wrapping_key,
        )?),
    )
    .await?;
    let healthy = corruption::healthy_credential(&fixture).await?;
    let broken = fixture.issue("owned-broken-reader").await?;
    let mut revisions = Vec::new();
    for (tenant, operation) in [
        (&fixture.tenant, "owned-broken-config"),
        (&healthy_tenant, "owned-healthy-config"),
    ] {
        let mut input = fixture.configuration_input(operation)?;
        if tenant == &healthy_tenant {
            input.document.routes.push(route.clone());
        }
        let saved = fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Configuration(tenant.clone(), input),
            )
            .await
            .map_err(failure)?;
        let StoredResponse::Configuration(saved) = saved else {
            anyhow::bail!("configuration fixture")
        };
        revisions.push(saved.revision_id);
    }
    fixture.service.shutdown().await?;
    let changed = sea_orm::sea_query::Query::update()
        .table(sea_orm::sea_query::Alias::new("tenancy_configurations"))
        .value(sea_orm::sea_query::Alias::new("compiled_json"), "{}")
        .and_where(
            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("tenant_id"))
                .eq(fixture.tenant.as_str()),
        )
        .and_where(
            sea_orm::sea_query::Expr::col(sea_orm::sea_query::Alias::new("revision_id"))
                .eq(revisions[0].as_str()),
        )
        .to_owned();
    ensure!(
        fixture
            .db
            .execute(fixture.db.get_database_backend().build(&changed))
            .await?
            .rows_affected()
            == 1
    );
    let installation = fixture.installation.clone();
    let tenant = fixture.tenant.clone();
    let key = fixture.wrapping_key;
    dispose(fixture).await?;
    drop(owner);
    let queries = Arc::new(Queries::default());
    let service = open_url(url, &queries, key).await?;
    let checked: Result<()> = async {
        observed_ready(&service, &queries)?;
        queries.require_no_history_writes()?;
        ensure!(calls.load(Ordering::SeqCst) == 0);
        let held = service
            .authenticate_with_reuse(broken.secret.as_deref().context("broken token")?, false)
            .await
            .map_err(failure)?;
        let epoch = service.authority.integrity_epoch();
        let denied = service
            .configuration(&held.caller, &tenant, &revisions[0])
            .await;
        ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));
        ensure!(service.authority.integrity_epoch() > epoch);
        ensure!(
            service
                .authority
                .read()
                .map_err(|code| failure(code.into()))?
                .check(&held.lease, |_| Ok(()))
                .is_err()
        );
        let caller = service
            .authenticate_with_reuse(healthy.secret.as_deref().context("healthy token")?, false)
            .await
            .map_err(failure)?;
        let selected = service
            .configuration(&caller.caller, &healthy_tenant, &revisions[1])
            .await
            .map_err(failure)?;
        ensure!(selected.tenant_id == healthy_tenant && selected.revision_id == revisions[1]);
        service.dispatch_test_ready.store(true, Ordering::Release);
        let state = service.current().map_err(failure)?;
        let binding = service.capability_document(&state)
            .map_err(|code| failure(code.into()))?
            .bind(revisions[1].clone(), "healthy-owned-correlation".try_into()?)
            .context("binding healthy neighboring strict inference")?;
        let object = serde_json::to_value(binding)?;
        let mut request = axum::http::Request::post("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", healthy.secret.as_deref().context("healthy token")?))
            .body(axum::body::Body::from(serde_json::to_vec(&serde_json::json!({
                "model":"owned-healthy-model","messages":[{"role":"user","content":"healthy-owned-request"}]
            }))?))?;
        for (name, value) in object.as_object().context("dispatch binding")? {
            request.headers_mut().insert(axum::http::HeaderName::from_bytes(name.as_bytes())?,
                value.as_str().context("binding text")?.parse()?);
        }
        let gateway = axum::Router::new().fallback(|| async { axum::http::StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(service.clone(), crate::tenancy::http::dispatch));
        let response = tokio::time::timeout(std::time::Duration::from_secs(10),
            tower::ServiceExt::oneshot(gateway, request)).await??;
        ensure!(response.status() == axum::http::StatusCode::OK, "healthy neighboring inference failed: {}", response.status());
        let response = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
        ensure!(std::str::from_utf8(&response)?.contains("healthy owned reply"));
        tokio::time::timeout(std::time::Duration::from_secs(10), service.admission.wait_drained()).await?;
        ensure!(calls.load(Ordering::SeqCst) == 1, "corrupt tenant affected healthy provider dispatch");
        ensure!(service.current().is_ok() && service.admission.receipts_ready());
        let quantities = agreement(&service.db, &installation).await?;
        ensure!(quantities[6] == u64::try_from(rows)? + 1 && quantities[5] == 0);
        Ok(())
    }
    .await;
    let stopped = close(service).await;
    checked.and(stopped)
}

async fn run(url: &str, rows: usize, case: Case, baseline: &mut Option<Vec<String>>) -> Result<()> {
    match case {
        Case::History => history_case(url, rows, baseline).await,
        Case::Recovery => recovery_case(url, rows).await,
        Case::Isolation => isolation_case(url, rows).await,
    }
}

async fn sqlite(case: Case) -> Result<()> {
    let mut baseline = None;
    for rows in [3, 257] {
        let home = tempfile::tempdir()?;
        run(
            &format!("sqlite://{}", home.path().join("owned.sqlite").display()),
            rows,
            case,
            &mut baseline,
        )
        .await?;
    }
    Ok(())
}

#[cfg(feature = "db-postgres")]
async fn postgres(case: Case) -> Result<()> {
    let mut baseline = None;
    for rows in [3, 257] {
        let database = crate::db::migration::server_fixture::OwnedServer::new(
            sea_orm::DatabaseBackend::Postgres,
            "BITROUTER_TEST_MIGRATION_DATABASE_URL",
        )
        .await?;
        let checked = run(&database.url, rows, case, &mut baseline).await;
        database.finish(checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_owned_mature_startup_reads_only_current_state() -> Result<()> {
    sqlite(Case::History).await
}
#[tokio::test]
async fn sqlite_owned_unfinished_recovery_twice_never_replays_provider() -> Result<()> {
    sqlite(Case::Recovery).await
}
#[tokio::test]
async fn sqlite_owned_retained_corruption_leaves_other_tenant_live() -> Result<()> {
    sqlite(Case::Isolation).await
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL and dedicated CREATEDB"]
async fn postgres_owned_mature_startup_reads_only_current_state() -> Result<()> {
    postgres(Case::History).await
}
#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL and dedicated CREATEDB"]
async fn postgres_owned_unfinished_recovery_twice_never_replays_provider() -> Result<()> {
    postgres(Case::Recovery).await
}
#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL and dedicated CREATEDB"]
async fn postgres_owned_retained_corruption_leaves_other_tenant_live() -> Result<()> {
    postgres(Case::Isolation).await
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL and dedicated CREATEDB"]
async fn postgres_owned_scheduler_and_receipt_key_pages_never_sort_retained_unknowns() -> Result<()>
{
    let database = crate::db::migration::server_fixture::OwnedServer::new(
        sea_orm::DatabaseBackend::Postgres,
        "BITROUTER_TEST_MIGRATION_DATABASE_URL",
    )
    .await?;
    let checked = super::scheduler::exercise(&database.url).await;
    database.finish(checked).await
}
