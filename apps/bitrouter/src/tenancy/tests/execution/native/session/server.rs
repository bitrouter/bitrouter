//! Opt-in real SQL journal companion. The caller supplies an empty disposable DB.
use super::*;
use crate::tenancy::{
    hosts::HostBoundary,
    transaction::{Disposition, Phase},
};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};

async fn connect() -> Result<DatabaseConnection> {
    let source = std::env::var("BITROUTER_TEST_NATIVE_DATABASE_URL")
        .context("set a dedicated empty PG/MySQL database URL")?;
    let target = url::Url::parse(&source)?;
    ensure!(
        !target.path().trim_matches('/').is_empty(),
        "explicit disposable database required"
    );
    let backend = match target.scheme() {
        "postgres" | "postgresql" => {
            ensure!(cfg!(feature = "db-postgres"));
            DatabaseBackend::Postgres
        }
        "mysql" => {
            ensure!(cfg!(feature = "db-mysql"));
            DatabaseBackend::MySql
        }
        _ => anyhow::bail!("native server companion requires PG or MySQL"),
    };
    let mut options = ConnectOptions::new(source);
    options
        .max_connections(1)
        .min_connections(1)
        .connect_timeout(TIMEOUT)
        .acquire_timeout(TIMEOUT)
        .sqlx_logging(false);
    let db = Database::connect(options).await?;
    let preflight = async {
        let (query, settings): (&str, &[&str]) = match backend {
            DatabaseBackend::Postgres => (
                "SELECT COUNT(*) AS total FROM information_schema.tables WHERE table_schema = current_schema()",
                &["SET statement_timeout = '10s'", "SET lock_timeout = '5s'"],
            ),
            DatabaseBackend::MySql => (
                "SELECT COUNT(*) AS total FROM information_schema.tables WHERE table_schema = DATABASE()",
                &["SET SESSION innodb_lock_wait_timeout = 5", "SET SESSION lock_wait_timeout = 5", "SET SESSION max_execution_time = 10000"],
            ),
            _ => anyhow::bail!("unsupported native companion backend"),
        };
        let row = db.query_one(Statement::from_string(backend, query)).await?.context("database preflight")?;
        ensure!(row.try_get::<i64>("", "total")? == 0, "refuse a populated database; provide a new disposable DB");
        for setting in settings { db.execute_unprepared(setting).await?; }
        Ok::<_, anyhow::Error>(())
    }.await;
    if let Err(error) = preflight {
        let closed = tokio::time::timeout(TIMEOUT, db.close()).await;
        ensure!(
            matches!(closed, Ok(Ok(()))),
            "preflight rejected and pool closure unconfirmed"
        );
        return Err(error);
    }
    Ok(db)
}

async fn fault(db: &DatabaseConnection, install: bool) -> Result<()> {
    let statements: &[&str] = match (db.get_database_backend(), install) {
        (DatabaseBackend::Postgres, true) => &[
            "CREATE FUNCTION native36_reject() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'native journal DML failure'; END $$",
            "CREATE TRIGGER native36_dml_fault BEFORE INSERT ON bro_managed_sessions FOR EACH ROW EXECUTE FUNCTION native36_reject()",
        ],
        (DatabaseBackend::Postgres, false) => &[
            "DROP TRIGGER native36_dml_fault ON bro_managed_sessions",
            "DROP FUNCTION native36_reject()",
        ],
        (DatabaseBackend::MySql, true) => &[
            "CREATE TRIGGER native36_dml_fault BEFORE INSERT ON bro_managed_sessions FOR EACH ROW SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'native journal DML failure'",
        ],
        (DatabaseBackend::MySql, false) => &["DROP TRIGGER native36_dml_fault"],
        _ => anyhow::bail!("unsupported native companion backend"),
    };
    for sql in statements {
        db.execute_unprepared(sql).await?;
    }
    Ok(())
}

async fn campaign(
    db: &DatabaseConnection,
    services: &mut Vec<Service>,
    gates: &mut Vec<Arc<crate::tenancy::hosts::HostGate>>,
    provider: &mut Provider,
    workspace: &std::path::Path,
) -> Result<()> {
    let mut retain = |service: &Service| services.push(service.clone());
    let fixture = Fixture::with_database(db.clone(), OperationLimits::default(), &mut retain)
        .await
        .context("native backend: provision authority fixture")?;
    let (mut fixture, route, _) =
        crate::tenancy::tests::execution::provision_protocol_with_fixture(
            fixture,
            &provider.url,
            "chat_completions",
            &mut retain,
        )
        .await
        .context("native backend: provision encrypted route")?;
    let (headers, key) = ready(&fixture, &route)
        .await
        .context("native backend: bind execution revision")?;
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    let mut session = NativeSession::open_authenticated(
        app.clone(),
        headers.clone(),
        "backend",
        workspace,
        ToolMode::ReadOnly,
        &HarnessConfig::default(),
    )
    .await
    .context("native backend: initial authenticated open")?;
    let id = session.core().snapshot().await.session_id;
    session
        .run(input(), Arc::new(Approval), CancellationToken::new())
        .await
        .context("native backend: initial run")?;
    provider
        .receive()
        .await
        .context("native backend: initial provider request")?;
    session
        .close()
        .await
        .context("native backend: initial close")?;
    let rotation = fixture
        .issue("native-backend-rotation")
        .await
        .context("native backend: issue rotated credential")?;
    let revision = headers
        .get("bitrouter-execution-revision")
        .context("revision")?
        .to_str()?
        .try_into()?;
    let rotated = super::super::super::headers(
        &fixture,
        rotation.secret.as_deref().context("rotated key")?,
        &revision,
    )?;
    let mut session = NativeSession::open_authenticated(
        app.clone(),
        rotated,
        "backend",
        workspace,
        ToolMode::ReadOnly,
        &HarnessConfig::default(),
    )
    .await
    .context("native backend: restore with rotated credential")?;
    ensure!(session.core().snapshot().await.session_id == id);
    session
        .run(input(), Arc::new(Approval), CancellationToken::new())
        .await
        .context("native backend: restored run")?;
    provider
        .receive()
        .await
        .context("native backend: restored provider request")?;
    session
        .close()
        .await
        .context("native backend: restored close")?;

    fault(db, true)
        .await
        .context("native backend: install DML fault")?;
    let gate = fixture.service.hold_host(HostBoundary::Resources)?;
    gates.push(gate.clone());
    let open_app = app.clone();
    let open_headers = headers.clone();
    let open_workspace = workspace.to_owned();
    let opener = AbortOnDropHandle::new(tokio::spawn(async move {
        NativeSession::open_authenticated(
            open_app,
            open_headers,
            "rollback",
            &open_workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    let rollback = fixture.service.cleanup.hold(Phase::RollingBack);
    gate.release();
    tokio::time::timeout(TIMEOUT, rollback.entered.notified()).await?;
    ensure!(fixture.service.host_slots_for_test() == 0 && !opener.is_finished());
    fixture.service.cleanup.release_gate();
    ensure!(tokio::time::timeout(TIMEOUT, opener).await??.is_err());
    tokio::time::timeout(TIMEOUT, async {
        while fixture.service.host_slots_for_test() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    fault(db, false)
        .await
        .context("native backend: remove confirmed rollback fault")?;
    // Confirmed rollback releases custody, but the existing writer deliberately
    // fences storage faults. Reopen through the actual bootstrap before the
    // independent collision case; do not reset live or manufacture a proof.
    ensure!(fixture.service.current().is_err());
    fixture.service.shutdown().await?;
    fixture.service = Service::start_prepared(
        db.clone(),
        None,
        OperationLimits::default(),
        store::load(db, Utc::now()).await?,
        Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?),
    )
    .await
    .context("native backend: bootstrap after confirmed rollback")?;
    retain(&fixture.service);
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers =
        super::super::super::headers(&fixture, key.secret.as_deref().context("key")?, &revision)?;
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );

    let backend = db.get_database_backend();
    let row = db.query_one(Statement::from_string(backend,
        "SELECT id, installation_id, tenant_id, session_principal_id, logical_name, execution_revision, revision, payload FROM bro_managed_sessions",
    )).await?.context("owned row")?;
    ensure!(row.try_get::<String>("", "installation_id")? == fixture.installation.as_str());
    ensure!(row.try_get::<String>("", "tenant_id")? == fixture.tenant.as_str());
    ensure!(row.try_get::<String>("", "session_principal_id")? == fixture.principal.as_str());
    ensure!(row.try_get::<String>("", "logical_name")? == "backend");
    ensure!(row.try_get::<String>("", "execution_revision")? == revision.as_str());
    let before = row.try_get::<i64>("", "revision")?;
    let journal: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
    ensure!(
        journal["journal"]["released"] == true && journal["journal"]["grant"]["session_id"] == id
    );
    // A colliding physical row outside scope must be excluded before payload
    // length/JSON validation. Reaching the real resource barrier proves Load(None).
    db.execute_unprepared("UPDATE bro_managed_sessions SET tenant_id = 'other-tenant', payload = 'not-json', payload_bytes = 999").await?;
    let gate = fixture.service.hold_host(HostBoundary::Resources)?;
    gates.push(gate.clone());
    let open_workspace = workspace.to_owned();
    let opener = AbortOnDropHandle::new(tokio::spawn(async move {
        NativeSession::open_authenticated(
            app,
            headers,
            "backend",
            &open_workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    gate.release();
    ensure!(tokio::time::timeout(TIMEOUT, opener).await??.is_err());
    fixture.service.shutdown().await?;
    let row = db
        .query_one(Statement::from_string(
            backend,
            "SELECT revision, tenant_id, payload, payload_bytes FROM bro_managed_sessions",
        ))
        .await?
        .context("foreign row")?;
    ensure!(row.try_get::<i64>("", "revision")? == before);
    ensure!(row.try_get::<String>("", "tenant_id")? == "other-tenant");
    ensure!(row.try_get::<String>("", "payload")? == "not-json");
    ensure!(row.try_get::<i64>("", "payload_bytes")? == 999);
    ensure!(provider.calls() == 2 && fixture.service.host_slots_for_test() == 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires empty disposable BITROUTER_TEST_NATIVE_DATABASE_URL (PG/MySQL), migrated through 036"]
async fn server_native36_persistent_consumer_rotation_rollback_and_foreign_row() -> Result<()> {
    let db = connect().await?;
    let workspace = tempfile::tempdir()?;
    let mut services = Vec::new();
    let mut gates = Vec::new();
    let mut provider = Provider::start(false).await?;
    // Retain DB, actors and provider outside timeout/unwind. No physical DROP,
    // force-close, automatic retry or cleanup acknowledgement on uncertainty.
    let outcome = tokio::time::timeout(
        Duration::from_secs(90),
        std::panic::AssertUnwindSafe(campaign(
            &db,
            &mut services,
            &mut gates,
            &mut provider,
            workspace.path(),
        ))
        .catch_unwind(),
    )
    .await
    .context("native backend campaign deadline")
    .and_then(|value| value.map_err(|_| anyhow::anyhow!("native backend campaign unwound")))
    .and_then(|value| value);
    let mut cleanup = Vec::new();
    for gate in gates {
        gate.release();
    }
    for service in &services {
        service.cleanup.release_gate();
        if let Err(error) = tokio::time::timeout(Duration::from_secs(35), service.shutdown())
            .await
            .context("native backend owner did not drain")
            .and_then(|result| result)
        {
            cleanup.push(error.to_string());
        }
    }
    if let Err(error) = provider.finish().await {
        cleanup.push(error.to_string());
    }
    if cleanup.is_empty()
        && let Err(error) = tokio::time::timeout(TIMEOUT, db.close())
            .await
            .context("native backend pool did not close")
            .and_then(|result| result.map_err(anyhow::Error::from))
    {
        cleanup.push(error.to_string());
    }
    ensure!(
        cleanup.is_empty(),
        "native backend cleanup unconfirmed; database retained: {}; test: {outcome:?}",
        cleanup.join("; ")
    );
    outcome
}
