//! Actual workspace consumers, including the distinct verification admission.
use super::*;
use crate::storage::checkpoint::snapshot::{Invocation, RunStatus, SessionSnapshot};
use bitrouter_ai::core::protocol::{ToolOutcome, Verification};

const CONTENT: &str = "actual-owned-workspace-proof";

fn bounded_task(outstanding: u32, verification: bool) -> Result<TaskInput> {
    let mut value = task();
    value.text = "Read source.txt and report its exact contents. Do not modify files.".into();
    let limits = value.limits.as_mut().context("explicit tool run limits")?;
    limits.input_bytes = 8 * 1024;
    limits.model_attempts = 2;
    limits.outstanding_tools = outstanding;
    // A finite small caller profile also bounds each future model result.
    // The larger profile can legitimately fail a later full ACK projection.
    limits.checkpoint_bytes = 1024 * 1024;
    limits.unacknowledged_bytes = 2 * 1024 * 1024;
    if verification {
        value.verification = Some(Verification {
            tool: "read".into(),
            arguments: json!({"path":"source.txt"}),
        });
    }
    Ok(value)
}

fn invocation(state: &SessionSnapshot) -> Result<&Invocation> {
    let calls = state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.invocations)
        .collect::<Vec<_>>();
    ensure!(calls.len() == 1, "expected one actual workspace invocation");
    Ok(calls[0])
}

fn check_call(state: &SessionSnapshot, verification: bool, artifact_bytes: u64) -> Result<Value> {
    ensure!(state.run.as_ref().context("actual run")?.status == RunStatus::Completed);
    ensure!(state.manifest.artifact_quota_bytes == 32 * 1024 * 1024);
    let call = invocation(state)?;
    ensure!(
        call.dispatch.tool == "read"
            && call.dispatch.arguments == json!({"path":"source.txt"})
            && call.dispatch.verification == verification
            && call.consumed
    );
    let limits = call
        .dispatch
        .result_limits
        .context("actual frozen invocation limits")?;
    ensure!(limits.artifact_bytes == Some(artifact_bytes));
    let result = call.result.as_ref().context("actual filesystem result")?;
    ensure!(
        result.status == ToolOutcome::Succeeded
            && result.output.contains(CONTENT)
            && result.output.contains(&"x".repeat(4096))
    );
    Ok(serde_json::to_value(&call.dispatch)?)
}

async fn tool_provider(verification: bool) -> MockServer {
    let server = MockServer::start().await;
    // Actual Chat completions tool/result pairing, consumed by the App adapter.
    // https://platform.openai.com/docs/api-reference/chat/create
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let Ok(body) = serde_json::from_slice::<Value>(&request.body) else { return ResponseTemplate::new(400); };
            let Some(messages) = body["messages"].as_array() else { return ResponseTemplate::new(400); };
            let results = messages.iter().filter(|message| message["role"] == "tool").collect::<Vec<_>>();
            let choice = if !verification && results.is_empty() {
                json!({"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{
                    "id":"actual-read","type":"function","function":{"name":"read","arguments":json!({"path":"source.txt"}).to_string()}}]},"finish_reason":"tool_calls"})
            } else {
                if !verification && (results.len() != 1 || !results[0].to_string().contains(CONTENT)
                    || !results[0].to_string().contains(&"x".repeat(4096))) {
                    return ResponseTemplate::new(422);
                }
                json!({"index":0,"message":{"role":"assistant","content":"The parser fix is complete."},"finish_reason":"stop"})
            };
            ResponseTemplate::new(200).set_body_json(json!({"id":format!("actual-tool-{}",results.len()),
                "object":"chat.completion","created":0,"model":body["model"],"choices":[choice],
                "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,
                    "prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
                    "completion_tokens_details":{"reasoning_tokens":0}}}))
        }).mount(&server).await;
    server
}

async fn receipts(fixture: &Fixture, run: &str, count: usize) -> Result<()> {
    drained(fixture).await?;
    let units = records(fixture, "native_experiment_unit_v1").await?;
    ensure!(
        units.len() == 1
            && units[0]["run"] == run
            && units[0]["pending"] == 0
            && units[0]["steps"] == count
    );
    let steps = records(fixture, "native_experiment_step_v1").await?;
    ensure!(steps.len() == count && !steps.is_empty());
    for step in steps {
        ensure!(
            step["unit"] == units[0]["key"]
                && step["facts"]["run"] == run
                && step["ack"]["planned"] == true
                && step["publication"]["state"] == "published"
        );
        let execution = step["execution"]
            .as_str()
            .context("actual execution")?
            .try_into()?;
        let saved = crate::tenancy::receipts::store::lookup(
            &fixture.db,
            &fixture.installation,
            &fixture.tenant,
            &execution,
        )
        .await?
        .context("original035 receipt")?;
        ensure!(
            saved.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed
                && saved.usage.input_tokens == Some(4)
                && saved.usage.output_tokens == Some(2)
                && saved.usage.complete
                && saved.attempts.len() == 1
                && saved.attempts[0].outcome
                    == bitrouter_ai::tenancy::execution::Outcome::Completed
        );
    }
    let counters = records(fixture, "native_experiment_counter_v1").await?;
    ensure!(counters.len() == 1 && counters[0]["accepted"] == 1);
    ensure!(
        records(fixture, "native_experiment_step_pending_v1")
            .await?
            .is_empty()
            && records(fixture, "native_experiment_unit_pending_v1")
                .await?
                .is_empty()
    );
    Ok(())
}

async fn actual(outstanding: u32, verification: bool) -> Result<()> {
    let root = tempfile::tempdir()?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(90), Box::pin(async {
        let server = tool_provider(verification).await;
        let url = format!("{}/v1", server.uri());
        let (fixture, headers, _) = fixture(root.path(), &url, 1_000_000, 4).await?;
        let workspace = root.path().join("actual-tool"); std::fs::create_dir(&workspace)?;
        std::fs::write(workspace.join("source.txt"), format!("{CONTENT} {}\n", "x".repeat(4096)))?;
        let mut session = None;
        let checked = async {
            session = Some(open(&fixture,&headers,&url,root.path(),"actual-tool").await?);
            let current = session.as_mut().context("actual native owner")?;
            let state = current.run(bounded_task(outstanding,verification)?, Arc::new(Approval), CancellationToken::new()).await?;
            let dispatch = check_call(&state,verification,64*1024)?;
            let count = if verification {1} else {2};
            ensure!(server.received_requests().await.context("real provider sends")?.len() == count);
            receipts(&fixture,&state.run.as_ref().context("run")?.run_id,count).await?;
            let view = current.review_run().await?;
            ensure!(view.scoring.evidence.items.iter().filter(|item| item.method == "native.tool.call").count() == 1
                && view.scoring.evidence.items.iter().filter(|item| item.method == "native.tool.result").count() == 1);
            let frozen = freeze(&fixture,&headers).await?;
            ensure!(!frozen.entries.is_empty());
            let journal = fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT payload FROM bro_managed_sessions WHERE installation_id=? AND tenant_id=? AND logical_name=?",
                [fixture.installation.as_str().into(),fixture.tenant.as_str().into(),"actual-tool".into()])).await?.context("actual journal")?;
            let envelope: Value = serde_json::from_str(&journal.try_get::<String>("","payload")?)?;
            let starts = envelope["journal"]["starts"].as_object().context("real tool-start ledger")?;
            ensure!(starts.len() == 1 && starts.values().all(|start| start["command"] == dispatch
                && start["result"]["status"] == "succeeded"));
            let current = session.take().context("closing native owner")?;
            tokio::time::timeout(TIMEOUT,current.close()).await??;
            session = Some(open(&fixture,&headers,&url,root.path(),"actual-tool").await?);
            let restored = session.as_ref().context("actual restored owner")?.core().snapshot().await;
            ensure!(invocation(&restored)?.dispatch.result_limits == invocation(&state)?.dispatch.result_limits);
            ensure!(server.received_requests().await.context("sends after restore")?.len() == count);
            ensure!(freeze(&fixture,&headers).await? == frozen);
            Ok::<_,anyhow::Error>(())
        }.await;
        let closed = match session.take() { Some(current) => tokio::time::timeout(TIMEOUT,current.close()).await
            .map_err(anyhow::Error::from).and_then(|r|r.map_err(Into::into)), None => Ok(()) };
        let stopped = tokio::time::timeout(TIMEOUT,fixture.service.shutdown()).await
            .map_err(anyhow::Error::from).and_then(|r|r);
        let db_closed = fixture.db.clone().close().await;
        drop(fixture);
        let released = crate::tenancy::ownership::acquire_path(&root.path().join("experiment.db"),false).map(drop);
        checked.and(closed).and(stopped).and(db_closed.map_err(Into::into)).and(released)
    })).await.map_err(anyhow::Error::from).and_then(|r|r);
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(error.context(format!(
            "tool admission artifacts retained at {}",
            root.keep().display()
        ))),
    }
}

#[tokio::test]
async fn actual_experimental_workspace_read_with_one_outstanding_tool() -> Result<()> {
    actual(1, false).await
}
#[tokio::test]
async fn actual_experimental_workspace_read_with_eight_outstanding_tools() -> Result<()> {
    actual(8, false).await
}
#[tokio::test]
async fn actual_experimental_verification_reads_workspace_under_frozen_bound() -> Result<()> {
    actual(1, true).await
}

#[tokio::test]
async fn ordinary_owned_verification_keeps_original_manifest_and_artifact_share() -> Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        Box::pin(async {
            let mut provider = Provider::start(false).await?;
            let root = tempfile::tempdir()?;
            let (fixture, route) = super::super::file_fixture(&provider, root.path()).await?;
            let workspace = root.path().join("plain");
            std::fs::create_dir(&workspace)?;
            std::fs::write(
                workspace.join("source.txt"),
                format!("{CONTENT} {}\n", "x".repeat(4096)),
            )?;
            let mut session = None;
            let checked = async {
                let (headers, _) = super::super::super::ready(&fixture, &route).await?;
                let app = Arc::new(
                    host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                        .with_tenancy(fixture.service.clone()),
                );
                session = Some(
                    NativeSession::open_authenticated(
                        app.clone(),
                        headers.clone(),
                        "plain",
                        &workspace,
                        ToolMode::ReadOnly,
                        &HarnessConfig::default(),
                    )
                    .await?,
                );
                let mut input = super::super::input();
                let bounded = bounded_task(8, true)?;
                input.limits = bounded.limits;
                input.verification = bounded.verification;
                let state = session
                    .as_mut()
                    .context("plain owner")?
                    .run(input, Arc::new(Approval), CancellationToken::new())
                    .await?;
                check_call(&state, true, 32 * 1024 * 1024 / 42)?;
                provider.receive().await?;
                ensure!(provider.calls() == 1);
                let current = session.take().context("plain close owner")?;
                tokio::time::timeout(TIMEOUT, current.close()).await??;
                session = Some(
                    NativeSession::open_authenticated(
                        app,
                        headers,
                        "plain",
                        &workspace,
                        ToolMode::ReadOnly,
                        &HarnessConfig::default(),
                    )
                    .await?,
                );
                let restored = session
                    .as_ref()
                    .context("plain restored owner")?
                    .core()
                    .snapshot()
                    .await;
                ensure!(
                    invocation(&restored)?.dispatch.result_limits
                        == invocation(&state)?.dispatch.result_limits
                        && restored.manifest == state.manifest
                        && provider.calls() == 1
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let closed = match session {
                Some(current) => tokio::time::timeout(TIMEOUT, current.close())
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r.map_err(Into::into)),
                None => Ok(()),
            };
            super::super::super::finish(&fixture, provider, checked.and(closed)).await
        }),
    )
    .await?
}
