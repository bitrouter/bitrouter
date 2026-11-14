//! Actual tenant admission, provider HTTP, wire delivery and durable readback.
use super::*;
use bitrouter_ai::tenancy::execution::Outcome;
#[cfg(feature = "server-tools")]
use bitrouter_ai::tenancy::execution::Provenance;
#[cfg(feature = "server-tools")]
use bitrouter_ai::types::ApiProtocol;
#[cfg(feature = "server-tools")]
use futures::StreamExt;

#[cfg(feature = "server-tools")]
mod cross_turn;

async fn converted_stream(
    harness: &Harness,
    path: &'static str,
    body: Value,
) -> Result<(ExecutionId, String)> {
    let router = Router::new()
        .route(path, post(|| async { StatusCode::NOT_FOUND }))
        .layer(axum::middleware::from_fn_with_state(
            harness.fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}{}", listener.local_addr()?, path);
    let _gateway =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let client = Harness::client(false)?;
    let response = client
        .post(endpoint)
        .headers(harness.headers.clone())
        .json(&body)
        .send()
        .await?;
    ensure!(
        response.status() == StatusCode::OK,
        "converted stream status {}",
        response.status()
    );
    let id = execution_id(&response)?;
    Ok((id, response.text().await?))
}

#[tokio::test]
async fn statusless_responses_snapshot_reaches_messages_tool_and_tenant_receipt() -> Result<()> {
    let item = json!({"type":"function_call","id":"item-statusless","call_id":"call-statusless",
        "name":"lookup","arguments":"{}"});
    let native = [
        json!({"type":"response.output_item.added","output_index":0,
            "item":{"type":"function_call","id":"item-statusless","call_id":"call-statusless","name":"lookup","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"item-statusless","delta":"{}"}),
        json!({"type":"response.completed","response":{"id":"resp-statusless","status":"completed","output":[item],
            "usage":{"input_tokens":40,"output_tokens":7,"total_tokens":47}}}),
    ].iter().map(|value| format!("data: {value}\n\n")).collect::<String>();
    let harness = Harness::with_reply("responses", StatusCode::OK, native, true, None).await?;
    let (id, wire) = converted_stream(
        &harness,
        "/v1/messages",
        json!({"model":MODEL,"stream":true,"max_tokens":25,
            "messages":[{"role":"user","content":"hello"}]}),
    )
    .await?;
    ensure!(wire.contains("call-statusless") && wire.contains("tool_use"));
    ensure!(wire.contains("event: message_stop") && !wire.contains("event: error"));
    let receipt = harness.receipt(&Harness::client(false)?, &id).await?;
    ensure!(receipt.outcome == Outcome::Completed);
    ensure!(receipt.usage.input_tokens == Some(40) && receipt.usage.output_tokens == Some(7));
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn nine_mib_chat_tool_crosses_tenant_responses_writer_and_commits() -> Result<()> {
    let arguments = format!(r#"{{"x":"{}"}}"#, "x".repeat(9 * 1024 * 1024));
    // The existing canonical handoff is 64 KiB. Use legal small provider
    // deltas while still exercising the 9 MiB accumulated call close.
    let mut native = String::new();
    for (index, chunk) in arguments.as_bytes().chunks(32 * 1024).enumerate() {
        let mut function = json!({"arguments":std::str::from_utf8(chunk)?});
        if index == 0 {
            function["name"] = json!("lookup");
        }
        native.push_str(&format!(
            "data: {}\n\n",
            json!({
                "id":"chat-large","choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":"call-large","function":function
                }]}}]
            })
        ));
    }
    native.push_str(&format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id":"chat-large","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":40,"completion_tokens":7,"total_tokens":47}
        })
    ));
    ensure!(native.len() < 16 * 1024 * 1024);
    let harness =
        Harness::with_reply("chat_completions", StatusCode::OK, native, true, None).await?;
    let (id, wire) = converted_stream(
        &harness,
        "/v1/responses",
        json!({"model":MODEL,"input":"hello","stream":true}),
    )
    .await?;
    let events = wire
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        !events
            .iter()
            .any(|event| event["type"] == "response.failed")
    );
    let done = events
        .iter()
        .find(|event| event["type"] == "response.function_call_arguments.done")
        .context("missing large arguments completion")?;
    ensure!(done["arguments"] == arguments);
    let item_done = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done")
        .context("missing large item completion")?;
    ensure!(item_done["item"]["arguments"] == arguments);
    let terminal = events.last().context("missing public terminal")?;
    ensure!(terminal["type"] == "response.completed");
    ensure!(terminal["response"]["output"][0]["arguments"] == arguments);
    let receipt = harness.receipt(&Harness::client(false)?, &id).await?;
    ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
    ensure!(receipt.usage.input_tokens == Some(40) && receipt.usage.output_tokens == Some(7));
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[cfg(feature = "server-tools")]
#[tokio::test]
async fn tenant_server_tool_stitcher_keeps_received_usage_on_upstream_error() -> Result<()> {
    use crate::gateway::pipeline::{
        operations::HookRegistration,
        server_tools::{
            approval::AllowAll, config::ServerToolLoopConfig, loop_controller::ServerToolLoop,
            toolset::ToolsetRegistry,
        },
        settlement::{SettlementContext, SettlementRecorder},
        types::{PipelineInput, PipelineRequest},
    };
    use async_trait::async_trait;
    use bitrouter_ai::types::OperationScope;
    use std::{any::TypeId, sync::Mutex};

    struct Recorder(Arc<Mutex<Vec<(u64, u64, bool)>>>);
    #[async_trait]
    impl SettlementRecorder for Recorder {
        async fn record(&self, ctx: &mut SettlementContext) -> crate::gateway::error::Result<()> {
            self.0
                .lock()
                .map_err(|_| crate::gateway::error::BitrouterError::UpstreamUnavailable)?
                .push((
                    ctx.prompt_tokens,
                    ctx.completion_tokens,
                    ctx.error.is_some(),
                ));
            Ok(())
        }
    }
    let native = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({"id":"chat-error","choices":[{"index":0,"delta":{"content":"before error"}}]}),
        json!({"id":"chat-error","choices":[],"usage":{"prompt_tokens":40,"completion_tokens":7,"total_tokens":47}}),
        json!({"error":{"type":"server_error","message":"fixture upstream error"}})
    );
    let harness =
        Harness::with_reply("chat_completions", StatusCode::OK, native, true, None).await?;
    let execution = super::super::admitted(&harness.fixture, &harness.headers).await?;
    let id = execution.id().clone();
    let mut pipeline = execution.pipeline()?;
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let unique = Arc::get_mut(&mut pipeline).context("fresh tenant pipeline is shared")?;
    unique.server_tool_loop = Some(Arc::new(ServerToolLoop::new(
        ToolsetRegistry::new(Vec::new()),
        ServerToolLoopConfig::default(),
        Arc::new(AllowAll),
    )));
    unique.settlement_recorders.push(HookRegistration::new(
        Arc::new(Recorder(recorded.clone())),
        OperationScope::Generation,
        TypeId::of::<Recorder>(),
    ));
    let PipelineInput::Generation(prompt) =
        crate::gateway::http::parse_input(&ApiProtocol::ChatCompletions, harness.request.clone())?
    else {
        anyhow::bail!("generation input");
    };
    let mut request = PipelineRequest::new(MODEL, execution.caller(), *prompt);
    request.request_id = id.to_string();
    request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
    request.headers = harness.headers.clone();
    execution.attach(pipeline.clone(), &mut request)?;
    let mut stream = pipeline.execute_stream(request).await?;
    let mut rejected = false;
    while let Some(part) = tokio::time::timeout(TIMEOUT, stream.next()).await? {
        match part {
            Ok(part) => ensure!(!part.is_terminal()),
            Err(_) => rejected = true,
        }
    }
    ensure!(rejected);
    drop(stream);
    drop(execution);
    ensure!(
        recorded
            .lock()
            .map_err(|_| anyhow::anyhow!("recorder poisoned"))?
            .as_slice()
            == [(40, 7, true)]
    );
    let receipt = harness.receipt(&Harness::client(false)?, &id).await?;
    ensure!(receipt.outcome == Outcome::Failed && receipt.attempts.len() == 1);
    ensure!(receipt.usage.input_tokens == Some(40) && receipt.usage.output_tokens == Some(7));
    ensure!(receipt.usage.provenance == Provenance::Provider);
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}
