//! Real tenant admission and two actual provider HTTP turns, with independent
//! raw evidence and canonical settlement assertions at the error boundary.
use super::*;
use crate::gateway::{
    error::BitrouterError,
    pipeline::{
        operations::HookRegistration,
        server_tools::{
            approval::AllowAll,
            config::ServerToolLoopConfig,
            loop_controller::ServerToolLoop,
            toolset::{RouterToolset, ToolContext, ToolsetRegistry},
        },
        settlement::{SettlementContext, SettlementRecorder},
        types::{PipelineInput, PipelineRequest},
    },
};
use async_trait::async_trait;
use bitrouter_ai::{
    tenancy::{
        execution::UsageDimension,
        usage_evidence::{Dimension, ExecutionUsageEvidenceV1, Source},
    },
    types::{OperationScope, Tool, ToolResultOutput, UsageOrigin},
};
use std::{any::TypeId, collections::VecDeque, sync::Mutex};

mod custody;

struct Search(Arc<AtomicUsize>);
#[async_trait]
impl RouterToolset for Search {
    async fn list_tools(&self, _: &ToolContext) -> crate::gateway::error::Result<Vec<Tool>> {
        Ok(vec![Tool::Function {
            name: "search".into(),
            description: None,
            parameters: json!({"type":"object"}),
            strict: None,
            provider_metadata: Default::default(),
        }])
    }
    async fn call_tool(
        &self,
        _: &str,
        _: &str,
        _: &ToolContext,
    ) -> crate::gateway::error::Result<ToolResultOutput> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResultOutput::Text {
            value: "ran".into(),
        })
    }
    fn owns(&self, name: &str) -> bool {
        name == "search"
    }
}

#[derive(Debug)]
struct Settlement {
    input: u64,
    output: u64,
    origin: UsageOrigin,
    raw: Option<Value>,
    embedded: Option<(u64, u64)>,
    failed: bool,
    error: String,
}
struct Recorder(Arc<Mutex<Vec<Settlement>>>);
#[async_trait]
impl SettlementRecorder for Recorder {
    async fn record(&self, ctx: &mut SettlementContext) -> crate::gateway::error::Result<()> {
        let embedded = match &ctx.error {
            Some(BitrouterError::UpstreamInvalidResponse {
                usage: Some(usage), ..
            }) => Some((usage.prompt_tokens, usage.completion_tokens)),
            _ => None,
        };
        self.0
            .lock()
            .map_err(|_| BitrouterError::UpstreamUnavailable)?
            .push(Settlement {
                input: ctx.prompt_tokens,
                output: ctx.completion_tokens,
                origin: ctx.usage_origin,
                raw: ctx.raw_usage.clone(),
                embedded,
                failed: ctx.error.is_some(),
                error: format!("{:?}", ctx.error),
            });
        Ok(())
    }
}

fn frame(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn message(kind: &str, mut value: Value) -> String {
    value["type"] = json!(kind);
    format!("event: {kind}\ndata: {value}\n\n")
}
fn turns(protocol: &ApiProtocol) -> Result<(String, String)> {
    match protocol {
        ApiProtocol::ChatCompletions => {
            let first = frame(
                json!({"id":"first","choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":0,"id":"c1","function":{"name":"search","arguments":"{}"}
                }]}}]}),
            ) + &frame(
                json!({"id":"first","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}),
            ) + "data: [DONE]\n\n";
            let second = frame(
                json!({"id":"second","choices":[{"index":0,"delta":{"content":"second turn"}}]}),
            ) + &frame(
                json!({"id":"second","choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"id":"c2","function":{"name":"search","arguments":"{}"}},
                    {"index":0,"id":"conflict","function":{"name":"search","arguments":"{}"}}
                ]}}],"usage":{"prompt_tokens":40,"completion_tokens":7,"total_tokens":47}}),
            );
            Ok((first, second))
        }
        ApiProtocol::Responses => {
            let item = json!({"type":"function_call","id":"item1","call_id":"c1","name":"search","arguments":"{}","status":"completed"});
            let first = frame(json!({"type":"response.output_item.added","output_index":0,
                "item":{"type":"function_call","id":"item1","call_id":"c1","name":"search","arguments":""}}))
                + &frame(
                    json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"item1","delta":"{}"}),
                )
                + &frame(
                    json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":"item1","arguments":"{}"}),
                )
                + &frame(json!({"type":"response.output_item.done","output_index":0,"item":item}))
                + &frame(
                    json!({"type":"response.completed","response":{"id":"first","status":"completed","output":[item],
                    "usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13}}}),
                );
            let second = frame(
                json!({"type":"response.failed","response":{"id":"second","status":"failed",
                "error":{"code":"server_error","message":"fixture failure"},
                "usage":{"input_tokens":40,"output_tokens":7,"total_tokens":47}}}),
            );
            Ok((first, second))
        }
        ApiProtocol::Messages => {
            let tool = |id, input, output| {
                message(
                    "message_start",
                    json!({"message":{"id":id,"usage":{"input_tokens":input,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
                ) + &message(
                    "content_block_start",
                    json!({"index":0,"content_block":{"type":"tool_use","id":if input == 10 {"c1"} else {"c2"},"name":"search","input":{}}}),
                ) + &message(
                    "content_block_delta",
                    json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
                ) + &message("content_block_stop", json!({"index":0}))
                    + &message(
                        "message_delta",
                        json!({"delta":{},"usage":{"output_tokens":output}}),
                    )
            };
            let first = tool("first", 10, 3)
                + &message("message_delta", json!({"delta":{"stop_reason":"tool_use"}}))
                + &message("message_stop", json!({}));
            let second = tool("second", 40, 7)
                + &message(
                    "error",
                    json!({"error":{"type":"api_error","message":"fixture failure"}}),
                );
            Ok((first, second))
        }
        _ => anyhow::bail!("only the three compatible protocols"),
    }
}

async fn multi_round(
    protocol: ApiProtocol,
    name: &str,
    path: &'static str,
    hold_first_eof: bool,
) -> Result<()> {
    let (first, second) = turns(&protocol)?;
    let queue = Arc::new(Mutex::new(VecDeque::from([first, second])));
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let provider_requests = requests.clone();
    let eof_gate = Arc::new(Semaphore::new(0));
    let prefix_written = Arc::new(tokio::sync::Notify::new());
    let provider_gate = eof_gate.clone();
    let provider_prefix = prefix_written.clone();
    let router = Router::new().route(
        path,
        post(move |headers: HeaderMap, Json(request): Json<Value>| {
            let authenticated = headers
                .get("authorization")
                .is_some_and(|v| v == "Bearer owned-upstream-secret")
                || headers
                    .get("x-api-key")
                    .is_some_and(|v| v == "owned-upstream-secret");
            let body = queue.lock().ok().and_then(|mut queue| queue.pop_front());
            let first = provider_requests.lock().ok().map(|mut requests| {
                let first = requests.is_empty();
                requests.push(request);
                first
            });
            let gate = provider_gate.clone();
            let prefix = provider_prefix.clone();
            async move {
                if !authenticated || first.is_none() {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                match body {
                    Some(body) if hold_first_eof && first == Some(true) => {
                        let body = axum::body::Body::from_stream(async_stream::stream! {
                            yield Ok::<_,std::convert::Infallible>(axum::body::Bytes::from(body));
                            prefix.notify_one();
                            if let Ok(permit) = gate.acquire().await { permit.forget(); }
                        });
                        ([("content-type", "text/event-stream")], body).into_response()
                    }
                    Some(body) => ([("content-type", "text/event-stream")], body).into_response(),
                    None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let (fixture, route, _) =
        provision_protocol(&format!("http://{}/v1", listener.local_addr()?), name).await?;
    let _provider =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let revision = draft(&fixture, &route, "cross-turn-config", 25).await?;
    let issued = fixture.issue("cross-turn-client").await?;
    let token = issued.secret.as_deref().context("credential")?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(&fixture, token, &revision.revision_id)?;
    let submission = fixture
        .service
        .submission(&headers)
        .await
        .map_err(failure)?;
    let execution = fixture
        .service
        .admit_execution(submission, MODEL, &protocol)
        .await
        .map_err(failure)?;
    let id = execution.id().clone();
    let mut pipeline = execution.pipeline()?;
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let unique = Arc::get_mut(&mut pipeline).context("fresh tenant pipeline shared")?;
    unique.server_tool_loop = Some(Arc::new(ServerToolLoop::new(
        ToolsetRegistry::new(vec![Arc::new(Search(calls.clone()))]),
        ServerToolLoopConfig::default(),
        Arc::new(AllowAll),
    )));
    unique.settlement_recorders.push(HookRegistration::new(
        Arc::new(Recorder(recorded.clone())),
        OperationScope::Generation,
        TypeId::of::<Recorder>(),
    ));
    let body = match protocol {
        ApiProtocol::Responses => json!({"model":MODEL,"input":"hello","stream":true}),
        _ => {
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25,"stream":true})
        }
    };
    let PipelineInput::Generation(prompt) = crate::gateway::http::parse_input(&protocol, body)?
    else {
        anyhow::bail!("generation input")
    };
    let mut request = PipelineRequest::new(MODEL, execution.caller(), *prompt);
    request.request_id = id.to_string();
    request.inbound_protocol = Some(protocol.clone());
    request.headers = headers;
    execution.attach(pipeline.clone(), &mut request)?;
    {
        let drain = async {
            let mut stream = pipeline.execute_stream(request).await?;
            let mut errors = 0;
            while let Some(part) = tokio::time::timeout(TIMEOUT, stream.next()).await? {
                match part {
                    Ok(part) => ensure!(
                        !part.is_terminal(),
                        "premature successful terminal: {part:?}"
                    ),
                    Err(_) => errors += 1,
                }
            }
            ensure!(errors == 1 && calls.load(Ordering::SeqCst) == 1);
            anyhow::Ok(())
        };
        tokio::pin!(drain);
        if hold_first_eof {
            tokio::select! {
                result = &mut drain => { result?; anyhow::bail!("completed while first EOF was held"); }
                () = prefix_written.notified() => {}
                () = tokio::time::sleep(TIMEOUT) => anyhow::bail!("first provider prefix stalled"),
            }
            tokio::select! {
                result = &mut drain => { result?; anyhow::bail!("completed while first EOF was held"); }
                () = tokio::time::sleep(Duration::from_millis(30)) => {}
            }
            ensure!(
                requests
                    .lock()
                    .map_err(|_| anyhow::anyhow!("requests lock"))?
                    .len()
                    == 1
            );
            ensure!(
                calls.load(Ordering::SeqCst) == 0,
                "tool ran before validated provider EOF"
            );
            eof_gate.add_permits(1);
        }
        tokio::time::timeout(TIMEOUT, &mut drain).await??;
    }
    drop(execution);
    {
        let settlements = recorded
            .lock()
            .map_err(|_| anyhow::anyhow!("recorder lock"))?;
        ensure!(settlements.len() == 1, "settlements {settlements:?}");
        let settlement = &settlements[0];
        ensure!(
            settlement.failed && (settlement.input, settlement.output) == (50, 10),
            "{settlement:?}; {}",
            settlement.error
        );
        ensure!(settlement.origin == UsageOrigin::ProviderReported);
        if matches!(protocol, ApiProtocol::ChatCompletions) {
            ensure!(settlement.embedded == Some((50, 10)));
        }
        let raw =
            settlement.raw.as_ref().context("provider raw fragments")?["provider_usage_fragments"]
                .as_array()
                .context("two original raw snapshots")?;
        ensure!(raw.len() == 2, "raw {raw:?}");
        let counters = |value: &Value| -> Result<(u64, u64)> {
            let (input, output) = match protocol {
                ApiProtocol::Messages => (
                    &value["message_start"]["input_tokens"],
                    &value["message_delta"]["output_tokens"],
                ),
                ApiProtocol::ChatCompletions => {
                    (&value["prompt_tokens"], &value["completion_tokens"])
                }
                _ => (&value["input_tokens"], &value["output_tokens"]),
            };
            Ok((
                input.as_u64().context("raw input")?,
                output.as_u64().context("raw output")?,
            ))
        };
        ensure!(
            counters(&raw[0])? == (10, 3) && counters(&raw[1])? == (40, 7),
            "raw {raw:?}"
        );
    }
    // Use the authenticated receipt authority, not a DTO-only or store read.
    let caller = fixture.service.authenticate(token).await.map_err(failure)?;
    let bytes = fixture
        .service
        .read_receipt(
            caller.clone(),
            fixture.tenant.clone(),
            read::Request::One(id.clone()),
        )
        .await
        .map_err(failure)?;
    let receipt: ExecutionReceipt = serde_json::from_slice(&bytes)?;
    ensure!(receipt.attempts.len() == 2, "receipt {receipt:?}");
    ensure!(
        receipt.usage.input_tokens == Some(50) && receipt.usage.output_tokens == Some(10),
        "receipt {receipt:?}"
    );
    ensure!(receipt.usage.provenance == Provenance::Provider && !receipt.usage.complete);
    for (index, attempt) in receipt.attempts.iter().enumerate() {
        let (input, output) = if index == 0 { (10, 3) } else { (40, 7) };
        ensure!(
            attempt.usage.input_tokens == Some(input)
                && attempt.usage.output_tokens == Some(output)
        );
        ensure!(attempt.usage.provenance == Provenance::Provider);
        ensure!(
            attempt
                .native_usage
                .iter()
                .any(|n| n.name == UsageDimension::InputTokens && n.value == input)
        );
        ensure!(
            attempt
                .native_usage
                .iter()
                .any(|n| n.name == UsageDimension::OutputTokens && n.value == output)
        );
        let outcome = if index == 0 {
            Outcome::Completed
        } else if matches!(protocol, ApiProtocol::ChatCompletions) {
            Outcome::Unknown
        } else {
            Outcome::Failed
        };
        ensure!(attempt.outcome == outcome, "attempt {attempt:?}");
    }
    let bytes = fixture
        .service
        .read_receipt(
            caller,
            fixture.tenant.clone(),
            read::Request::UsageEvidence(id),
        )
        .await
        .map_err(failure)?;
    let evidence: ExecutionUsageEvidenceV1 = serde_json::from_slice(&bytes)?;
    ensure!(!evidence.truncated && evidence.dropped_frames == 0);
    for (index, attempt) in receipt.attempts.iter().enumerate() {
        let (input, output) = if index == 0 { (10, 3) } else { (40, 7) };
        let frames = evidence
            .frames
            .iter()
            .filter(|frame| frame.attempt_id == attempt.attempt_id)
            .collect::<Vec<_>>();
        ensure!(
            frames.len()
                == if matches!(protocol, ApiProtocol::Messages) {
                    2
                } else {
                    1
                },
            "duplicate/missing native frames: {frames:?}"
        );
        let expected_source = match protocol {
            ApiProtocol::ChatCompletions => Source::ChatChunk,
            ApiProtocol::Responses => Source::ResponseTerminal,
            _ => Source::MessageDelta,
        };
        ensure!(frames.iter().any(|frame| {
            frame.source == expected_source
                && frame
                    .quantities
                    .iter()
                    .any(|q| q.dimension == Dimension::OutputTokens && q.value == Some(output))
        }));
        ensure!(frames.iter().any(|frame| {
            frame
                .quantities
                .iter()
                .any(|q| q.dimension == Dimension::InputTokens && q.value == Some(input))
        }));
        ensure!(frames.iter().all(|frame| !frame.invalid && frame.quantities.iter().all(|q| q.value != Some(50))),"never store canonical totals as native evidence");
    }
    let sent = requests
        .lock()
        .map_err(|_| anyhow::anyhow!("request lock"))?
        .clone();
    ensure!(sent.len() == 2);
    ensure!(
        sent[1].to_string().contains("ran") && sent[1].to_string().contains("c1"),
        "missing real tool result on second send"
    );
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn chat_embedded_error_keeps_cross_turn_settlement_and_raw_tenant_evidence() -> Result<()> {
    multi_round(
        ApiProtocol::ChatCompletions,
        "chat_completions",
        "/v1/chat/completions",
        false,
    )
    .await
}
#[tokio::test]
async fn responses_failure_keeps_cross_turn_settlement_and_raw_tenant_evidence() -> Result<()> {
    multi_round(ApiProtocol::Responses, "responses", "/v1/responses", false).await
}
#[tokio::test]
async fn messages_failure_keeps_cross_turn_settlement_and_raw_tenant_evidence() -> Result<()> {
    multi_round(ApiProtocol::Messages, "messages", "/v1/messages", false).await
}

#[tokio::test]
async fn messages_first_turn_holds_tools_and_next_send_until_real_http_eof() -> Result<()> {
    multi_round(ApiProtocol::Messages, "messages", "/v1/messages", true).await
}
