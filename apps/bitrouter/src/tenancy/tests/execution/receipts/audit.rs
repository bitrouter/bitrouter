//! Actual provider receipt -> retired snapshot -> protected archive SDK consumer.
use super::*;
mod schema;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use std::{collections::BTreeMap, path::Path};
use tokio_util::sync::CancellationToken;
const AUDITOR: &str = "archive-auditor-token-at-least-thirty-two-bytes";

// File-first provisioning keeps the provider and receipt path identical to the
// ordinary harness, while the final Service and its pool retain the real lease.
async fn file_harness(source: &Path) -> Result<Harness> {
    let owner = crate::tenancy::ownership::acquire_path(source, true)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    let fixture = Fixture::with_database(db, Default::default(), &mut |_| {}).await?;
    let harness = Harness::with_initial(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        None,
        false,
        Some((fixture, owner)),
    )
    .await?;
    crate::tenancy::audit::tests::assert_file_database(&harness.fixture.db, source).await?;
    ensure!(crate::tenancy::ownership::acquire_path(source, false).is_err());
    let persisted =
        crate::tenancy::store::load_existing(&harness.fixture.db, chrono::Utc::now()).await?;
    ensure!(persisted.installation == harness.fixture.installation);
    Ok(harness)
}

fn files(path: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        out.insert(
            entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("filename"))?,
            std::fs::read(entry.path())?,
        );
    }
    Ok(out)
}
pub(in crate::tenancy::tests::execution) async fn roundtrip(
    db: &DatabaseConnection,
    live_source: &Path,
    expected: &ExecutionReceipt,
    old_token: &str,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    crate::tenancy::audit::tests::assert_file_database(db, live_source).await?;
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("retired.sqlite");
    std::fs::File::create(&source)?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
    // The actual Service has drained. Its leased, file-backed pool remains alive
    // through this physical copy; no memory database or synthetic receipt is used.
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "VACUUM INTO ?",
        [source.to_str().context("path")?.into()],
    ))
    .await?;
    ensure!(
        std::fs::metadata(&source)?.len() > 0,
        "archive copy was not materialized"
    );
    let original = std::fs::read(&source)?;
    let bundle = directory.path().join("bundle");
    let report = crate::offline::snapshot(&source, &bundle).await?;
    let report = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
    ensure!(
        report["evidence"]["complete"] == false && report["evidence"]["cutover_ready"] == false
    );
    let digest = report["evidence_digest"].as_str().context("anchor")?;
    let original_bundle = files(&bundle)?;
    let mut protocols = vec![false];
    if cfg!(feature = "http2") {
        protocols.push(true);
    }
    for h2 in protocols {
        let stop = CancellationToken::new();
        let _stop_on_drop = stop.clone().drop_guard();
        let projection = directory.path().join(format!("projection-{h2}"));
        let audit = crate::offline::audit::open(&bundle, &projection, digest, stop.clone()).await?;
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: "archive-reader".into(),
                    token_env: "ARCHIVE_TOKEN".into(),
                    scopes: vec![bitrouter_ai::control::ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some(AUDITOR.into()),
        )?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = {
            let audit = audit.clone();
            tokio::spawn(async move { audit.serve(listener, auth).await })
        };
        let checked = async {
            let options = bitrouter_sdk::tenancy::TenancyOptions {
                system_proxy: false,
                http_version: bitrouter_sdk::gateway::HttpVersion::Http1Only,
                ..Default::default()
            };
            #[cfg(feature = "http2")]
            let options = if h2 {
                bitrouter_sdk::tenancy::TenancyOptions {
                    http_version: bitrouter_sdk::gateway::HttpVersion::Http2PriorKnowledge,
                    ..options
                }
            } else {
                options
            };
            let sdk =
                bitrouter_sdk::tenancy::TenancyClient::with_options(&endpoint, AUDITOR, options)?;
            let actual = sdk
                .execution(
                    &expected.installation_id,
                    &expected.tenant_id,
                    &expected.execution_id,
                )
                .await?;
            ensure!(serde_json::to_value(actual)? == serde_json::to_value(expected)?);
            let page = sdk
                .executions(
                    &expected.installation_id,
                    &expected.tenant_id,
                    &expected.correlation_id,
                    None,
                    100,
                )
                .await?;
            ensure!(page.execution_ids.contains(&expected.execution_id));
            let client = Harness::client(h2)?;
            let uri = format!(
                "{endpoint}/control/v1/tenants/{}/executions/{}",
                expected.tenant_id, expected.execution_id
            );
            let response = client
                .get(&uri)
                .bearer_auth(AUDITOR)
                .header(
                    "bitrouter-installation-id",
                    expected.installation_id.as_str(),
                )
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::OK
                    && response
                        .headers()
                        .get("cache-control")
                        .is_some_and(|value| value == "no-store")
            );
            ensure!(
                response.version()
                    == if h2 {
                        reqwest::Version::HTTP_2
                    } else {
                        reqwest::Version::HTTP_11
                    }
            );
            let body = response.bytes().await?;
            ensure!(!String::from_utf8_lossy(&body).contains("owned-upstream-secret"));
            for token in ["wrong-token", old_token] {
                let response = client
                    .get(&uri)
                    .bearer_auth(token)
                    .header(
                        "bitrouter-installation-id",
                        expected.installation_id.as_str(),
                    )
                    .send()
                    .await?;
                ensure!(
                    response.status() == StatusCode::UNAUTHORIZED
                        && response
                            .headers()
                            .get("cache-control")
                            .is_some_and(|v| v == "no-store")
                );
            }
            ensure!(
                sdk.execution(
                    &"wrong-installation".try_into()?,
                    &expected.tenant_id,
                    &expected.execution_id
                )
                .await
                .is_err()
            );
            ensure!(
                sdk.execution(
                    &expected.installation_id,
                    &"foreign-tenant".try_into()?,
                    &expected.execution_id
                )
                .await
                .is_err()
            );
            for request in [
                client.get(&uri).bearer_auth(AUDITOR),
                client
                    .get(&uri)
                    .bearer_auth(AUDITOR)
                    .header(
                        "bitrouter-installation-id",
                        expected.installation_id.as_str(),
                    )
                    .header(
                        "bitrouter-installation-id",
                        expected.installation_id.as_str(),
                    ),
                client
                    .get(&uri)
                    .bearer_auth(AUDITOR)
                    .header(
                        "bitrouter-installation-id",
                        expected.installation_id.as_str(),
                    )
                    .header("origin", "https://foreign.example"),
            ] {
                ensure!(!request.send().await?.status().is_success());
            }
            for query in [
                format!("correlation_id={}&limit=0", expected.correlation_id),
                format!("correlation_id={}&limit=101", expected.correlation_id),
                format!("correlation_id={}&unknown=1", expected.correlation_id),
            ] {
                let response = client
                    .get(format!(
                        "{endpoint}/control/v1/tenants/{}/executions?{query}",
                        expected.tenant_id
                    ))
                    .bearer_auth(AUDITOR)
                    .header(
                        "bitrouter-installation-id",
                        expected.installation_id.as_str(),
                    )
                    .send()
                    .await?;
                ensure!(response.status() == StatusCode::BAD_REQUEST);
                ensure!(
                    response
                        .headers()
                        .get("cache-control")
                        .is_some_and(|value| value == "no-store")
                );
            }
            for path in [
                "/v1/messages",
                "/v1/responses",
                "/control/v1/tenancy/capabilities",
                "/control/v1/tenants/tenant",
                "/control/v1/reload",
            ] {
                let response = client
                    .post(format!("{endpoint}{path}"))
                    .bearer_auth(AUDITOR)
                    .header(
                        "bitrouter-installation-id",
                        expected.installation_id.as_str(),
                    )
                    .send()
                    .await?;
                ensure!(
                    response.status() == StatusCode::NOT_FOUND
                        || response.status() == StatusCode::METHOD_NOT_ALLOWED
                );
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        stop.cancel();
        let ended = tokio::time::timeout(TIMEOUT, server).await;
        let closed = audit.shutdown().await;
        checked?;
        ended???;
        closed?;
        drop(crate::tenancy::ownership::acquire_path(
            &projection.join("audit.sqlite"),
            false,
        )?);
        ensure!(files(&bundle)? == original_bundle && std::fs::read(&source)? == original);
    }
    Ok(())
}
#[tokio::test]
async fn actual_static_receipt_old_realm_audit_get_page_and_bundle_bytes_survive_restart()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let live_source = directory.path().join("authority.sqlite");
    let harness = file_harness(&live_source).await?;
    let client = Harness::client(false)?;
    let response = harness.send(&client).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    response.bytes().await?;
    let expected = harness.receipt(&client, &id).await?;
    ensure!(expected.attempts.len() == 1 && harness.calls.load(Ordering::SeqCst) == 1);
    harness.fixture.service.shutdown().await?;
    roundtrip(
        &harness.fixture.db,
        &live_source,
        &expected,
        harness.credential.secret.as_deref().context("old token")?,
    )
    .await?;
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    Ok(())
}

async fn digest_row(
    db: &DatabaseConnection,
    receipt: &ExecutionReceipt,
) -> Result<sea_orm::QueryResult> {
    db.query_one(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT content_digest,receipt_json,disposition_json,byte_count,unknown_outcome FROM tenancy_execution_receipts WHERE installation_id=? AND tenant_id=? AND execution_id=?",
        [receipt.installation_id.as_str().into(), receipt.tenant_id.as_str().into(), receipt.execution_id.as_str().into()],
    )).await?.context("actual retained receipt row")
}

async fn digest_read_case(
    ending: crate::tenancy::audit::tests::WaiterEnd,
    fatal: bool,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir()?;
    let live_source = directory.path().join("authority.sqlite");
    let harness = file_harness(&live_source).await?;
    let client = Harness::client(false)?;
    let response = harness.send(&client).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    response.bytes().await?;
    let expected = harness.receipt(&client, &id).await?;
    ensure!(expected.attempts.len() == 1 && harness.calls.load(Ordering::SeqCst) == 1);
    ensure!(expected.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed);
    harness.fixture.service.shutdown().await?;

    let db = &harness.fixture.db;
    let before = digest_row(db, &expected).await?;
    if fatal {
        let original: String = before.try_get("", "content_digest")?;
        let wrong = if original == "0".repeat(64) {
            "1".repeat(64)
        } else {
            "0".repeat(64)
        };
        let affected = db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tenancy_execution_receipts SET content_digest=? WHERE installation_id=? AND tenant_id=? AND execution_id=?",
            [wrong.clone().into(), expected.installation_id.as_str().into(), expected.tenant_id.as_str().into(), id.as_str().into()],
        )).await?;
        ensure!(affected.rows_affected() == 1);
        let after = digest_row(db, &expected).await?;
        ensure!(after.try_get::<String>("", "content_digest")? == wrong && wrong != original);
        for column in ["receipt_json", "disposition_json"] {
            ensure!(before.try_get::<String>("", column)? == after.try_get::<String>("", column)?);
        }
        ensure!(
            before.try_get::<i64>("", "byte_count")? == after.try_get::<i64>("", "byte_count")?
        );
        ensure!(
            before.try_get::<bool>("", "unknown_outcome")?
                == after.try_get::<bool>("", "unknown_outcome")?
        );
        ensure!(
            store::lookup(db, &expected.installation_id, &expected.tenant_id, &id)
                .await
                .is_err()
        );
    }
    // This is the precise bounded counterexample: all startup byte/state
    // inventory passes, while a wrong digest fails the exact point decoder.
    store::inventory(db, &expected.installation_id).await?;
    crate::tenancy::audit::tests::assert_file_database(db, &live_source).await?;
    let source = directory.path().join("retired.sqlite");
    std::fs::File::create(&source)?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "VACUUM INTO ?",
        [source.to_str().context("source path")?.into()],
    ))
    .await?;
    ensure!(
        std::fs::metadata(&source)?.len() > 0,
        "archive copy was not materialized"
    );
    let source_bytes = std::fs::read(&source)?;
    let bundle = directory.path().join("bundle");
    let report = crate::offline::snapshot(&source, &bundle).await?;
    let report = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
    let digest = report["evidence_digest"].as_str().context("anchor")?;
    crate::tenancy::audit::tests::held_receipt_read(&bundle, digest, &expected, ending, fatal)
        .await?;
    ensure!(std::fs::read(&source)? == source_bytes && harness.calls.load(Ordering::SeqCst) == 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_digest_cancelled_waiter_fences_archive_through_actual_close() -> Result<()> {
    digest_read_case(crate::tenancy::audit::tests::WaiterEnd::Cancel, true).await
}

#[tokio::test(flavor = "current_thread")]
async fn wrong_digest_elapsed_deadline_fences_archive_through_actual_close() -> Result<()> {
    digest_read_case(crate::tenancy::audit::tests::WaiterEnd::Deadline, true).await
}

#[tokio::test(flavor = "current_thread")]
async fn successful_read_waiter_cancel_or_deadline_keeps_archive_usable() -> Result<()> {
    for ending in [
        crate::tenancy::audit::tests::WaiterEnd::Cancel,
        crate::tenancy::audit::tests::WaiterEnd::Deadline,
    ] {
        digest_read_case(ending, false).await?;
    }
    Ok(())
}
