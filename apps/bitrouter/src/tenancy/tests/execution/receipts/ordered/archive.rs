//! Actual v2 provider observations survive the stopped-writer audit path.
use super::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::os::unix::fs::PermissionsExt;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn ordered_v2_receipt_and_evidence_survive_archive_http_without_new_dispatch() -> Result<()> {
    let harness = OrderedHarness::new(9, 64, None).await?;
    let binding = harness
        .binding((0..9).rev().collect(), GenerationProtocol::ChatCompletions)
        .await?;
    let id = harness
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await?;
    let receipt = harness.receipt(&id).await?;
    let evidence = harness
        .control(false, HttpVersion::Http1Only)?
        .usage_evidence_v2(&receipt)
        .await?;
    harness.fixture.service.shutdown().await?;
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("retired.sqlite");
    std::fs::File::create(&source)?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
    harness
        .fixture
        .db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "VACUUM INTO ?",
            [source.to_str().context("archive source path")?.into()],
        ))
        .await?;
    let original = std::fs::read(&source)?;
    let bundle = directory.path().join("bundle");
    let report = crate::offline::snapshot(&source, &bundle).await?;
    let report = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
    let digest = report["evidence_digest"]
        .as_str()
        .context("archive digest")?;
    let before = harness.observed()?;
    for version in [
        HttpVersion::Http1Only,
        #[cfg(feature = "http2")]
        HttpVersion::Http2PriorKnowledge,
    ] {
        let projection = directory.path().join(format!("projection-{version:?}"));
        let stop = CancellationToken::new();
        let _stop_on_drop = stop.clone().drop_guard();
        let archive = crate::offline::audit::open(&bundle, &projection, digest, stop).await?;
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: "ordered-archive-auditor".into(),
                    token_env: "ORDERED_ARCHIVE_AUDITOR".into(),
                    scopes: vec![ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some(OPERATOR.into()),
        )?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = {
            let archive = archive.clone();
            AbortOnDropHandle::new(tokio::spawn(
                async move { archive.serve(listener, auth).await },
            ))
        };
        let checked = async {
            let options = TenancyOptions {
                http_version: version,
                system_proxy: false,
                ..Default::default()
            };
            let sdk = TenancyClient::with_options(&endpoint, OPERATOR, options.clone())?;
            let actual = sdk
                .execution_v2(&receipt.installation_id, &receipt.tenant_id, &id)
                .await?;
            ensure!(serde_json::to_value(&actual)? == serde_json::to_value(&receipt)?);
            ensure!(
                serde_json::to_value(sdk.usage_evidence_v2(&actual).await?)?
                    == serde_json::to_value(&evidence)?
            );
            ensure!(
                sdk.execution(&receipt.installation_id, &receipt.tenant_id, &id)
                    .await
                    .is_err()
            );
            let page = sdk
                .executions(
                    &receipt.installation_id,
                    &receipt.tenant_id,
                    &receipt.correlation_id,
                    None,
                    100,
                )
                .await?;
            ensure!(page.execution_ids == vec![id.clone()]);
            let scoped = TenancyClient::with_options(&endpoint, harness.token()?, options)?;
            ensure!(
                scoped
                    .execution_v2(&receipt.installation_id, &receipt.tenant_id, &id)
                    .await
                    .is_err()
            );
            let builder = reqwest::Client::builder().no_proxy();
            #[cfg(feature = "http2")]
            let builder = if version == HttpVersion::Http2PriorKnowledge {
                builder.http2_prior_knowledge()
            } else {
                builder.http1_only()
            };
            let client = builder.build()?;
            for suffix in ["v2", "usage-evidence/v2"] {
                let url = format!(
                    "{endpoint}/control/v1/tenancy/tenants/{}/executions/{id}/{suffix}",
                    receipt.tenant_id
                );
                let reply = client
                    .get(&url)
                    .bearer_auth(OPERATOR)
                    .header(
                        "bitrouter-installation-id",
                        receipt.installation_id.as_str(),
                    )
                    .send()
                    .await?;
                ensure!(
                    reply.status() == StatusCode::OK
                        && reply
                            .headers()
                            .get("cache-control")
                            .is_some_and(|value| value == "no-store")
                );
                ensure!(
                    !String::from_utf8_lossy(&reply.bytes().await?)
                        .contains("owned-upstream-secret")
                );
                let denied = client
                    .get(&url)
                    .bearer_auth(OPERATOR)
                    .header("bitrouter-installation-id", "other-installation")
                    .send()
                    .await?;
                ensure!(!denied.status().is_success());
                let head = client
                    .head(&url)
                    .bearer_auth(OPERATOR)
                    .header(
                        "bitrouter-installation-id",
                        receipt.installation_id.as_str(),
                    )
                    .send()
                    .await?;
                ensure!(!head.status().is_success());
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = archive.shutdown().await;
        let served = server.await;
        checked?;
        closed?;
        served??;
    }
    ensure!(harness.observed()? == before && before.len() == 9);
    ensure!(std::fs::read(&source)? == original);
    Ok(())
}
