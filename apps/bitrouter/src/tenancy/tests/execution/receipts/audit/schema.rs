//! A legitimate snapshot anchor is not a receipt-key schema proof.
use super::*;
use sea_orm::TransactionTrait;
use std::os::unix::fs::PermissionsExt;
mod fixture;

async fn reacquire(path: &Path) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(owner) = crate::tenancy::ownership::acquire_path(path, false) {
                drop(owner);
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("actual file Lease was not released")?;
    Ok(())
}

async fn copy_database(db: &DatabaseConnection, path: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "VACUUM INTO ?",
        [path.to_str().context("copy path")?.into()],
    ))
    .await?;
    ensure!(std::fs::metadata(path)?.len() > 0);
    Ok(())
}

// SQL quote() includes every scalar's original type and bytes; no receipt/ledger
// payload is parsed, rewritten, canonicalized or synthesized by this fault setup.
async fn values(db: &impl ConnectionTrait) -> Result<Vec<Vec<String>>> {
    let mut output = Vec::new();
    for (table, columns, order) in [
        (
            "tenancy_execution_receipts",
            &[
                "installation_id",
                "tenant_id",
                "execution_id",
                "principal_id",
                "correlation_id",
                "receipt_json",
                "content_digest",
                "disposition_json",
                "byte_count",
                "unknown_outcome",
                "recorded_at",
            ][..],
            "installation_id COLLATE BINARY,tenant_id COLLATE BINARY,execution_id COLLATE BINARY",
        ),
        (
            "seaql_migrations",
            &["version", "applied_at"][..],
            "version COLLATE BINARY",
        ),
    ] {
        let projection = columns
            .iter()
            .map(|name| format!("quote(\"{name}\") AS \"{name}\""))
            .collect::<Vec<_>>()
            .join(",");
        let rows = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT {projection} FROM \"{table}\" ORDER BY {order} LIMIT 1025"),
            ))
            .await?;
        ensure!(rows.len() <= 1024);
        for row in rows {
            output.push(
                columns
                    .iter()
                    .map(|name| row.try_get::<String>("", name))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
        }
    }
    Ok(output)
}

#[derive(Clone, Copy, Debug)]
enum Change {
    TenantColumn,
    CorrelationColumn,
    IndexCollation,
    IndexDescending,
    IndexUnique,
}
impl Change {
    fn name(self) -> &'static str {
        match self {
            Self::TenantColumn => "tenant-column",
            Self::CorrelationColumn => "correlation-column",
            Self::IndexCollation => "index-collation",
            Self::IndexDescending => "index-descending",
            Self::IndexUnique => "index-unique",
        }
    }
}

async fn mutate_retired(path: &Path, change: Change) -> Result<()> {
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    let transaction = db.begin().await?;
    let changed = async {
        let before = values(&transaction).await?;
        let definition = transaction.query_one(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='tenancy_execution_receipts'".to_owned())).await?.context("receipt definition")?.try_get::<String>("", "sql")?;
        let original_index = transaction.query_one(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT sql FROM sqlite_schema WHERE type='index' AND name='idx_tenancy_receipts_correlation'".to_owned())).await?.context("receipt index")?.try_get::<String>("", "sql")?;
        match change {
            Change::TenantColumn | Change::CorrelationColumn => {
                let column = if matches!(change, Change::TenantColumn) {"tenant_id"} else {"correlation_id"};
                let token = format!("\"{column}\"");
                let start = definition.find(&token).context("exact generated identity column")?;
                let end = definition[start..].find(',').context("column delimiter")? + start;
                let part = &definition[start..end];
                ensure!(part.matches("COLLATE BINARY").count() == 1);
                let changed = format!("{}{}{}", &definition[..start], part.replace("COLLATE BINARY", "COLLATE NOCASE"), &definition[end..]);
                transaction.execute_unprepared("ALTER TABLE tenancy_execution_receipts RENAME TO archive_receipts_old").await?;
                transaction.execute_unprepared(&changed).await?;
                transaction.execute_unprepared("INSERT INTO tenancy_execution_receipts SELECT * FROM archive_receipts_old").await?;
                transaction.execute_unprepared("DROP TABLE archive_receipts_old").await?;
                transaction.execute_unprepared(&original_index).await?;
            }
            other => {
                transaction.execute_unprepared("DROP INDEX idx_tenancy_receipts_correlation").await?;
                let (unique, correlation, suffix) = match other {
                    Change::IndexCollation => ("", "correlation_id COLLATE NOCASE", ""),
                    Change::IndexDescending => ("", "correlation_id DESC", ""),
                    Change::IndexUnique => ("UNIQUE ", "correlation_id", ""),
                    Change::TenantColumn | Change::CorrelationColumn => anyhow::bail!("unexpected column change"),
                };
                transaction.execute_unprepared(&format!("CREATE {unique}INDEX idx_tenancy_receipts_correlation ON tenancy_execution_receipts (installation_id,tenant_id,{correlation},execution_id){suffix}")).await?;
            }
        }
        ensure!(values(&transaction).await? == before, "schema fault changed receipt cells or ledger");
        Ok::<_, anyhow::Error>(())
    }.await;
    let settled = if changed.is_ok() {
        transaction.commit().await
    } else {
        transaction.rollback().await
    };
    let closed = db.close().await;
    drop(owner);
    let released = reacquire(path).await;
    changed?;
    settled?;
    closed?;
    released
}

async fn anchor(source: &Path, bundle: &Path) -> Result<String> {
    let report = crate::offline::snapshot(source, bundle).await?;
    let report = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
    ensure!(
        report["evidence"]["complete"] == false && report["evidence"]["cutover_ready"] == false
    );
    let digest = report["evidence_digest"]
        .as_str()
        .context("anchor")?
        .to_owned();
    // This must remain supported by the generic Legacy snapshot/verify engine.
    // A failure here does not count as an archive compatibility rejection.
    crate::offline::verify(bundle, &digest).await?;
    Ok(digest)
}

#[tokio::test]
async fn real_receipt_archive_rejects_column_and_index_comparison_drift_after_valid_snapshot()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let (original, receipts) = fixture::source(directory.path()).await?;
    ensure!(receipts.len() == 4);
    let original_bytes = std::fs::read(&original)?;
    for change in [
        Change::TenantColumn,
        Change::CorrelationColumn,
        Change::IndexCollation,
        Change::IndexDescending,
        Change::IndexUnique,
    ] {
        let variant = directory.path().join(format!("{}.sqlite", change.name()));
        std::fs::copy(&original, &variant)?;
        std::fs::set_permissions(&variant, std::fs::Permissions::from_mode(0o600))?;
        mutate_retired(&variant, change).await?;
        let mutated_bytes = std::fs::read(&variant)?;
        let bundle = directory.path().join(format!("bundle-{}", change.name()));
        let digest = anchor(&variant, &bundle).await?;
        let before = files(&bundle)?;
        let projection = directory
            .path()
            .join(format!("projection-{}", change.name()));
        let outcome =
            crate::offline::audit::open(&bundle, &projection, &digest, CancellationToken::new())
                .await;
        if let Ok(archive) = outcome {
            archive.shutdown().await?;
            anyhow::bail!("incompatible {:?} reached archive readiness", change);
        }
        ensure!(projection.join("INCOMPLETE").exists() && !projection.join("audit.json").exists());
        reacquire(&projection.join("audit.sqlite")).await?;
        ensure!(files(&bundle)? == before && std::fs::read(&variant)? == mutated_bytes);
        ensure!(std::fs::read(&original)? == original_bytes);
    }
    Ok(())
}

async fn positive(
    bundle: &Path,
    projection: &Path,
    digest: &str,
    receipts: &[ExecutionReceipt],
    h2: bool,
) -> Result<()> {
    let stop = CancellationToken::new();
    let _stop_on_drop = stop.clone().drop_guard();
    let archive = crate::offline::audit::open(bundle, projection, digest, stop.clone()).await?;
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
        let archive = archive.clone();
        tokio::spawn(async move { archive.serve(listener, auth).await })
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
        let sdk = bitrouter_sdk::tenancy::TenancyClient::with_options(&endpoint, AUDITOR, options)?;
        let lower = receipts.first().context("lower receipt")?;
        let upper = receipts.last().context("upper receipt")?;
        ensure!(
            lower.tenant_id != upper.tenant_id
                && lower.tenant_id.as_str().to_ascii_uppercase() == upper.tenant_id.as_str()
        );
        for tenant in [&lower.tenant_id, &upper.tenant_id] {
            for correlation in ["case", "CASE"] {
                let correlation: bitrouter_ai::tenancy::identity::CorrelationId =
                    correlation.try_into()?;
                let mut expected: Vec<_> = receipts
                    .iter()
                    .filter(|r| r.tenant_id == *tenant && r.correlation_id == correlation)
                    .map(|r| r.execution_id.clone())
                    .collect();
                expected.sort();
                let page = sdk
                    .executions(&lower.installation_id, tenant, &correlation, None, 1)
                    .await?;
                ensure!(page.execution_ids == expected.iter().take(1).cloned().collect::<Vec<_>>());
                ensure!(page.multiple_executions == (expected.len() > 1));
                let cursor = if expected.len() > 1 {
                    expected.first().cloned()
                } else {
                    None
                };
                ensure!(page.next_after == cursor);
                if let Some(cursor) = cursor {
                    let next = sdk
                        .executions(
                            &lower.installation_id,
                            tenant,
                            &correlation,
                            Some(&cursor),
                            1,
                        )
                        .await?;
                    ensure!(
                        next.execution_ids == expected.iter().skip(1).cloned().collect::<Vec<_>>()
                            && next.next_after.is_none()
                            && next.multiple_executions
                    );
                }
            }
        }
        let client = Harness::client(h2)?;
        for receipt in receipts {
            let actual = sdk
                .execution(
                    &receipt.installation_id,
                    &receipt.tenant_id,
                    &receipt.execution_id,
                )
                .await?;
            ensure!(serde_json::to_value(actual)? == serde_json::to_value(receipt)?);
            let other = if receipt.tenant_id == lower.tenant_id {
                &upper.tenant_id
            } else {
                &lower.tenant_id
            };
            let response = client
                .get(format!(
                    "{endpoint}/control/v1/tenants/{other}/executions/{}",
                    receipt.execution_id
                ))
                .bearer_auth(AUDITOR)
                .header(
                    "bitrouter-installation-id",
                    receipt.installation_id.as_str(),
                )
                .send()
                .await?;
            ensure!(response.status() == StatusCode::NOT_FOUND);
            ensure!(
                response
                    .headers()
                    .get("cache-control")
                    .is_some_and(|v| v == "no-store")
            );
            ensure!(
                response.version()
                    == if h2 {
                        reqwest::Version::HTTP_2
                    } else {
                        reqwest::Version::HTTP_11
                    }
            );
            response.bytes().await?;
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    stop.cancel();
    let ended = tokio::time::timeout(TIMEOUT, server).await;
    let closed = archive.shutdown().await;
    let released = reacquire(&projection.join("audit.sqlite")).await;
    checked?;
    ended???;
    closed?;
    released
}

#[tokio::test]
async fn binary_archive_exact_case_pages_cursors_points_and_final_file_owner() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (source, receipts) = fixture::source(directory.path()).await?;
    let original = std::fs::read(&source)?;
    let bundle = directory.path().join("bundle");
    let digest = anchor(&source, &bundle).await?;
    let before = files(&bundle)?;
    positive(
        &bundle,
        &directory.path().join("http1"),
        &digest,
        &receipts,
        false,
    )
    .await?;
    #[cfg(feature = "http2")]
    positive(
        &bundle,
        &directory.path().join("http2"),
        &digest,
        &receipts,
        true,
    )
    .await?;
    ensure!(files(&bundle)? == before && std::fs::read(&source)? == original);
    reacquire(&source).await
}
