//! Actual workspace consumers, including the distinct verification admission.
use super::*;
use crate::storage::checkpoint::snapshot::{Invocation, RunStatus, SessionSnapshot};
use bitrouter_ai::core::protocol::{ToolOutcome, Verification};

const CONTENT: &str = "actual-owned-workspace-proof";

fn bounded_task(outstanding: u32, verification: bool) -> Result<TaskInput> {
    let mut value = input();
    value.limits = Some(bitrouter_ai::core::protocol::Limits {
        active_models: 1,
        agents: 1,
        child_depth: 1,
        model_attempts: 1,
        outstanding_tools: 1,
        total_tools: 1,
        queued_runs: 1,
        mailbox_messages: 1,
        active_seconds: 30,
        checkpoint_bytes: 160 * 1024,
        unacknowledged_bytes: 256 * 1024,
        ephemeral_bytes: 64 * 1024,
        input_bytes: 4096,
    });
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

#[tokio::test]
async fn ordinary_owned_verification_keeps_original_manifest_and_artifact_share() -> Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        Box::pin(async {
            let mut provider = Provider::start(false).await?;
            let root = tempfile::tempdir()?;
            let (fixture, route) = super::file_fixture(&provider, root.path()).await?;
            let workspace = root.path().join("plain");
            std::fs::create_dir(&workspace)?;
            std::fs::write(
                workspace.join("source.txt"),
                format!("{CONTENT} {}\n", "x".repeat(4096)),
            )?;
            let mut session = None;
            let checked = async {
                let (headers, _) = super::super::ready(&fixture, &route).await?;
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
                let mut input = super::input();
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
            super::super::finish(&fixture, provider, checked.and(closed)).await
        }),
    )
    .await?
}
