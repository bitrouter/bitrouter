//! Source accounting comes from an actual truncated gateway stream. Its
//! immutable receipt and outstanding obligation must survive BRO transfer.
use super::*;
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
        "tenancy_configurations",
        "tenancy_resource_revisions",
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
#[ignore = "set BITROUTER_TEST_BRO and BITROUTER_TEST_GATEWAY_BINARY"]
async fn mixed_restart_with_live_bro_continuation() -> Result<()> {
    let bin = std::env::var_os("BITROUTER_TEST_BRO").context("missing BRO executable")?;
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
    let mut h = Harness::with_initial(
        "chat_completions",
        StatusCode::OK,
        wire::chat_stream(full_usage("chat_completions"), false)?,
        true,
        None,
        false,
        Some((fixture, lease)),
    )
    .await?;
    use bitrouter_ai::tenancy::{authority::ResourceKind, configuration::Activation};
    let mut policy = h.slot.clone();
    policy.resource_kind = ResourceKind::Policy;
    policy.resource_id = "migration-policy".try_into()?;
    let mut route = h.route.clone();
    route.resource_id = "selected".try_into()?;
    let document = crate::model_policy::parse(&json!({
        "spec_version":"policy/1.0.0-beta.1","policies":{"selected":{"tiers":{"economy":{"model":"mock:model-b"}},"default_tier":"economy"}}
    }))?;
    h.fixture.service.resources.provision_bytes(&h.fixture.db, &serde_json::to_vec(&json!({
        "format":4,"installation_id":h.fixture.installation,"resources":[
            {"reference":policy,"definition":{"kind":"policy","name":"selected","document":document}},
            {"reference":route,"definition":{"kind":"route","selector":"bitrouter/selected","policy":policy,
                "routing":{"providers":{"mock":{"api_base":h.provider_base,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"},{"id":"model-b"}]}},
                    "routers":{"selected":{"selection":{"kind":"policy","policy":"selected","base_model":"mock:model-a"}}}},
                "credentials":[{"provider":"mock","account":"default","credential":h.slot}]}}
        ]
    }))?).await?;
    let active =
        super::super::draft_with_policy(&h.fixture, &route, "migration-config", 25, Some(policy))
            .await?;
    h.fixture
        .service
        .execute(
            h.fixture.operator.clone(),
            crate::tenancy::command::Command::Activate(
                h.fixture.tenant.clone(),
                Activation {
                    operation_id: "migration-activate".try_into()?,
                    expected_active_revision: None,
                    revision_id: active.revision_id.clone(),
                    expected_authority_revision: h
                        .fixture
                        .service
                        .current()
                        .map_err(failure)?
                        .revision,
                    installation_id: h.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    h.headers = headers(
        &h.fixture,
        h.credential
            .secret
            .as_deref()
            .context("issued credential")?,
        &active.revision_id,
    )?;
    h.request["model"] = json!("selected");
    let client = Harness::client(false)?;
    let response = h.send(&client).send().await?;
    if !response.status().is_success() {
        anyhow::bail!(
            "source Policy dispatch: {} {}",
            response.status(),
            response.text().await?
        );
    }
    let id = execution_id(&response)?;
    response.text().await?;
    ensure!(h.received.try_recv().context("policy provider request")?["model"] == "model-b");
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
    let thread = capture::install_approved(dir.path(), &db).await?;
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
    let bro_home = dest.join("bro");
    let checkpoint: Value = serde_json::from_slice(&std::fs::read(
        bro_home.join("threads").join(format!("{thread}.json")),
    )?)?;
    let old = &checkpoint["turns"][0];
    ensure!(
        old["sealed_tool_results"]["write-evidence"]
            == json!({"effect":"completed","output":{"type":"json","value":{"path":"evidence.txt","bytes":18}}})
            && old["outcome"]["final_answer"] == "Wrote evidence.txt."
    );
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
    if let Some(bin) = bro {
        restart_products(&mut h, &id, bin, &stores, &thread, old).await?;
    }
    Ok(())
}

async fn restart_products(
    h: &mut Harness,
    old_execution: &ExecutionId,
    bro: &std::path::Path,
    stores: &mixed_store::StagedStores,
    thread: &str,
    old: &Value,
) -> Result<()> {
    use std::{
        fs,
        os::unix::fs::MetadataExt,
        time::{Duration, SystemTime},
    };
    use tokio::process::Command;
    let gateway =
        std::env::var_os("BITROUTER_TEST_GATEWAY_BINARY").context("gateway executable")?;
    let service_home = stores.bitrouter.parent().context("gateway home")?;
    let home = stores.bro.parent().context("BRO home")?;
    let work = tempfile::tempdir()?;
    let effect = work.path().join("evidence.txt");
    let bytes = include_bytes!("../../../../../tests/fixtures/m0-approved-write/evidence.txt");
    fs::write(&effect, bytes)?;
    fs::File::options()
        .write(true)
        .open(&effect)?
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1000))?;
    let before = fs::metadata(&effect)?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = port.local_addr()?;
    let endpoint = format!("http://{address}");
    let control_port = std::net::TcpListener::bind("127.0.0.1:0")?;
    let control_address = control_port.local_addr()?;
    let config = service_home.join("bitrouter.yaml");
    fs::write(
        &config,
        format!(
            "inherit_defaults: false\nregistry: {{enabled: false}}\nserver: {{listen: '{address}', skip_auth: false}}\ntenancy: {{enabled: true, wrapping_key_env: M4_SERVICE_KEY}}\ncontrol:\n  enabled: true\n  listen: '{control_address}'\n  credentials:\n    - id: migration-operator\n      token_env: M4_CONTROL_TOKEN\n      scopes: [control:tenancy]\ndatabase: {{url: 'sqlite://bitrouter.db?mode=rwc'}}\n"
        ),
    )?;
    drop((port, control_port));
    let client = Harness::client(false)?;
    let run = |args: Vec<&str>| {
        let mut cmd = Command::new(bro);
        cmd.kill_on_drop(true)
            .env_clear()
            .env("HOME", work.path())
            .current_dir(work.path())
            .arg("--data-dir")
            .arg(home)
            .args(["--endpoint", &endpoint])
            .args(args);
        async move {
            let out = tokio::time::timeout(Duration::from_secs(15), cmd.output()).await??;
            ensure!(
                out.status.success(),
                "BRO: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Ok::<Value, anyhow::Error>(serde_json::from_slice(
                out.stdout
                    .split(|b| *b == b'\n')
                    .rfind(|s| !s.is_empty())
                    .context("BRO reply")?,
            )?)
        }
    };
    let mut new_execution = None;
    for boot in 0..2 {
        use base64::Engine;
        let mut child = Command::new(&gateway)
            .kill_on_drop(true)
            .env_clear()
            .env("HOME", service_home)
            .env("BITROUTER_HOME", service_home)
            .env(
                "M4_SERVICE_KEY",
                base64::engine::general_purpose::STANDARD.encode([29; 32]),
            )
            .env(
                "M4_CONTROL_TOKEN",
                "migration-fixture-control-token-at-least-32-bytes",
            )
            .current_dir(service_home)
            .args(["serve", "--config"])
            .arg(&config)
            .spawn()?;
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                ensure!(
                    child.try_wait()?.is_none(),
                    "gateway exited before readiness"
                );
                if client
                    .get(format!("{endpoint}/health"))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success())
                {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await??;
        if boot == 0 {
            run(vec![
                "managed",
                "restore",
                thread,
                "--workspace",
                work.path().to_str().context("workspace")?,
            ])
            .await?;
        }
        let historical = run(vec![
            "thread",
            "start",
            thread,
            old["prompt"].as_str().context("prompt")?,
            "--request-id",
            old["request_id"].as_str().context("start key")?,
        ])
        .await?;
        ensure!(
            historical["turn_id"] == old["turn_id"]
                && historical["outcome"]["final_answer"] == "Wrote evidence.txt."
        );
        let receipt = |id: &ExecutionId| {
            client
                .get(format!(
                    "http://{control_address}/control/v1/tenants/{}/executions/{id}",
                    h.fixture.tenant
                ))
                .bearer_auth(h.credential.secret.as_deref().unwrap_or_default())
                .header("bitrouter-installation-id", h.fixture.installation.as_str())
        };
        let retained: ExecutionReceipt = receipt(old_execution)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            retained.state == bitrouter_ai::tenancy::execution::ReceiptState::Unknown
                && retained.attempts.len() == 1
        );
        ensure!(h.calls.load(Ordering::SeqCst) == boot + 1);
        if boot == 0 {
            let sent = client
                .post(format!("{endpoint}{}", h.path))
                .headers(h.headers.clone())
                .json(&h.request)
                .send()
                .await?
                .error_for_status()?;
            let id = execution_id(&sent)?;
            ensure!(&id != old_execution);
            sent.text().await?;
            ensure!(
                h.received
                    .try_recv()
                    .context("policy provider request after migration")?["model"]
                    == "model-b"
            );
            new_execution = Some(id);
        } else {
            let persisted: ExecutionReceipt =
                receipt(new_execution.as_ref().context("new execution")?)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
            ensure!(persisted.attempts.len() == 1);
        }
        let after = fs::metadata(&effect)?;
        ensure!(
            fs::read(&effect)? == bytes
                && after.ino() == before.ino()
                && after.modified()? == before.modified()?
                && h.calls.load(Ordering::SeqCst) == 2
        );
        let stopped = tokio::time::timeout(
            Duration::from_secs(20),
            Command::new(&gateway)
                .kill_on_drop(true)
                .env_clear()
                .env("HOME", service_home)
                .env("BITROUTER_HOME", service_home)
                .env(
                    "M4_CONTROL_TOKEN",
                    "migration-fixture-control-token-at-least-32-bytes",
                )
                .env(
                    "M4_SERVICE_KEY",
                    base64::engine::general_purpose::STANDARD.encode([29; 32]),
                )
                .current_dir(service_home)
                .args(["stop", "--config"])
                .arg(&config)
                .output(),
        )
        .await??;
        ensure!(
            stopped.status.success(),
            "gateway stop: {}",
            String::from_utf8_lossy(&stopped.stderr)
        );
        ensure!(
            tokio::time::timeout(Duration::from_secs(20), child.wait())
                .await??
                .success()
        );
    }
    Ok(())
}
