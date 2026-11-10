//! Exercise the real HTTP send, body validation, stream and cancellation owners.

use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use serde_json::json;
use std::sync::Arc;
use tokio::{sync::Notify, time::Duration};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::{prompt, settings};
use crate::{
    config::{Config, routing_table::ConfigRoutingTable},
    gateway::{
        caller::CallerContext,
        error::BitrouterError,
        model::client::HttpTimeouts,
        pipeline::{
            builder::PipelineBuilder,
            context::{PipelineContext, StreamContext},
            executor::HttpExecutor,
            hooks::{HopOutcome, ObserveHook, Phase, RequestOutcome},
            types::{PipelineRequest, RoutingTarget},
        },
        scheduling::{Scheduler, observation::Shape},
    },
};

fn response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"id":"result", "model":"model",
        "choices":[{"index":0,"message":{"role":"assistant","content":"ready"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}))
}

fn builder(
    first: &MockServer,
    second: Option<&MockServer>,
    scheduler: Arc<Scheduler>,
) -> Result<PipelineBuilder> {
    let mut value = json!({"inherit_defaults":false,"registry":{"enabled":false},"providers":{
        "fixture":{"api_base":first.uri(),"api_key":"fixture-key","priority":1,
            "api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model"}]}
    }});
    if let Some(second) = second {
        value["providers"]["backup"] = json!({"api_base":second.uri(),"api_key":"backup-key","priority":2,
            "api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model"}]});
    }
    let config: Config = serde_json::from_value(value)?;
    let mut builder = PipelineBuilder::new();
    builder
        .routing_table(Arc::new(ConfigRoutingTable::from_config(config)))
        .executor(Arc::new(HttpExecutor::new(HttpTimeouts::default())?))
        .scheduler(scheduler, false);
    Ok(builder)
}

fn runtime(first: &MockServer, second: Option<&MockServer>) -> Result<Arc<Scheduler>> {
    let mut config = settings(&first.uri(), 2);
    config.limits.uncertainty_ttl_ms = 10000;
    if let Some(second) = second {
        let mut destination = config.destinations[0].clone();
        destination.id = "backup".into();
        destination.provider = "backup".into();
        destination.endpoint = second.uri();
        config.destinations.push(destination);
    }
    Scheduler::new(&config, 1).map_err(Into::into)
}

fn request(stream: bool) -> Result<PipelineRequest> {
    Ok(PipelineRequest::new(
        "model",
        CallerContext::local(),
        prompt(stream)?,
    ))
}

#[tokio::test]
async fn queued_http_work_sends_only_after_grant_and_never_after_exit() -> Result<()> {
    for mode in ["resume", "cancel", "timeout", "close"] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(response())
            .expect(u64::from(mode == "resume"))
            .mount(&upstream)
            .await;
        let scheduler = Scheduler::new(&settings(&upstream.uri(), 1), 1)?;
        let target = super::target(&upstream.uri())?;
        let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
        let blocker = scheduler.book(
            scheduler.candidate(&target, 0)?,
            &target,
            work,
            shape,
            tokio::time::Instant::now() + Duration::from_secs(60),
        )?;
        let pipeline = Arc::new(builder(&upstream, None, scheduler.clone())?.build()?);
        let worker = pipeline.clone();
        let req = request(false)?;
        let task = tokio::spawn(async move { worker.execute(req).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while scheduler.inspect_queue().queued != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        ensure!(!task.is_finished());
        ensure!(
            upstream
                .received_requests()
                .await
                .is_some_and(|r| r.is_empty())
        );
        ensure!(scheduler.inspect(shape)[0].active == 1);
        match mode {
            "resume" => {
                drop(blocker);
                ensure!(task.await??.result.generation().is_some());
            }
            "cancel" => {
                task.abort();
                ensure!(task.await.is_err());
                drop(blocker);
            }
            "timeout" => {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(3)).await;
                ensure!(matches!(
                    task.await?,
                    Err(BitrouterError::RateLimited { .. })
                ));
                tokio::time::resume();
                drop(blocker);
            }
            _ => {
                scheduler.close();
                ensure!(matches!(
                    task.await?,
                    Err(BitrouterError::UpstreamUnavailable)
                ));
                drop(blocker);
            }
        }
        pipeline.drain_required_pending_settlements().await?;
        ensure!(scheduler.inspect_queue().queued == 0 && scheduler.inspect_queue().bytes == 0);
        ensure!(scheduler.inspect(shape)[0].work == 0);
        scheduler.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn messages_and_responses_keep_known_failure_separate_from_malformed_output() -> Result<()> {
    fn event(kind: &str, value: serde_json::Value) -> String {
        format!("event: {kind}\ndata: {value}\n\n")
    }
    for protocol in ["messages", "responses"] {
        for stream in [false, true] {
            for mode in ["complete", "failed", "malformed"] {
                if mode == "failed" && protocol == "messages" && !stream {
                    continue;
                }
                let upstream = MockServer::start().await;
                let complete = if protocol == "messages" {
                    json!({"id":"m","type":"message","role":"assistant","model":"model",
                        "content":[{"type":"text","text":"ready"}],"stop_reason":"end_turn",
                        "usage":{"input_tokens":3,"output_tokens":1}})
                } else {
                    json!({"id":"r","object":"response","status":"completed","model":"model",
                    "output":[{"id":"m","type":"message","role":"assistant","status":"completed",
                        "content":[{"type":"output_text","text":"ready","annotations":[]}]}],
                    "usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}})
                };
                let failed = json!({"id":"r","status":"failed","output":[],
                    "error":{"type":"api_error","message":"provider failed"},"usage":{"input_tokens":3,"output_tokens":1}});
                let body = if mode == "malformed" {
                    if stream {
                        "data: {broken\n\n".into()
                    } else {
                        "{broken".into()
                    }
                } else if !stream {
                    serde_json::to_string(if mode == "complete" {
                        &complete
                    } else {
                        &failed
                    })?
                } else if mode == "failed" {
                    if protocol == "messages" {
                        event(
                            "error",
                            json!({"type":"error","error":{"type":"api_error","message":"provider failed"}}),
                        )
                    } else {
                        event(
                            "response.failed",
                            json!({"type":"response.failed","response":failed}),
                        )
                    }
                } else if protocol == "messages" {
                    [event("message_start",json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"model","content":[],"usage":{"input_tokens":3,"output_tokens":0}}})),
                        event("content_block_start",json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
                        event("content_block_delta",json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ready"}})),
                        event("content_block_stop",json!({"type":"content_block_stop","index":0})),
                        event("message_delta",json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}})),
                        event("message_stop",json!({"type":"message_stop"}))].concat()
                } else {
                    [event("response.output_text.delta",json!({"type":"response.output_text.delta","item_id":"m","output_index":0,"content_index":0,"delta":"ready"})),
                    event("response.completed",json!({"type":"response.completed","response":complete}))].concat()
                };
                let mut reply = ResponseTemplate::new(200).set_body_string(body);
                if stream {
                    reply = reply.insert_header("content-type", "text/event-stream");
                }
                Mock::given(method("POST"))
                    .respond_with(reply)
                    .expect(1)
                    .mount(&upstream)
                    .await;
                let scheduler = runtime(&upstream, None)?;
                let mut build = builder(&upstream, None, scheduler.clone())?;
                let config: Config = serde_json::from_value(
                    json!({"inherit_defaults":false,"providers":{"fixture":{
                        "api_base":upstream.uri(),"api_key":"fixture","models":[{"id":"model"}],"api_protocol":[{"*":protocol}]
                    }}}),
                )?;
                build.routing_table(Arc::new(ConfigRoutingTable::from_config(config)));
                let pipeline = Arc::new(build.build()?);
                if stream {
                    let mut output = pipeline.clone().execute_stream(request(true)?).await?;
                    let mut error = false;
                    while let Some(part) = output.next().await {
                        error |= part.is_err();
                    }
                    ensure!(error == (mode != "complete"), "{protocol}/{mode}");
                    drop(output);
                } else {
                    ensure!(
                        pipeline.execute(request(false)?).await.is_ok() == (mode == "complete"),
                        "{protocol}/{mode}"
                    );
                }
                pipeline.drain_required_pending_settlements().await?;
                let (shape, _) = Shape::from_prompt(&prompt(stream)?)?;
                let view = scheduler.inspect(shape);
                match mode {
                    "complete" => ensure!(
                        view[0].counters.completed == 1 && view[0].work == 0,
                        "{protocol}/{stream}"
                    ),
                    "failed" => ensure!(
                        view[0].counters.failed_terminal == 1 && view[0].uncertain == 0,
                        "{protocol}/{stream}"
                    ),
                    _ => ensure!(
                        view[0].uncertain == 1 && view[0].counters.completed == 0,
                        "{protocol}/{stream}"
                    ),
                }
                scheduler.shutdown().await?;
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn scheduled_json_and_sse_never_follow_redirects_outside_membership() -> Result<()> {
    for stream in [false, true] {
        for status in [307, 308] {
            let first = MockServer::start().await;
            let unselected = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status).insert_header(
                        "location",
                        format!("{}/chat/completions", unselected.uri()),
                    ),
                )
                .expect(1)
                .mount(&first)
                .await;
            Mock::given(method("POST"))
                .respond_with(response())
                .expect(0)
                .mount(&unselected)
                .await;
            let scheduler = runtime(&first, None)?;
            let pipeline = Arc::new(builder(&first, None, scheduler.clone())?.build()?);
            if stream {
                ensure!(
                    pipeline
                        .clone()
                        .execute_stream(request(true)?)
                        .await
                        .is_err()
                );
            } else {
                ensure!(pipeline.execute(request(false)?).await.is_err());
            }
            let (shape, _) = Shape::from_prompt(&prompt(stream)?)?;
            ensure!(scheduler.inspect(shape)[0].counters.rejected == 1);
            pipeline.drain_required_pending_settlements().await?;
            scheduler.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn scheduled_stream_rejects_post_terminal_content_and_duplicate_terminals() -> Result<()> {
    let content =
        "data: {\"choices\":[{\"delta\":{\"content\":\"late\"},\"finish_reason\":null}]}\n\n";
    let finish = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    for trailer in [
        format!("data: [DONE]\n\n{content}"),
        format!("data: [DONE]\n\n{finish}data: [DONE]\n\n"),
        format!("{content}data: [DONE]\n\n"),
        format!("{finish}data: [DONE]\n\n"),
        content.to_string(),
        finish.to_string(),
        "data: [DONE]\n\ndata: [DONE]\n\n".to_string(),
    ] {
        let upstream = MockServer::start().await;
        let body = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"ready\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n{trailer}"
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let scheduler = runtime(&upstream, None)?;
        let pipeline = Arc::new(builder(&upstream, None, scheduler.clone())?.build()?);
        let mut stream = pipeline.clone().execute_stream(request(true)?).await?;
        let mut failed = false;
        while let Some(part) = stream.next().await {
            match part {
                Err(_) => failed = true,
                Ok(bitrouter_ai::types::StreamPart::TextDelta { text }) => ensure!(text != "late"),
                Ok(part) => ensure!(!part.is_terminal()),
            }
        }
        ensure!(failed);
        drop(stream);
        pipeline.drain_required_pending_settlements().await?;
        let (shape, _) = Shape::from_prompt(&prompt(true)?)?;
        ensure!(
            scheduler.inspect(shape)[0].uncertain == 1
                && scheduler.inspect(shape)[0].counters.completed == 0
        );
        scheduler.shutdown().await?;
    }
    Ok(())
}

struct PendingHandoff {
    entered: Arc<Notify>,
    after_headers: bool,
}

#[async_trait::async_trait]
impl ObserveHook for PendingHandoff {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}
    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &bitrouter_ai::types::StreamPart) {}
    async fn on_request_end(&self, _ctx: &PipelineContext, _outcome: &RequestOutcome) {}
    async fn on_hop_start(&self, _ctx: &PipelineContext, _target: &RoutingTarget) {
        if !self.after_headers {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
    }
    async fn on_hop_end(
        &self,
        _ctx: &PipelineContext,
        _target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        if self.after_headers && matches!(outcome, HopOutcome::StreamStarted) {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
    }
}

#[tokio::test]
async fn deadlines_bound_observers_on_both_sides_of_stream_headers() -> Result<()> {
    for after_headers in [false, true] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: {}\n\n"),
            )
            .expect(u64::from(after_headers))
            .mount(&upstream)
            .await;
        let mut config = settings(&upstream.uri(), 1);
        config.queue.wait_timeout_ms = 1000;
        config.queue.request_timeout_ms = 1000;
        config.limits.uncertainty_ttl_ms = 10000;
        let scheduler = Scheduler::new(&config, 1)?;
        let entered = Arc::new(Notify::new());
        let mut build = builder(&upstream, None, scheduler.clone())?;
        build.observe_hook(PendingHandoff {
            entered: entered.clone(),
            after_headers,
        });
        let pipeline = Arc::new(build.build()?);
        let worker = pipeline.clone();
        let req = request(true)?;
        let task = tokio::spawn(async move { worker.execute_stream(req).await });
        tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(2)).await;
        ensure!(matches!(task.await?, Err(BitrouterError::UpstreamTimeout)));
        tokio::time::resume();
        pipeline.drain_required_pending_settlements().await?;
        let (shape, _) = Shape::from_prompt(&prompt(true)?)?;
        let view = scheduler.inspect(shape);
        ensure!(view[0].active == 0 && view[0].uncertain == u32::from(after_headers));
        scheduler.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn unpolled_stream_expires_without_releasing_its_owner_bookkeeping() -> Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: {}\n\n"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let mut config = settings(&upstream.uri(), 1);
    config.queue.wait_timeout_ms = 1000;
    config.queue.request_timeout_ms = 1000;
    let scheduler = Scheduler::new(&config, 1)?;
    let pipeline = Arc::new(builder(&upstream, None, scheduler.clone())?.build()?);
    let mut stream = pipeline.clone().execute_stream(request(true)?).await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let (shape, _) = Shape::from_prompt(&prompt(true)?)?;
    let view = scheduler.inspect(shape);
    ensure!(view[0].active == 0 && view[0].uncertain == 0 && view[0].work == 0);
    ensure!(
        scheduler.shutdown().await.is_err(),
        "unpolled owner was mistaken for joined custody"
    );
    ensure!(matches!(
        stream.next().await,
        Some(Err(BitrouterError::UpstreamTimeout))
    ));
    drop(stream);
    pipeline.drain_required_pending_settlements().await?;
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn known_rejection_allows_original_fallback_but_malformed_success_does_not() -> Result<()> {
    for rejected in [true, false] {
        let first = MockServer::start().await;
        let second = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(if rejected {
                ResponseTemplate::new(429).set_body_json(json!({"error":{"message":"busy"}}))
            } else {
                ResponseTemplate::new(200).set_body_string("{truncated")
            })
            .expect(1)
            .mount(&first)
            .await;
        Mock::given(method("POST"))
            .respond_with(response())
            .expect(if rejected { 1 } else { 0 })
            .mount(&second)
            .await;
        let scheduler = runtime(&first, Some(&second))?;
        let pipeline = builder(&first, Some(&second), scheduler.clone())?.build()?;
        let result = pipeline.execute(request(false)?).await;
        ensure!(result.is_ok() == rejected, "{result:?}");
        let (shape, _) = Shape::from_prompt(&prompt(false)?)?;
        let view = scheduler.inspect(shape);
        if rejected {
            ensure!(view[0].counters.rejected == 1 && view[0].work == 0);
            ensure!(view[1].counters.completed == 1 && view[1].work == 0);
        } else {
            ensure!(view[0].uncertain == 1 && view[1].counters.completed == 0);
        }
        pipeline.drain_required_pending_settlements().await?;
        scheduler.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn headers_and_useful_stream_output_keep_capacity_until_validated_eof() -> Result<()> {
    for (complete, sentinel) in [(true, true), (true, false), (false, false)] {
        let first = MockServer::start().await;
        let mut body = concat!(
            "data: {\"id\":\"s\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"s\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ready\"},\"finish_reason\":null}]}\n\n"
        ).to_string();
        if complete {
            body.push_str("data: {\"id\":\"s\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"last\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n");
            if sentinel {
                body.push_str("data: [DONE]\n\n");
            }
        }
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .expect(1)
            .mount(&first)
            .await;
        let scheduler = runtime(&first, None)?;
        let pipeline = Arc::new(builder(&first, None, scheduler.clone())?.build()?);
        let (shape, _) = Shape::from_prompt(&prompt(true)?)?;
        let mut stream = pipeline.clone().execute_stream(request(true)?).await?;
        ensure!(
            scheduler.inspect(shape)[0].active == 1,
            "headers released reservation"
        );
        let mut failed = false;
        while let Some(part) = stream.next().await {
            match part {
                Ok(bitrouter_ai::types::StreamPart::TextDelta { .. }) => {
                    ensure!(scheduler.inspect(shape)[0].active == 1)
                }
                Err(_) => failed = true,
                _ => {}
            }
        }
        ensure!(failed != complete);
        drop(stream);
        pipeline.drain_required_pending_settlements().await?;
        let view = scheduler.inspect(shape);
        ensure!(view[0].active == 0);
        if complete {
            ensure!(view[0].counters.completed == 1 && view[0].uncertain == 0);
            ensure!(view[0].counters.usage_reports == 1);
        } else {
            ensure!(view[0].uncertain == 1 && view[0].counters.completed == 0);
        }
        scheduler.shutdown().await?;
    }
    Ok(())
}

struct BeforeSend(Arc<Notify>);
#[async_trait::async_trait]
impl ObserveHook for BeforeSend {
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}
    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &bitrouter_ai::types::StreamPart) {}
    async fn on_request_end(&self, _ctx: &PipelineContext, _outcome: &RequestOutcome) {}
    async fn on_hop_start(&self, _ctx: &PipelineContext, _target: &RoutingTarget) {
        self.0.notify_one();
        std::future::pending::<()>().await;
    }
}

#[tokio::test]
async fn cancellation_on_each_side_of_send_has_distinct_cleanup() -> Result<()> {
    for sent in [false, true] {
        let first = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(response().set_delay(Duration::from_secs(10)))
            .expect(u64::from(sent))
            .mount(&first)
            .await;
        let scheduler = runtime(&first, None)?;
        let entered = Arc::new(Notify::new());
        let mut builder = builder(&first, None, scheduler.clone())?;
        if !sent {
            builder.observe_hook(BeforeSend(entered.clone()));
        }
        let pipeline = Arc::new(builder.build()?);
        let worker = pipeline.clone();
        let request = request(false)?;
        let task = tokio::spawn(async move { worker.execute(request).await });
        if sent {
            tokio::time::timeout(Duration::from_secs(2), async {
                while first
                    .received_requests()
                    .await
                    .is_none_or(|requests| requests.is_empty())
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
        } else {
            tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
        }
        task.abort();
        ensure!(task.await.is_err());
        pipeline.drain_required_pending_settlements().await?;
        let (shape, _) = Shape::from_prompt(&prompt(false)?)?;
        let view = scheduler.inspect(shape);
        ensure!(view[0].active == 0);
        ensure!(view[0].uncertain == u32::from(sent));
        ensure!(view[0].counters.unsent == u64::from(!sent));
        scheduler.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn total_deadline_stops_provider_work_without_renewal() -> Result<()> {
    let first = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(response().set_delay(Duration::from_secs(10)))
        .expect(1)
        .mount(&first)
        .await;
    let mut config = settings(&first.uri(), 1);
    config.limits.uncertainty_ttl_ms = 10000;
    config.queue.wait_timeout_ms = 1000;
    config.queue.request_timeout_ms = 1000;
    let scheduler = Scheduler::new(&config, 1)?;
    let pipeline = Arc::new(builder(&first, None, scheduler.clone())?.build()?);
    let worker = pipeline.clone();
    let req = request(false)?;
    let task = tokio::spawn(async move { worker.execute(req).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while first
            .received_requests()
            .await
            .is_none_or(|requests| requests.is_empty())
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    ensure!(matches!(task.await?, Err(BitrouterError::UpstreamTimeout)));
    let (shape, _) = Shape::from_prompt(&prompt(false)?)?;
    ensure!(scheduler.inspect(shape)[0].uncertain == 1);
    pipeline.drain_required_pending_settlements().await?;
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn prefix_http_json_and_sse_use_validated_terminal_and_cached_usage_only() -> Result<()> {
    for mode in ["json", "sse", "rejected", "malformed"] {
        let upstream = MockServer::start().await;
        let usage = json!({"prompt_tokens":3,"completion_tokens":1,"total_tokens":4,
            "prompt_tokens_details":{"cached_tokens":1}});
        let reply = match mode {
            "sse" => ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                .set_body_string(format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[{"index":0,"delta":{"content":"ready"},"finish_reason":"stop"}]}),
                    json!({"choices":[],"usage":usage}))),
            "rejected" => ResponseTemplate::new(429).set_body_json(json!({"error":{"message":"busy"}})),
            "malformed" => ResponseTemplate::new(200).set_body_string("{broken"),
            _ => ResponseTemplate::new(200).set_body_json(json!({"id":"result","model":"model",
                "choices":[{"index":0,"message":{"role":"assistant","content":"ready"},"finish_reason":"stop"}],"usage":usage})),
        };
        Mock::given(method("POST"))
            .respond_with(reply)
            .expect(1)
            .mount(&upstream)
            .await;
        let mut config = settings(&upstream.uri(), 2);
        config.policy = crate::config::inference_routing::Policy::PrefixAffinity;
        let scheduler = Scheduler::new(&config, 1)?;
        let pipeline = Arc::new(builder(&upstream, None, scheduler.clone())?.build()?);
        let mut req = request(mode == "sse")?;
        let crate::gateway::pipeline::types::PipelineInput::Generation(prompt) = &mut req.input
        else {
            anyhow::bail!("generation");
        };
        prompt.system = Some("shared system prefix ".repeat(100));
        let mut target = super::target(&upstream.uri())?;
        target.api_key = "fixture-key".into();
        let binding = scheduler
            .projector
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("projector"))?
            .prepare(
                std::slice::from_ref(&target),
                &PipelineContext::new(req.clone()),
            )
            .pop()
            .flatten()
            .ok_or_else(|| anyhow::anyhow!("projection"))?;
        if mode == "sse" {
            let mut stream = pipeline.clone().execute_stream(req).await?;
            while let Some(part) = stream.next().await {
                part?;
            }
            drop(stream);
        } else {
            ensure!(pipeline.execute(req).await.is_ok() == (mode == "json"));
        }
        pipeline.drain_required_pending_settlements().await?;
        let view = scheduler.inspect_prefix(&binding, None);
        match mode {
            "json" | "sse" => ensure!(
                view.successful > 0
                    && view.affinity_millionths > 0
                    && view.last_cached_tokens == Some((Some(1), None)),
                "{mode}: {view:?}"
            ),
            "rejected" => ensure!(view.entries == 0 && view.affinity_millionths == 0),
            _ => ensure!(view.unknown > 0 && view.affinity_millionths == 0),
        }
        let requests = upstream
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("requests"))?;
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body)?;
        ensure!(
            ["prefix", "digests", "namespace", "inference_routing"]
                .iter()
                .all(|key| body.get(key).is_none())
        );
        scheduler.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn adaptive_http_json_and_sse_prefer_warm_then_escape_assigned_load() -> Result<()> {
    use crate::config::inference_routing::Policy;
    for stream in [false, true] {
        for loaded in [false, true] {
            let cold = MockServer::start().await;
            let warm = MockServer::start().await;
            let reply = if stream {
                ResponseTemplate::new(200).insert_header("content-type","text/event-stream")
                    .set_body_string(format!("data: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":"ready"},"finish_reason":"stop"}]})))
            } else {
                response()
            };
            Mock::given(method("POST"))
                .respond_with(reply.clone())
                .expect(u64::from(loaded))
                .mount(&cold)
                .await;
            Mock::given(method("POST"))
                .respond_with(reply)
                .expect(u64::from(!loaded))
                .mount(&warm)
                .await;
            let mut config = settings(&cold.uri(), 16);
            config.policy = Policy::PrefixAffinity;
            config.destinations[0].max_active = 8;
            let mut destination = config.destinations[0].clone();
            destination.id = "backup".into();
            destination.provider = "backup".into();
            destination.endpoint = warm.uri();
            config.destinations.push(destination);
            let scheduler = Scheduler::new(&config, 1)?;
            let pipeline = Arc::new(builder(&cold, Some(&warm), scheduler.clone())?.build()?);
            let mut req = request(stream)?;
            let crate::gateway::pipeline::types::PipelineInput::Generation(prompt) = &mut req.input
            else {
                anyhow::bail!("generation");
            };
            prompt.system = Some("shared system prefix ".repeat(128));
            let (shape, work) = Shape::from_prompt(prompt)?;
            let mut target = super::target(&warm.uri())?;
            target.provider_name = "backup".into();
            target.api_key = "backup-key".into();
            let binding = scheduler
                .projector
                .as_ref()
                .context("projector")?
                .prepare(
                    std::slice::from_ref(&target),
                    &PipelineContext::new(req.clone()),
                )
                .pop()
                .flatten()
                .context("prefix")?;
            {
                let mut state = scheduler.inner.lock();
                let index = state.prefix.as_mut().context("index")?;
                index.observe(
                    777,
                    &binding,
                    crate::gateway::scheduling::prefix::Event::Sent,
                    tokio::time::Instant::now(),
                );
                index.observe(
                    777,
                    &binding,
                    crate::gateway::scheduling::prefix::Event::Succeeded,
                    tokio::time::Instant::now(),
                );
            }
            let blocker = if loaded {
                Some(scheduler.book(
                    scheduler.candidate(&target, 0)?,
                    &target,
                    work,
                    shape,
                    tokio::time::Instant::now() + Duration::from_secs(30),
                )?)
            } else {
                None
            };
            if stream {
                let mut output = pipeline.clone().execute_stream(req).await?;
                while let Some(part) = output.next().await {
                    part?;
                }
                drop(output);
            } else {
                pipeline.execute(req).await?;
            }
            pipeline.drain_required_pending_settlements().await?;
            ensure!(cold.received_requests().await.context("cold")?.len() == usize::from(loaded));
            ensure!(warm.received_requests().await.context("warm")?.len() == usize::from(!loaded));
            drop(blocker);
            scheduler.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn adaptive_conversion_mask_skips_saturated_input_and_bypasses_all_excluded_queue()
-> Result<()> {
    for stream in [false, true] {
        for all_excluded in [false, true] {
            let first = MockServer::start().await;
            let second = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(response())
                .expect(0)
                .mount(&first)
                .await;
            let reply = if stream {
                ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string(format!("data: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":"ready"},"finish_reason":"stop"}]})))
            } else {
                response()
            };
            Mock::given(method("POST"))
                .respond_with(reply)
                .expect(u64::from(!all_excluded))
                .mount(&second)
                .await;
            let config: Config = serde_json::from_value(
                json!({"inherit_defaults":false,"registry":{"enabled":false},"providers":{
                    "fixture":{"api_base":first.uri(),"api_key":"fixture-key","priority":1,"api_protocol":[{"*":"messages"}],"models":[{"id":"model"}]},
                    "backup":{"api_base":second.uri(),"api_key":"backup-key","priority":2,"api_protocol":[{"*":if all_excluded {"messages"} else {"chat_completions"}}],"models":[{"id":"model"}]}
                }}),
            )?;
            let mut settings = settings(&first.uri(), 2);
            settings.destinations[0].max_active = 1;
            let mut backup = settings.destinations[0].clone();
            backup.id = "backup".into();
            backup.provider = "backup".into();
            backup.endpoint = second.uri();
            settings.destinations.push(backup);
            let scheduler = Scheduler::new(&settings, 1)?;
            let mut pipeline = PipelineBuilder::new();
            pipeline
                .routing_table(Arc::new(ConfigRoutingTable::from_config(config)))
                .executor(Arc::new(HttpExecutor::new(HttpTimeouts::default())?))
                .scheduler(scheduler.clone(), false);
            let pipeline = Arc::new(pipeline.build()?);
            let mut req = request(stream)?;
            let crate::gateway::pipeline::types::PipelineInput::Generation(prompt) = &mut req.input
            else {
                anyhow::bail!("generation");
            };
            prompt.response_format = Some(bitrouter_ai::types::ResponseFormat::JsonSchema {
                name: Some("result".into()),
                description: None,
                strict: Some(true),
                schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            });
            let (shape, work) = Shape::from_prompt(prompt)?;
            let mut target = super::target(&first.uri())?;
            target.api_protocol = bitrouter_ai::types::ApiProtocol::Messages;
            let holder = scheduler.book(
                scheduler.candidate(&target, 0)?,
                &target,
                work,
                shape,
                tokio::time::Instant::now() + Duration::from_secs(30),
            )?;
            let result = if stream {
                match pipeline.clone().execute_stream(req).await {
                    Ok(mut output) => {
                        while let Some(item) = output.next().await {
                            item?;
                        }
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            } else {
                pipeline.execute(req).await.map(|_| ())
            };
            if all_excluded {
                ensure!(matches!(result, Err(BitrouterError::Incompatible { .. })));
            } else {
                result?;
            }
            ensure!(scheduler.inspect_queue().totals[0] == u64::from(!all_excluded));
            drop(holder);
            pipeline.drain_required_pending_settlements().await?;
            scheduler.shutdown().await?;
        }
    }
    Ok(())
}
