//! Provider fixtures served through the gateway's HTTP ingress and lifecycle.

use std::{pin::Pin, sync::Arc};

use bitrouter_ai::{
    decisions::{DecisionRequest, DecisionResult},
    routing::ModelInfo,
    types::{ApiProtocol, GenerateResult, OperationScope, Prompt, StreamPart},
};
use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::gateway::{
    assembly::App,
    caller::CallerContext,
    model::{
        client::{HttpTimeouts, ModelClient, parse_retry_after},
        error::{ModelError, Result},
        protocol::{OutboundDispatch, SseEvent, decisions::DecisionsCodec},
        target::{CredentialPriority, ModelTarget},
    },
    pipeline::{
        executor::HttpExecutor,
        routing::{RoutingPrefs, RoutingTable},
        types::RoutingTarget,
    },
    providers::auth::AuthAppliers,
};

pub(super) type Parts = Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>;

pub(super) fn renderer() -> ModelClient {
    ModelClient::with_http_client(
        HttpTimeouts::default(),
        Arc::new(OutboundDispatch::builtin()),
        reqwest::Client::new(),
    )
}

pub(super) fn render_request(
    target: &ModelTarget,
    prompt: &Prompt,
    stream: bool,
) -> Result<serde_json::Value> {
    renderer()
        .render_request_with_report(target, prompt, stream)
        .map(|(body, _)| body)
}

struct Route(RoutingTarget);

#[async_trait::async_trait]
impl RoutingTable for Route {
    async fn route_chain(
        &self,
        _: &str,
        _: &RoutingPrefs,
        _: &CallerContext,
    ) -> crate::gateway::error::Result<Vec<RoutingTarget>> {
        Ok(vec![self.0.clone()])
    }

    fn list_models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.0.service_id.clone(),
            providers: vec![self.0.provider_name.clone()],
        }]
    }

    fn model_info(&self, _: &str) -> Option<ModelInfo> {
        self.list_models().into_iter().next()
    }
}

fn route(target: &ModelTarget) -> RoutingTarget {
    let chat = &target.compatibility.chat_completions;
    RoutingTarget {
        registry: None,
        provider_name: target.provider_name.clone(),
        service_id: target.service_id.clone(),
        api_base: target.api_base.clone(),
        api_key: target.api_key.clone(),
        api_protocol: target.api_protocol.clone(),
        chat_token_limit_field: chat.token_limit_field,
        chat_supports_store: chat.supports_store,
        chat_supports_stream_options: chat.supports_stream_options,
        chat_google_extensions: chat.google_extensions,
        reasoning_effort: None,
        model_constraints: Default::default(),
        account_label: target.account_label.clone(),
        api_key_override: (target.credential_priority == CredentialPriority::Explicit)
            .then(|| target.api_key.clone()),
        api_base_override: None,
        auth_scheme: target.auth_scheme,
        headers: Vec::new(),
    }
}

fn failure(error: impl std::fmt::Display) -> ModelError {
    ModelError::configuration(format!("gateway fixture: {error}"))
}

struct Serving {
    url: String,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<crate::gateway::error::Result<()>>>,
}

impl Serving {
    async fn finish(mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            tokio::time::timeout(std::time::Duration::from_secs(10), task)
                .await
                .map_err(failure)?
                .map_err(failure)?
                .map_err(failure)?;
        }
        Ok(())
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        // A dropped client still leaves accepted nonstream work to the real
        // HTTP server's graceful drain. Do not abort its settlement task.
        self.stop.cancel();
    }
}

#[derive(Clone, Default)]
struct RecordedUsage {
    entries: Arc<std::sync::Mutex<Vec<(u64, u64)>>>,
    changed: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::gateway::pipeline::settlement::SettlementRecorder for RecordedUsage {
    async fn record(
        &self,
        ctx: &mut crate::gateway::pipeline::settlement::SettlementContext,
    ) -> crate::gateway::error::Result<()> {
        self.entries
            .lock()
            .map_err(|_| {
                crate::gateway::error::BitrouterError::internal("fixture usage lock poisoned")
            })?
            .push((ctx.prompt_tokens, ctx.completion_tokens));
        self.changed.notify_one();
        Ok(())
    }
}

pub(super) struct HttpGateway {
    timeouts: HttpTimeouts,
    auth: AuthAppliers,
    client: reqwest::Client,
    usage: RecordedUsage,
}

impl HttpGateway {
    pub(super) fn new(timeouts: HttpTimeouts) -> Result<Self> {
        Ok(Self {
            timeouts,
            auth: AuthAppliers::new(),
            usage: RecordedUsage::default(),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .map_err(failure)?,
        })
    }

    pub(super) fn with_auth_appliers(mut self, auth: AuthAppliers) -> Self {
        self.auth = auth;
        self
    }

    pub(super) async fn wait_for_settlements(&self, count: usize) -> Result<Vec<(u64, u64)>> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let entries = self.usage.entries.lock().map_err(failure)?.clone();
                if entries.len() >= count {
                    return Ok(entries);
                }
                self.usage.changed.notified().await;
            }
        })
        .await
        .map_err(failure)?
    }

    async fn start(&self, target: &ModelTarget) -> Result<Serving> {
        let executor = HttpExecutor::with_dispatch_and_auth(
            self.timeouts.clone(),
            OutboundDispatch::builtin(),
            self.auth.clone(),
        )
        .map_err(failure)?;
        let app = App::builder()
            .skip_auth(true)
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(Route(route(target))))
                    .executor(Arc::new(executor))
                    .served_operations(OperationScope::Both)
                    .settlement_recorder_for(self.usage.clone(), OperationScope::Both);
            })
            .build()
            .map_err(failure)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(failure)?;
        let url = format!("http://{}", listener.local_addr().map_err(failure)?);
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            let result = app
                .serve_listener_with_router_wrapper_and_shutdown(
                    listener,
                    |router| router,
                    shutdown.cancelled_owned(),
                )
                .await;
            if let Err(error) = &result {
                tracing::error!(%error, "HTTP model fixture shutdown failed");
            }
            result
        });
        Ok(Serving {
            url,
            stop,
            task: Some(task),
        })
    }

    async fn post(
        &self,
        serving: &Serving,
        path: &str,
        body: serde_json::Value,
        cancellation: &CancellationToken,
    ) -> Result<reqwest::Response> {
        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ModelError::Cancelled),
            response = self.client.post(format!("{}{path}", serving.url)).json(&body).send() => response.map_err(failure)?,
        };
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let retry_after = parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
        let body = ModelClient::read_body(response, cancellation).await?;
        Err(ModelError::HttpResponse {
            status,
            body,
            retry_after,
        })
    }

    fn generation_request(
        target: &ModelTarget,
        prompt: &Prompt,
        stream: bool,
    ) -> Result<(&'static str, serde_json::Value)> {
        let path = match target.api_protocol {
            ApiProtocol::ChatCompletions => "/v1/chat/completions",
            ApiProtocol::Messages => "/v1/messages",
            ApiProtocol::Responses => "/v1/responses",
            _ => {
                return Err(ModelError::invalid_request(
                    "fixture requires a public generation endpoint",
                ));
            }
        };
        let dispatch = OutboundDispatch::builtin();
        let (adapter, _) = dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| failure("missing codec"))?;
        let mut prompt = prompt.clone();
        prompt.stream = stream;
        Ok((path, adapter.render_request(&prompt)?))
    }

    pub(super) async fn generate(
        &self,
        target: &ModelTarget,
        prompt: &Prompt,
        cancellation: &CancellationToken,
    ) -> Result<GenerateResult> {
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let (path, body) = Self::generation_request(target, prompt, false)?;
        let serving = self.start(target).await?;
        let response = self.post(&serving, path, body, cancellation).await?;
        let text = ModelClient::read_body(response, cancellation).await?;
        let dispatch = OutboundDispatch::builtin();
        let (adapter, _) = dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| failure("missing codec"))?;
        let result = ModelClient::parse_response(adapter.as_ref(), &target.api_protocol, &text);
        serving.finish().await?;
        result
    }

    pub(super) async fn decide(
        &self,
        target: &ModelTarget,
        request: &DecisionRequest,
        cancellation: &CancellationToken,
    ) -> Result<DecisionResult> {
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let serving = self.start(target).await?;
        let response = self
            .post(
                &serving,
                "/v1/decisions",
                DecisionsCodec::render_request(request)?,
                cancellation,
            )
            .await?;
        let text = ModelClient::read_body(response, cancellation).await?;
        let result = ModelClient::parse_decision_response(&text, request);
        serving.finish().await?;
        result
    }

    pub(super) async fn stream(
        &self,
        target: &ModelTarget,
        prompt: &Prompt,
        cancellation: &CancellationToken,
    ) -> Result<Parts> {
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let (path, body) = Self::generation_request(target, prompt, true)?;
        let serving = self.start(target).await?;
        let response = self.post(&serving, path, body, cancellation).await?;
        let dispatch = OutboundDispatch::builtin();
        let (adapter, _) = dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| failure("missing codec"))?;
        let mut decoder = adapter.stream_decoder();
        let cancellation = cancellation.clone();
        Ok(Box::pin(async_stream::try_stream! {
            use eventsource_stream::Eventsource;
            let mut events = response.bytes_stream().eventsource();
            loop {
                let event = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Err(ModelError::Cancelled),
                    event = events.next() => Ok(event),
                }?;
                let Some(event) = event else { break; };
                let event = event.map_err(failure)?;
                for part in decoder.decode(&SseEvent { event: (!event.event.is_empty()).then_some(event.event), data: event.data })? {
                    yield part;
                }
            }
            for part in decoder.finish()? { yield part; }
            serving.finish().await?;
        }))
    }
}
