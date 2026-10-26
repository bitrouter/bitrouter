//! Exercise provider counting through the real persistent harness.
use super::*;
use crate::tenancy::tests::execution::native::counting;
use bitrouter_ai::tenancy::{execution::Provenance, identity::ExecutionId};

#[tokio::test]
async fn persistent_owned_harness_counts_before_generation_with_separate_evidence() -> Result<()> {
    for h2 in counting::protocols() {
        let mut provider = Provider::start(false).await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let (fixture, route) = file_fixture_profile(&provider, directory.path(), true).await?;
        let checked = async {
            counting::client(&fixture, h2)?;
            let (headers, _) = ready(&fixture, &route).await?;
            let app = Arc::new(
                host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone()),
            );
            let mut session = tokio::time::timeout(
                TIMEOUT,
                NativeSession::open_authenticated(
                    app,
                    headers,
                    "counted-session",
                    &workspace,
                    ToolMode::ReadOnly,
                    &HarnessConfig::default(),
                ),
            )
            .await??;
            let run: Result<()> = async {
                let state = tokio::time::timeout(
                    TIMEOUT,
                    session.run(input(), Arc::new(Approval), CancellationToken::new()),
                )
                .await??;
                ensure!(
                    state.run.as_ref().is_some_and(|run| run.status
                        == crate::storage::checkpoint::snapshot::RunStatus::Completed)
                );
                let counted = provider.receive().await?;
                let generated = provider.receive().await?;
                ensure!(counted.path == "/v1/responses/input_tokens");
                ensure!(generated.path == "/v1/responses");
                ensure!(counted.body.get("stream").is_none());
                // NativeSession uses managed::store::local::NativePort, whose
                // stream observation is disabled. The agent driver uses a
                // different Port; controlled-stream coverage lives in counting.rs.
                ensure!(generated.body["stream"] == false);
                ensure!(counted.body["input"] == generated.body["input"]);
                for request in [&counted, &generated] {
                    ensure!(request.version == counting::version(h2));
                    ensure!(request.headers["authorization"] == "Bearer owned-upstream-secret");
                    ensure!(request.headers["openai-organization"] == "count-owned-org");
                    ensure!(request.headers["openai-project"] == "count-owned-project");
                    ensure!(request.headers["x-private"] == "count-private");
                }
                ensure!(
                    counted.headers["x-bitrouter-request-id"]
                        == generated.headers["x-bitrouter-request-id"]
                );
                let id: ExecutionId = generated
                    .headers
                    .get("x-bitrouter-request-id")
                    .context("execution request id")?
                    .to_str()?
                    .try_into()?;
                let (receipt, evidence) = counting::facts_for_id(&fixture, &id).await?;
                ensure!(receipt.attempts.len() == 1);
                ensure!(receipt.usage.input_tokens == Some(4));
                ensure!(receipt.usage.output_tokens == Some(2));
                ensure!(receipt.usage.provenance == Provenance::Provider);
                ensure!(evidence.counts.len() == 1 && evidence.counts[0].input_tokens == Some(4));
                ensure!(provider.calls() == 2);
                Ok(())
            }
            .await;
            // Exercise and observe cleanup even when a run assertion fails.
            let closed = tokio::time::timeout(TIMEOUT, session.close()).await;
            run?;
            closed??;
            let row = fixture.db.query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT payload FROM bro_managed_sessions WHERE logical_name = 'counted-session'",
            )).await?.context("released counted session")?;
            let journal: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
            ensure!(journal["journal"]["released"] == true);
            Ok(())
        }
        .await;
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}
