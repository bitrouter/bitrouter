//! Source accounting comes from an actual truncated gateway stream. Its
//! immutable receipt and outstanding obligation must survive BRO transfer.
use super::*;
use crate as product;
use crate::db::mixed_store;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::os::unix::fs::PermissionsExt;

#[path = "../../../../../tests/mixed_store_migration/capture.rs"]
mod capture;

async fn evidence(db: &sea_orm::DatabaseConnection) -> Result<Vec<Value>> {
    // SQLite JSON wraps the source bytes without parsing or reserializing a
    // receipt. Include every column, including counters and evidence digests.
    let mut all = Vec::new();
    for table in [
        "tenancy_installation",
        "tenancy_active_configurations",
        "tenancy_execution_receipts",
        "tenancy_unknown_obligations",
        "tenancy_usage_evidence",
        "tenancy_dispatch_intents",
        "requests",
    ] {
        let cols = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"),
            ))
            .await?
            .into_iter()
            .map(|r| r.try_get::<String>("", "name"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let values = cols
            .iter()
            .map(|c| format!("quote(\"{c}\")"))
            .collect::<Vec<_>>()
            .join(",");
        let rows = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT json_array({values}) AS content FROM {table} ORDER BY content"),
            ))
            .await?
            .into_iter()
            .map(|r| r.try_get::<String>("", "content"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        all.push(json!([table, rows]));
    }
    Ok(all)
}

#[tokio::test]
async fn ordinary_restart_preserves_unknown_receipt_with_sealed_m0_thread() -> Result<()> {
    mixed_restart(None).await
}

#[tokio::test]
#[ignore = "set BITROUTER_TEST_BRO_BIN to the independently built bro executable"]
async fn mixed_restart_with_live_bro_continuation() -> Result<()> {
    let bin = std::env::var_os("BITROUTER_TEST_BRO_BIN").context("missing BRO executable")?;
    mixed_restart(Some(std::path::Path::new(&bin))).await
}

async fn mixed_restart(bro: Option<&std::path::Path>) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("m0.sqlite");
    let authority = dir.path().join("authority.sqlite");
    let lease = crate::tenancy::ownership::acquire_path(&authority, true)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", authority.display()), &lease).await?;
    let fixture = Fixture::with_database(db, &mut |_| {}).await?;
    let h = Harness::with_initial(
        "chat_completions",
        StatusCode::OK,
        wire::chat_stream(full_usage("chat_completions"), false)?,
        true,
        None,
        false,
        Some((fixture, lease)),
    )
    .await?;
    let client = Harness::client(false)?;
    let response = h.send(&client).send().await?;
    let id = execution_id(&response)?;
    response.text().await?;
    let receipt = h.receipt(&client, &id).await?;
    ensure!(
        receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Unknown
            && receipt.attempts.len() == 1
    );
    h.fixture.service.shutdown().await?;
    h.fixture
        .db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "VACUUM INTO ?",
            [source.to_str().context("database path")?.into()],
        ))
        .await?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
    let pool = sea_orm::sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sea_orm::sqlx::sqlite::SqliteConnectOptions::new().filename(&source))
        .await?;
    let db = sea_orm::DatabaseConnection::from(pool);
    let objects: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../db/migration/baseline/fixtures/sqlite-current-schema.json"
    ))?;
    for kind in ["table", "index"] {
        for obj in objects.iter().filter(|o| o["type"] == kind) {
            let exists = db
                .query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT 1 FROM sqlite_schema WHERE name=?",
                    [obj["name"].as_str().context("catalog object")?.into()],
                ))
                .await?
                .is_some();
            if !exists {
                db.execute_unprepared(obj["sql"].as_str().context("DDL")?)
                    .await?;
            }
        }
    }
    db.execute_unprepared("UPDATE seaql_migrations SET version='m20261118_000001_model_policy_store'; INSERT OR IGNORE INTO bro_runtime_ownership(id,generation) VALUES(1,0)").await?;
    capture::install(dir.path(), &db).await?;
    for request in [
        "2ad653eb-9a51-4999-b59f-3c4b1d65174c",
        "55caad5a-b17a-47cb-93c0-32d22e203796",
    ] {
        capture::settle(&db, request, 0).await?;
    }
    let before = evidence(&db).await?;
    ensure!(
        !before[3][1]
            .as_array()
            .context("unknown inventory")?
            .is_empty()
    );
    let invalid = dir.path().join("missing-obligation.sqlite");
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "VACUUM INTO ?",
        [invalid.to_str().context("invalid fixture path")?.into()],
    ))
    .await?;
    std::fs::set_permissions(&invalid, std::fs::Permissions::from_mode(0o600))?;
    let invalid_db = sea_orm::sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sea_orm::sqlx::sqlite::SqliteConnectOptions::new().filename(&invalid))
        .await?;
    let removed = sea_orm::sqlx::query("DELETE FROM tenancy_unknown_obligations")
        .execute(&invalid_db)
        .await?;
    ensure!(removed.rows_affected() == 1);
    invalid_db.close().await;
    let denied_path = dir.path().join("denied");
    let denied = mixed_store::activate(&invalid, &denied_path).await?;
    ensure!(
        !denied.dispatch_allowed
            && !denied_path.exists()
            && denied
                .blockers
                .iter()
                .any(|b| b.contains("inconsistent execution obligations")),
        "{:?}",
        denied.blockers
    );
    db.close().await?;
    let dest = dir.path().join("pair");
    let result = mixed_store::activate(&source, &dest).await?;
    ensure!(result.dispatch_allowed, "{:?}", result.blockers);
    let stores = result.stores.context("missing pair")?;
    ensure!(
        dest.join("bro/threads/1149fbda-eb06-43b7-8d0e-108e857d9c2d.json")
            .exists()
    );
    if let Some(bin) = bro {
        continue_bro(bin, dir.path(), &dest.join("bro")).await?;
    }
    for _ in 0..2 {
        let owned =
            crate::db::connect_owned_tenancy(&format!("sqlite://{}", stores.bitrouter.display()))
                .await?;
        let (db, lease, claim) = owned.claim()?;
        let restarted = crate::tenancy::Service::start_resources_with_continuations(
            db.clone(),
            Some(lease),
            crate::tenancy::store::load(&db, chrono::Utc::now()).await?,
            Arc::new(crate::tenancy::resources::Resources::with_key(
                h.fixture.installation.clone(),
                [29; 32],
            )?),
            crate::tenancy::Provisioning {
                import: None,
                continuations: &crate::config::ContinuationConfig::default(),
                claim: Some(claim),
                receipt_observer: Default::default(),
                config: &crate::config::tenancy::TenancyConfig::default(),
            },
            None,
        )
        .await?;
        ensure!(evidence(&db).await? == before && h.calls.load(Ordering::SeqCst) == 1);
        restarted.shutdown().await?;
        drop(restarted);

        db.close().await?;
    }
    Ok(())
}

async fn continue_bro(
    bin: &std::path::Path,
    tmp: &std::path::Path,
    home: &std::path::Path,
) -> Result<()> {
    let thread = "1149fbda-eb06-43b7-8d0e-108e857d9c2d";
    let workspace = tmp.join("new-workspace");
    std::fs::create_dir(&workspace)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
    let router = axum::Router::new().route("/v1/chat/completions", axum::routing::post(move |axum::Json(request):axum::Json<Value>| async move {
        send.send(request).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let chunk=json!({"object":"chat.completion.chunk","id":"next","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","content":"continued"},"finish_reason":"stop"}]});
        Ok::<_,StatusCode>(([("content-type","text/event-stream")],format!("data: {chunk}\n\ndata: [DONE]\n\n")))
    }));
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    async fn run(bin: &std::path::Path, home: &std::path::Path, args: &[&str]) -> Result<Value> {
        let out = tokio::process::Command::new(bin)
            .arg("--data-dir")
            .arg(home)
            .args(args)
            .env_remove("BITROUTER_API_KEY")
            .env_remove("BITROUTER_ENDPOINT")
            .output()
            .await?;
        ensure!(
            out.status.success(),
            "BRO command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(serde_json::from_slice(
            out.stdout
                .split(|b| *b == b'\n')
                .rfind(|l| !l.is_empty())
                .context("missing BRO result")?,
        )?)
    }
    run(
        bin,
        home,
        &[
            "--endpoint",
            &endpoint,
            "managed",
            "restore",
            thread,
            "--workspace",
            workspace.to_str().context("workspace")?,
        ],
    )
    .await?;
    let old = run(
        bin,
        home,
        &[
            "thread",
            "start",
            thread,
            "--request-id",
            "feba0903-39c1-4d18-a768-4c9fcaba4c55:start",
            "Read evidence.txt and report its contents.",
        ],
    )
    .await?;
    ensure!(old["status"] == "completed" && received.try_recv().is_err());
    let args = [
        "thread",
        "start",
        thread,
        "--request-id",
        "next",
        "continue",
    ];
    let new = run(bin, home, &args).await?;
    let req = received.try_recv()?;
    let history = req["messages"].as_array().context("missing SDK history")?;
    ensure!(
        new["status"] == "completed"
            && history
                .iter()
                .filter(|m| m["role"] == "tool"
                    && m["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("baseline persisted payload")))
                .count()
                == 1
    );
    ensure!(
        history.last().is_some_and(|m| m["content"] == "continue")
            && !workspace.join("evidence.txt").exists()
    );
    server.abort();
    ensure!(
        run(bin, home, &args).await?["turn_id"] == new["turn_id"] && received.try_recv().is_err()
    );
    Ok(())
}
