//! Actual tenant/provider schedules for the per-turn routing handoff. Execution
//! and pipeline owners stay alive through cleanup and competing admissions.
use super::*;
use crate::{
    gateway::{
        caller::CallerContext,
        pipeline::{
            execution::Pipeline,
            executor::StreamPartStream,
            routing::{RoutingPrefs, RoutingTable},
            server_tools::{approval::ApprovalPolicy, classify::RouterCall},
        },
    },
    tenancy::{
        command::Command,
        execution::TenantExecution,
        routing::state::{self, custody as observe},
    },
};
use bitrouter_ai::{
    tenancy::{execution::ReceiptState, operation::Mutation},
    types::StreamPart,
};
use tokio::sync::oneshot;

struct ApprovalGate {
    arrived: Semaphore,
    proceed: Semaphore,
}
impl Default for ApprovalGate {
    fn default() -> Self {
        Self {
            arrived: Semaphore::new(0),
            proceed: Semaphore::new(0),
        }
    }
}
#[async_trait]
impl ApprovalPolicy for ApprovalGate {
    async fn allow(&self, _: &RouterCall, _: &CallerContext) -> bool {
        self.arrived.add_permits(1);
        match self.proceed.acquire().await {
            Ok(permit) => {
                permit.forget();
                true
            }
            Err(_) => false,
        }
    }
}
impl ApprovalGate {
    async fn reached(&self) -> Result<()> {
        tokio::time::timeout(TIMEOUT, self.arrived.acquire())
            .await??
            .forget();
        Ok(())
    }
}

enum Reply {
    Stream {
        prefix: String,
        tail: oneshot::Receiver<Vec<u8>>,
    },
    Unavailable,
}
struct Campaign {
    fixture: Fixture,
    revision: ConfigurationRevision,
    protocol: ApiProtocol,
    queue: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    _provider: AbortOnDropHandle<std::io::Result<()>>,
}
impl Campaign {
    async fn new(protocol: ApiProtocol, routing_capacity: usize) -> Result<Self> {
        let (name, path) = match protocol {
            ApiProtocol::ChatCompletions => ("chat_completions", "/v1/chat/completions"),
            ApiProtocol::Responses => ("responses", "/v1/responses"),
            ApiProtocol::Messages => ("messages", "/v1/messages"),
            _ => anyhow::bail!("compatible protocol"),
        };
        let queue = Arc::new(Mutex::new(VecDeque::<Reply>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let script = queue.clone();
        let received = requests.clone();
        let router = Router::new().route(path,post(move |headers: HeaderMap,Json(request): Json<Value>| {
            let authenticated = headers.get("authorization").is_some_and(|v| v == "Bearer owned-upstream-secret")
                || headers.get("x-api-key").is_some_and(|v| v == "owned-upstream-secret");
            let recorded = received.lock().map(|mut values| values.push(request)).is_ok();
            let reply = script.lock().ok().and_then(|mut queue| queue.pop_front());
            async move {
                if !authenticated || !recorded { return StatusCode::UNAUTHORIZED.into_response(); }
                match reply {
                    Some(Reply::Stream {prefix,tail}) => {
                        let body = axum::body::Body::from_stream(async_stream::stream! {
                            yield Ok::<_,std::convert::Infallible>(axum::body::Bytes::from(prefix));
                            if let Ok(tail) = tail.await {
                                yield Ok(axum::body::Bytes::from(tail));
                            }
                        });
                        ([("content-type","text/event-stream")],body).into_response()
                    }
                    Some(Reply::Unavailable) => (StatusCode::SERVICE_UNAVAILABLE,Json(json!({"error":{"type":"api_error","message":"fixture unavailable"}}))).into_response(),
                    None => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let (mut fixture, route, _) =
            provision_protocol(&format!("http://{}/v1", listener.local_addr()?), name).await?;
        // Independent routing capacity is a supported Runtime budget. Leave
        // logical admission at four, so contenders reach the actual send gate.
        fixture.service.routing = Arc::new(state::Runtime::configured(
            &crate::config::tenancy::capacity::Routing::default(),
            routing_capacity,
        )?);
        let revision = draft(&fixture, &route, "custody-config", 25).await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let provider =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        Ok(Self {
            fixture,
            revision,
            protocol,
            queue,
            requests,
            _provider: provider,
        })
    }
    fn hold(&self, prefix: String) -> Result<oneshot::Sender<Vec<u8>>> {
        let (send, tail) = oneshot::channel();
        self.queue
            .lock()
            .map_err(|_| anyhow::anyhow!("provider script poisoned"))?
            .push_back(Reply::Stream { prefix, tail });
        Ok(send)
    }
    fn unavailable(&self) -> Result<()> {
        self.queue
            .lock()
            .map_err(|_| anyhow::anyhow!("provider script poisoned"))?
            .push_back(Reply::Unavailable);
        Ok(())
    }
    fn sends(&self) -> Result<usize> {
        Ok(self
            .requests
            .lock()
            .map_err(|_| anyhow::anyhow!("provider requests poisoned"))?
            .len())
    }
    async fn attempt(&self, label: &str, approval: Arc<dyn ApprovalPolicy>) -> Result<Attempt> {
        let credential = self.fixture.issue(label).await?;
        let headers = headers(
            &self.fixture,
            credential.secret.as_deref().context("caller credential")?,
            &self.revision.revision_id,
        )?;
        let submission = self
            .fixture
            .service
            .submission(&headers)
            .await
            .map_err(failure)?;
        let execution = self
            .fixture
            .service
            .admit_execution(submission, MODEL, &self.protocol)
            .await
            .map_err(failure)?;
        let target = execution
            .route()
            .route_chain(MODEL, &RoutingPrefs::default(), &execution.caller())
            .await?
            .into_iter()
            .next()
            .context("retained target")?;
        let key = observe::key(&execution, &target)?;
        let calls = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut pipeline = execution.pipeline()?;
        let unique = Arc::get_mut(&mut pipeline).context("fresh pipeline unexpectedly shared")?;
        unique.server_tool_loop = Some(Arc::new(ServerToolLoop::new(
            ToolsetRegistry::new(vec![Arc::new(Search(calls.clone()))]),
            ServerToolLoopConfig::default(),
            approval,
        )));
        unique.settlement_recorders.push(HookRegistration::new(
            Arc::new(Recorder(recorded.clone())),
            OperationScope::Generation,
            TypeId::of::<Recorder>(),
        ));
        let body = match self.protocol {
            ApiProtocol::Responses => json!({"model":MODEL,"input":"hello","stream":true}),
            _ => {
                json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25,"stream":true})
            }
        };
        let PipelineInput::Generation(prompt) =
            crate::gateway::http::parse_input(&self.protocol, body)?
        else {
            anyhow::bail!("generation input")
        };
        let mut request = PipelineRequest::new(MODEL, execution.caller(), *prompt);
        request.request_id = execution.id().to_string();
        request.inbound_protocol = Some(self.protocol.clone());
        request.headers = headers;
        execution.attach(pipeline.clone(), &mut request)?;
        Ok(Attempt {
            execution,
            pipeline,
            replay: Some(request),
            stream: None,
            credential,
            key,
            calls,
            recorded,
            terminals: 0,
            errors: 0,
        })
    }
    fn snapshot(&self, attempt: &Attempt) -> Result<observe::Snapshot> {
        observe::snapshot(&self.fixture.service.routing, &attempt.key)
            .map_err(|error| anyhow::anyhow!("routing snapshot: {error:?}"))?
            .context("actual routing key absent")
    }
    async fn facts(&self, attempt: &Attempt) -> Result<(Vec<u8>, Vec<u8>)> {
        let service = &self.fixture.service;
        let receipt = service
            .read_receipt(
                self.fixture.operator.clone(),
                self.fixture.tenant.clone(),
                read::Request::One(attempt.execution.id().clone()),
            )
            .await
            .map_err(failure)?;
        let evidence = service
            .read_receipt(
                self.fixture.operator.clone(),
                self.fixture.tenant.clone(),
                read::Request::UsageEvidence(attempt.execution.id().clone()),
            )
            .await
            .map_err(failure)?;
        Ok((receipt, evidence))
    }
    async fn closed(
        &self,
        attempt: &mut Attempt,
        input: u64,
        output: u64,
        outcome: Outcome,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        drop(attempt.stream.take());
        // A copied request owns an external lifecycle ticket. Retain the
        // execution/pipeline and replay metadata, not a test-owned producer.
        if let Some(request) = attempt.replay.as_mut() {
            request
                .extensions
                .remove::<Arc<crate::tenancy::execution::lifecycle::Registration>>();
        }
        wait_until(|| {
            Ok(attempt.execution.receipt_owner()?.durable()
                && attempt
                    .recorded
                    .lock()
                    .map_err(|_| anyhow::anyhow!("settlement poisoned"))?
                    .len()
                    == 1)
        })
        .await
        .with_context(|| {
            format!(
                "receipt/settlement cleanup for {}: pending {:?}, durable {}, records {}",
                attempt.execution.id().as_str(),
                attempt.pipeline.owned_tasks().map(|tasks| tasks.pending()),
                attempt
                    .execution
                    .receipt_owner()
                    .map(|owner| owner.durable())
                    .unwrap_or(false),
                attempt
                    .recorded
                    .lock()
                    .map(|records| records.len())
                    .unwrap_or(usize::MAX)
            )
        })?;
        let facts = self.facts(attempt).await?;
        let receipt: ExecutionReceipt = serde_json::from_slice(&facts.0)?;
        let evidence: ExecutionUsageEvidenceV1 = serde_json::from_slice(&facts.1)?;
        ensure!(
            receipt.execution_id == *attempt.execution.id()
                && receipt.principal_id == self.fixture.principal
        );
        ensure!(
            receipt.attempts.len() == 1 && receipt.attempts[0].outcome == outcome,
            "receipt {receipt:?}"
        );
        ensure!(
            receipt.usage.input_tokens == Some(input)
                && receipt.usage.output_tokens == Some(output),
            "receipt {receipt:?}"
        );
        ensure!(receipt.usage.provenance == Provenance::Provider && !receipt.usage.complete);
        if outcome == Outcome::Unknown {
            ensure!(
                receipt.state == ReceiptState::Unknown && receipt.outcome != Outcome::Completed
            );
        }
        ensure!(
            evidence.attempts.len() == 1
                && evidence.frames.len()
                    == if self.protocol == ApiProtocol::Messages {
                        2
                    } else {
                        1
                    }
        );
        ensure!(!evidence.truncated && evidence.dropped_frames == 0);
        ensure!(
            evidence.attempts[0].attempt_id == receipt.attempts[0].attempt_id
                && evidence.attempts[0].outcome == outcome
        );
        ensure!(evidence.attempts[0].terminal_observed == (outcome == Outcome::Completed));
        ensure!(
            evidence
                .frames
                .iter()
                .all(|frame| frame.attempt_id == receipt.attempts[0].attempt_id && !frame.invalid)
        );
        let recorded = attempt
            .recorded
            .lock()
            .map_err(|_| anyhow::anyhow!("settlement poisoned"))?;
        ensure!(
            recorded.len() == 1
                && recorded[0].input == input
                && recorded[0].output == output
                && recorded[0].origin == UsageOrigin::ProviderReported,
            "settlement {recorded:?}"
        );
        ensure!(attempt.terminals == 0, "successful caller terminal escaped");
        Ok(facts)
    }
    async fn quiescent(&self, attempt: &Attempt) -> Result<()> {
        wait_until(|| {
            let pending = attempt
                .pipeline
                .owned_tasks()
                .context("owned tasks absent")?
                .pending();
            Ok(pending.sources == 0 && pending.executions == 0 && pending.settlements == 0)
        })
        .await
        .with_context(|| {
            format!(
                "owned cleanup for {}: pending {:?}, durable {}",
                attempt.execution.id().as_str(),
                attempt.pipeline.owned_tasks().map(|tasks| tasks.pending()),
                attempt
                    .execution
                    .receipt_owner()
                    .map(|owner| owner.durable())
                    .unwrap_or(false)
            )
        })
    }
    async fn stop(&self) -> Result<()> {
        tokio::time::timeout(TIMEOUT, self.fixture.service.admission.wait_drained()).await?;
        tokio::time::timeout(TIMEOUT, self.fixture.service.shutdown()).await??;
        Ok(())
    }
}
struct Attempt {
    execution: Arc<TenantExecution>,
    pipeline: Arc<Pipeline>,
    replay: Option<PipelineRequest>,
    stream: Option<StreamPartStream>,
    credential: CredentialIssued,
    key: state::Key,
    calls: Arc<AtomicUsize>,
    recorded: Arc<Mutex<Vec<Settlement>>>,
    terminals: usize,
    errors: usize,
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if self.stream.is_some() {
            match self.execution.receipt_owner() {
                Ok(owner) => owner.cancel(),
                Err(error) => eprintln!("test owner cancellation unavailable: {error}"),
            }
        }
    }
}
impl Attempt {
    async fn open(&mut self) -> Result<bool> {
        let request = self.replay.as_ref().context("request absent")?.clone();
        match tokio::time::timeout(
            TIMEOUT,
            Box::pin(self.pipeline.clone().execute_stream(request)),
        )
        .await
        .context("stream preparation stalled")?
        {
            Ok(stream) => {
                self.stream = Some(stream);
                Ok(true)
            }
            Err(_) => {
                self.errors += 1;
                Ok(false)
            }
        }
    }
    fn observe(&mut self, part: Option<crate::gateway::error::Result<StreamPart>>) -> bool {
        match part {
            Some(Ok(part)) => {
                self.terminals += usize::from(part.is_terminal());
                true
            }
            Some(Err(_)) => {
                self.errors += 1;
                true
            }
            None => false,
        }
    }
    async fn received(&mut self, input: u64, output: u64) -> Result<()> {
        tokio::time::timeout(TIMEOUT,async {
            loop {
                if self.execution.journal().map_err(|code| anyhow::anyhow!("journal {code:?}"))?
                    .received_usage_for_test(input,output)? {break Ok(());}
                let stream=self.stream.as_mut().context("stream absent")?;
                tokio::select! {
                    part=stream.next()=>ensure!(self.observe(part),"stream ended before held source usage"),
                    ()=tokio::time::sleep(Duration::from_millis(2))=>{}
                }
            }
        }).await?
    }
    async fn approval(&mut self, gate: &ApprovalGate) -> Result<()> {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let stream = self.stream.as_mut().context("stream absent")?;
                tokio::select! {
                    result=gate.reached()=>{result?;break Ok(());}
                    part=stream.next()=>ensure!(self.observe(part),"stream ended before approval"),
                }
            }
        })
        .await?
    }
    async fn drain(&mut self) -> Result<()> {
        while let Some(stream) = self.stream.as_mut() {
            let part = tokio::time::timeout(TIMEOUT, stream.next()).await?;
            if !self.observe(part) {
                break;
            }
        }
        Ok(())
    }
    async fn replay_refused(&mut self) -> Result<()> {
        let mut request = self
            .replay
            .take()
            .context("replay request already consumed")?;
        if request
            .extensions
            .get::<Arc<crate::tenancy::execution::lifecycle::Registration>>()
            .is_none()
            && self
                .execution
                .attach(self.pipeline.clone(), &mut request)
                .is_err()
        {
            return Ok(());
        }
        match tokio::time::timeout(TIMEOUT, self.pipeline.clone().execute_stream(request)).await? {
            Err(_) => {}
            Ok(mut stream) => {
                let mut errors = 0;
                while let Some(part) = tokio::time::timeout(TIMEOUT, stream.next()).await? {
                    match part {
                        Err(_) => errors += 1,
                        Ok(part) => ensure!(!part.is_terminal(), "replayed success"),
                    }
                }
                ensure!(errors > 0, "replay was not refused");
            }
        }
        Ok(())
    }
}
async fn wait_until(mut condition: impl FnMut() -> Result<bool>) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if condition()? {
                break Ok(());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await?
}
fn ended(tail: oneshot::Sender<Vec<u8>>, data: String) -> Result<()> {
    tail.send(data.into_bytes())
        .map_err(|_| anyhow::anyhow!("provider body disappeared before requested tail"))
}
fn messages_prefix(id: &str, input: u64, output: u64, tool: bool) -> String {
    let mut body = message(
        "message_start",
        json!({"message":{"id":id,"usage":{"input_tokens":input,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
    );
    if tool {
        body += &message(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"tool_use","id":"c1","name":"search","input":{}}}),
        );
        body += &message(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        );
    } else {
        body += &message(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"text","text":""}}),
        );
        body += &message(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta","text":"ok"}}),
        );
    }
    body += &message("content_block_stop", json!({"index":0}));
    body += &message(
        "message_delta",
        json!({"delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":output}}),
    );
    body + &message("message_stop", json!({}))
}
fn invalid_tail(protocol: &ApiProtocol) -> Vec<u8> {
    match protocol {
        ApiProtocol::Messages=>message("content_block_delta",json!({"index":0,"delta":{"type":"input_json_delta","partial_json":" "}})).into_bytes(),
        ApiProtocol::Responses=>frame(json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"late","call_id":"late","name":"search","arguments":""}})).into_bytes(),
        _=>b"data: \xff\n\n".to_vec(),
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Drop,
    Cancel,
    InvalidTail,
}
async fn before_eof(protocol: ApiProtocol, fault: Fault) -> Result<()> {
    let campaign = Campaign::new(protocol.clone(), 1).await?;
    let checked: Result<()> = async {
        let tail = campaign.hold(turns(&protocol)?.0)?;
        let mut a = campaign.attempt("held-a", Arc::new(AllowAll)).await?;
        ensure!(a.open().await?);
        a.received(10, 3).await.context("held source usage")?;
        let held = campaign.snapshot(&a)?;
        ensure!(
            held.active == 1
                && held.target_active == 1
                && held.pending == 0
                && held.sent == 1
                && held.samples.is_empty()
                && held.failures == 0
        );
        // A different real execution reaches the send gate, cannot borrow A's slot,
        // and its own failed cleanup must not release the live reservation.
        let mut refused = campaign
            .attempt("held-contender", Arc::new(AllowAll))
            .await?;
        ensure!(!refused.open().await?);
        drop(refused.replay.take());
        campaign
            .quiescent(&refused)
            .await
            .context("refused contender cleanup")?;
        ensure!(
            campaign.snapshot(&a)? == held
                && campaign.sends()? == 1
                && refused.calls.load(Ordering::SeqCst) == 0
        );
        let refusal: ExecutionReceipt = serde_json::from_slice(&campaign.facts(&refused).await?.0)?;
        ensure!(refusal.attempts.is_empty());
        let late_tail = match fault {
            Fault::Drop => {
                drop(a.stream.take());
                Some(tail)
            }
            Fault::Cancel => {
                a.execution.receipt_owner()?.cancel();
                a.drain().await?;
                Some(tail)
            }
            Fault::InvalidTail => {
                tail.send(invalid_tail(&protocol))
                    .map_err(|_| anyhow::anyhow!("provider disappeared before invalid tail"))?;
                a.drain().await?;
                None
            }
        };
        let facts = campaign
            .closed(&mut a, 10, 3, Outcome::Unknown)
            .await
            .context("held original closure")?;
        ensure!(a.calls.load(Ordering::SeqCst) == 0 && campaign.sends()? == 1);
        let cleaned = campaign.snapshot(&a)?;
        ensure!(
            cleaned.active == 0
                && cleaned.target_active == 0
                && cleaned.pending == 0
                && cleaned.sent == 1
                && cleaned.samples.is_empty()
                && cleaned.failures == 0
        );
        // A copy of the consumed input is not another send/settlement permit.
        a.replay_refused().await?;
        campaign.quiescent(&a).await?;
        ensure!(
            campaign.facts(&a).await? == facts
                && campaign.snapshot(&a)? == cleaned
                && campaign.sends()? == 1
        );
        if let Some(tail) = late_tail {
            // The peer may already have observed transport withdrawal. Otherwise
            // deliver late EOF only after the producer and receipt have drained.
            if tail.send(Vec::new()).is_ok() {
                tokio::task::yield_now().await;
            }
            ensure!(campaign.facts(&a).await? == facts && campaign.snapshot(&a)? == cleaned);
        }
        recover(&campaign, &a, 20, 4, 2).await?;
        campaign.stop().await?;
        Ok(())
    }
    .await;
    if let Err(error) = &checked {
        eprintln!("custody schedule failure before runtime teardown: {error:#}");
    }
    let stopped = tokio::time::timeout(TIMEOUT, campaign.fixture.service.shutdown()).await;
    checked?;
    stopped??;
    Ok(())
}

#[tokio::test]
async fn real_tenant_drop_before_eof_retains_usage_and_releases_only_its_capacity() -> Result<()> {
    for protocol in [
        ApiProtocol::Messages,
        ApiProtocol::ChatCompletions,
        ApiProtocol::Responses,
    ] {
        before_eof(protocol, Fault::Drop).await?;
    }
    Ok(())
}
#[tokio::test]
async fn real_tenant_cancel_before_eof_retains_usage_and_releases_only_its_capacity() -> Result<()>
{
    for protocol in [
        ApiProtocol::Messages,
        ApiProtocol::ChatCompletions,
        ApiProtocol::Responses,
    ] {
        before_eof(protocol, Fault::Cancel).await?;
    }
    Ok(())
}
#[tokio::test]
async fn real_tenant_invalid_tail_cannot_complete_the_turn_or_heal_routing() -> Result<()> {
    for protocol in [
        ApiProtocol::Messages,
        ApiProtocol::ChatCompletions,
        ApiProtocol::Responses,
    ] {
        before_eof(protocol, Fault::InvalidTail).await?;
    }
    Ok(())
}

fn plain_prefix(protocol: &ApiProtocol, input: u64, output: u64) -> String {
    match protocol {
        ApiProtocol::Messages => messages_prefix("recovery", input, output, false),
        ApiProtocol::Responses => frame(
            json!({"type":"response.completed","response":{"id":"recovery","status":"completed","output":[],"usage":{"input_tokens":input,"output_tokens":output,"total_tokens":input+output}}}),
        ),
        _ => {
            frame(json!({"id":"recovery","choices":[{"index":0,"delta":{"content":"ok"}}]}))
                + &frame(
                    json!({"id":"recovery","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":input,"completion_tokens":output,"total_tokens":input+output}}),
                )
                + "data: [DONE]\n\n"
        }
    }
}
async fn recover(
    campaign: &Campaign,
    previous: &Attempt,
    input: u64,
    output: u64,
    sent: u64,
) -> Result<()> {
    let before = campaign.snapshot(previous)?;
    let tail = campaign.hold(plain_prefix(&campaign.protocol, input, output))?;
    ended(tail, String::new())?;
    let mut recovery = campaign
        .attempt("fresh-recovery", Arc::new(AllowAll))
        .await?;
    ensure!(
        recovery.open().await?,
        "cleanup left routing or logical admission closed"
    );
    recovery.drain().await?;
    drop(recovery.stream.take());
    drop(recovery.replay.take());
    campaign.quiescent(&recovery).await?;
    let receipt: ExecutionReceipt = serde_json::from_slice(&campaign.facts(&recovery).await?.0)?;
    ensure!(
        recovery.terminals == 1
            && recovery.errors == 0
            && recovery.calls.load(Ordering::SeqCst) == 0
    );
    ensure!(
        receipt.outcome == Outcome::Completed
            && receipt.attempts.len() == 1
            && receipt.attempts[0].outcome == Outcome::Completed
    );
    ensure!(
        receipt.usage.input_tokens == Some(input) && receipt.usage.output_tokens == Some(output)
    );
    let after = campaign.snapshot(&recovery)?;
    ensure!(
        after.active == 0
            && after.target_active == 0
            && after.pending == 0
            && after.sent == sent
            && campaign.sends()? == sent as usize
    );
    ensure!(
        after.samples.len() == before.samples.len() + 1
            && after.samples.last() == Some(&Some(output))
            && after.failures == 0
            && !after.open
            && after.probe.is_none()
    );
    Ok(())
}
async fn refuse_contender(campaign: &Campaign, current: &Attempt, label: &str) -> Result<()> {
    let before = campaign.snapshot(current)?;
    let mut contender = campaign.attempt(label, Arc::new(AllowAll)).await?;
    ensure!(
        !contender.open().await?,
        "another owner borrowed live capacity/probe"
    );
    drop(contender.replay.take());
    campaign.quiescent(&contender).await?;
    let facts: ExecutionReceipt = serde_json::from_slice(&campaign.facts(&contender).await?.0)?;
    ensure!(facts.attempts.is_empty() && contender.calls.load(Ordering::SeqCst) == 0);
    ensure!(campaign.snapshot(current)? == before && campaign.sends()? == before.sent as usize);
    Ok(())
}
async fn rebind_refused(campaign: &Campaign, original: &Attempt, current: &Attempt) -> Result<()> {
    let before = campaign.snapshot(current)?;
    let mut different = campaign
        .attempt("rebound-owner", Arc::new(AllowAll))
        .await?;
    // Copy the original registration/pipeline but substitute an independently
    // admitted live execution. Same tenant, route and upstream credential do
    // not transfer the original preparation/custody permit.
    let mut request = original
        .replay
        .as_ref()
        .context("original request")?
        .clone();
    request.request_id = different.execution.id().to_string();
    request.caller = different.execution.caller();
    request.extensions.insert(different.execution.clone());
    ensure!(
        tokio::time::timeout(
            TIMEOUT,
            Box::pin(original.pipeline.clone().execute_stream(request))
        )
        .await
        .context("rebound input preparation stalled")?
        .is_err()
    );
    ensure!(
        !different.open().await?,
        "capacity contender unexpectedly dispatched"
    );
    drop(different.replay.take());
    campaign.quiescent(&different).await?;
    ensure!(
        different
            .recorded
            .lock()
            .map_err(|_| anyhow::anyhow!("settlement lock"))?
            .len()
            == 1,
        "rebinding consumed the replacement's own preparation permit"
    );
    let facts: ExecutionReceipt = serde_json::from_slice(&campaign.facts(&different).await?.0)?;
    ensure!(facts.attempts.is_empty() && facts.execution_id == *different.execution.id());
    ensure!(
        !original.execution.receipt_owner()?.durable(),
        "rebound input closed the original owner"
    );
    ensure!(
        original.execution.failure().is_none(),
        "rebound input changed the original owner's refusal evidence"
    );
    ensure!(campaign.snapshot(current)? == before && campaign.sends()? == before.sent as usize);
    Ok(())
}
async fn live_copy_refused(original: &Attempt) -> Result<()> {
    for stream in [true, false] {
        let request = original
            .replay
            .as_ref()
            .context("original request")?
            .clone();
        let refused = if stream {
            tokio::time::timeout(
                TIMEOUT,
                Box::pin(original.pipeline.clone().execute_stream(request)),
            )
            .await
            .context("copied stream preparation stalled")?
            .is_err()
        } else {
            tokio::time::timeout(
                TIMEOUT,
                Box::pin(original.pipeline.clone().execute_detached(request)),
            )
            .await
            .context("copied JSON preparation stalled")?
            .is_err()
        };
        ensure!(refused, "copied live permit was accepted");
        ensure!(
            !original.execution.receipt_owner()?.durable(),
            "copied input finalized the current logical owner"
        );
        ensure!(
            original.execution.failure().is_none(),
            "copied input changed current refusal evidence"
        );
        ensure!(
            original
                .recorded
                .lock()
                .map_err(|_| anyhow::anyhow!("settlement lock"))?
                .is_empty(),
            "copied input caused another settlement"
        );
    }
    Ok(())
}

async fn after_handoff(fault: Fault) -> Result<()> {
    let campaign = Campaign::new(ApiProtocol::Messages, 1).await?;
    let checked: Result<()> = async {
        let tail = campaign.hold(messages_prefix("original", 10, 3, true))?;
        let gate = Arc::new(ApprovalGate::default());
        let mut a = campaign.attempt("handed-off-a", gate.clone()).await?;
        ensure!(a.open().await?);
        ended(tail, String::new())?;
        a.approval(&gate).await?;
        let handed = campaign.snapshot(&a)?;
        ensure!(
            handed.active == 0
                && handed.target_active == 0
                && handed.sent == 1
                && handed.samples == [Some(3)]
                && a.calls.load(Ordering::SeqCst) == 0
        );
        let b_tail = campaign.hold(messages_prefix("current", 20, 4, true))?;
        let mut b = campaign.attempt("current-b", Arc::new(AllowAll)).await?;
        ensure!(b.open().await?);
        b.received(20, 4).await?;
        let current = campaign.snapshot(&b)?;
        ensure!(
            current.active == 1
                && current.target_active == 1
                && current.sent == 2
                && current.samples == [Some(3)]
                && current.last_admission > handed.last_admission
        );
        refuse_contender(&campaign, &b, "current-contender").await?;
        rebind_refused(&campaign, &a, &b).await?;
        live_copy_refused(&a).await?;
        match fault {
            Fault::Cancel => {
                a.execution.receipt_owner()?.cancel();
                a.drain().await?;
            }
            Fault::Drop => drop(a.stream.take()),
            Fault::InvalidTail => anyhow::bail!("invalid tail belongs before EOF"),
        }
        let a_facts = campaign.closed(&mut a, 10, 3, Outcome::Completed).await?;
        ensure!(
            campaign.snapshot(&b)? == current
                && campaign.sends()? == 2
                && a.calls.load(Ordering::SeqCst) == 0
        );
        a.replay_refused().await?;
        campaign.quiescent(&a).await?;
        ensure!(campaign.snapshot(&b)? == current && campaign.facts(&a).await? == a_facts);
        refuse_contender(&campaign, &b, "late-cleanup-contender").await?;
        drop(b.stream.take());
        let b_facts = campaign.closed(&mut b, 20, 4, Outcome::Unknown).await?;
        b.replay_refused().await?;
        campaign.quiescent(&b).await?;
        let cleaned = campaign.snapshot(&b)?;
        ensure!(
            cleaned.active == 0
                && cleaned.target_active == 0
                && cleaned.sent == 2
                && cleaned.samples == [Some(3)]
                && cleaned.failures == 0
        );
        if b_tail.send(Vec::new()).is_ok() {
            tokio::task::yield_now().await;
        }
        ensure!(campaign.facts(&b).await? == b_facts && campaign.snapshot(&b)? == cleaned);
        recover(&campaign, &b, 40, 7, 3).await?;
        ensure!(campaign.facts(&a).await? == a_facts && campaign.facts(&b).await? == b_facts);
        campaign.stop().await?;
        Ok(())
    }
    .await;
    if let Err(error) = &checked {
        eprintln!("custody schedule failure before runtime teardown: {error:#}");
    }
    let stopped = tokio::time::timeout(TIMEOUT, campaign.fixture.service.shutdown()).await;
    checked?;
    stopped??;
    Ok(())
}
#[tokio::test]
async fn real_tenant_drop_after_handoff_cannot_release_another_current_permit() -> Result<()> {
    after_handoff(Fault::Drop).await
}
#[tokio::test]
async fn real_tenant_cancel_after_handoff_cannot_release_another_current_permit() -> Result<()> {
    after_handoff(Fault::Cancel).await
}

#[tokio::test]
async fn real_tenant_revoke_after_handoff_denies_next_send_and_keeps_lineage() -> Result<()> {
    let campaign = Campaign::new(ApiProtocol::Messages, 1).await?;
    let tail = campaign.hold(messages_prefix("original", 10, 3, true))?;
    let gate = Arc::new(ApprovalGate::default());
    let mut a = campaign
        .attempt("revoke-between-turns", gate.clone())
        .await?;
    ensure!(a.open().await?);
    ended(tail, String::new())?;
    a.approval(&gate).await?;
    ensure!(campaign.snapshot(&a)?.samples == [Some(3)] && campaign.snapshot(&a)?.active == 0);
    let b_tail = campaign.hold(messages_prefix("unrelated-current", 20, 4, true))?;
    let mut b = campaign
        .attempt("unrevoked-current", Arc::new(AllowAll))
        .await?;
    ensure!(b.open().await?);
    b.received(20, 4).await?;
    let current = campaign.snapshot(&b)?;
    campaign
        .fixture
        .service
        .execute(
            campaign.fixture.operator.clone(),
            Command::Revoke(
                campaign.fixture.tenant.clone(),
                a.credential.credential_id.clone(),
                Mutation {
                    operation_id: "revoke-at-handoff".try_into()?,
                    expected_revision: a.credential.credential_revision.clone(),
                    installation_id: campaign.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    ensure!(a.execution.revalidate().is_err() && b.execution.revalidate().is_ok());
    gate.proceed.add_permits(1);
    a.drain().await?;
    let a_facts = campaign.closed(&mut a, 10, 3, Outcome::Completed).await?;
    ensure!(
        a.execution.failure().is_some_and(|failure| {
            failure.0.code == bitrouter_ai::tenancy::operation::ErrorCode::Unauthorized
        }),
        "next send was not rejected by current caller authority"
    );
    ensure!(a.errors == 1 && a.calls.load(Ordering::SeqCst) == 1 && campaign.sends()? == 2);
    ensure!(
        campaign.snapshot(&b)? == current,
        "revoked owner released the current owner's reservation"
    );
    a.replay_refused().await?;
    campaign.quiescent(&a).await?;
    ensure!(campaign.facts(&a).await? == a_facts && campaign.snapshot(&b)? == current);
    refuse_contender(&campaign, &b, "post-revoke-contender").await?;
    drop(b.stream.take());
    let b_facts = campaign.closed(&mut b, 20, 4, Outcome::Unknown).await?;
    drop(b.replay.take());
    campaign.quiescent(&b).await?;
    if b_tail.send(Vec::new()).is_ok() {
        tokio::task::yield_now().await;
    }
    recover(&campaign, &b, 40, 7, 3).await?;
    ensure!(campaign.facts(&a).await? == a_facts && campaign.facts(&b).await? == b_facts);
    campaign.stop().await?;
    Ok(())
}

#[tokio::test]
async fn real_tenant_late_cleanup_preserves_the_current_half_open_probe() -> Result<()> {
    let campaign = Campaign::new(ApiProtocol::Messages, 2).await?;
    let tail = campaign.hold(messages_prefix("old-owner", 10, 3, true))?;
    let gate = Arc::new(ApprovalGate::default());
    let mut a = campaign.attempt("old-probe-owner", gate.clone()).await?;
    ensure!(a.open().await?);
    ended(tail, String::new())?;
    a.approval(&gate).await?;
    for index in 0..3 {
        campaign.unavailable()?;
        let mut failed = campaign
            .attempt(&format!("real-unavailable-{index}"), Arc::new(AllowAll))
            .await?;
        ensure!(!failed.open().await?);
        drop(failed.replay.take());
        campaign.quiescent(&failed).await?;
    }
    let opened = campaign.snapshot(&a)?;
    ensure!(
        opened.failures == 3
            && opened.open
            && opened.probe.is_none()
            && opened.sent == 4
            && opened.samples == [Some(3)]
    );
    observe::elapse_open_period(&campaign.fixture.service.routing, &a.key)
        .map_err(|error| anyhow::anyhow!("open interval: {error:?}"))?;
    let b_tail = campaign.hold(messages_prefix("current-probe", 20, 4, true))?;
    let mut b = campaign
        .attempt("current-probe", Arc::new(AllowAll))
        .await?;
    ensure!(b.open().await?);
    b.received(20, 4).await?;
    let probe = campaign.snapshot(&b)?;
    ensure!(
        probe.probe == Some(probe.last_admission)
            && probe.active == 1
            && probe.target_active == 1
            && probe.sent == 5
            && probe.generation == opened.generation
    );
    refuse_contender(&campaign, &b, "probe-contender").await?;
    rebind_refused(&campaign, &a, &b).await?;
    drop(a.stream.take());
    let a_facts = campaign.closed(&mut a, 10, 3, Outcome::Completed).await?;
    a.replay_refused().await?;
    campaign.quiescent(&a).await?;
    ensure!(
        campaign.snapshot(&b)? == probe,
        "old completion released/healed another execution's probe"
    );
    refuse_contender(&campaign, &b, "post-cleanup-probe-contender").await?;
    b.execution.receipt_owner()?.cancel();
    b.drain().await?;
    let b_facts = campaign.closed(&mut b, 20, 4, Outcome::Unknown).await?;
    b.replay_refused().await?;
    campaign.quiescent(&b).await?;
    let withdrawn = campaign.snapshot(&b)?;
    ensure!(
        withdrawn.active == 0
            && withdrawn.target_active == 0
            && withdrawn.pending == 0
            && withdrawn.sent == 5
            && withdrawn.probe.is_none()
            && withdrawn.open
            && withdrawn.failures == 3
            && withdrawn.samples == [Some(3)]
            && withdrawn.generation == probe.generation
    );
    if b_tail.send(Vec::new()).is_ok() {
        tokio::task::yield_now().await;
    }
    refuse_contender(&campaign, &b, "reopened-probe-cannot-retry").await?;
    observe::elapse_open_period(&campaign.fixture.service.routing, &b.key)
        .map_err(|error| anyhow::anyhow!("open interval: {error:?}"))?;
    recover(&campaign, &b, 40, 7, 6).await?;
    ensure!(campaign.facts(&a).await? == a_facts && campaign.facts(&b).await? == b_facts);
    campaign.stop().await?;
    Ok(())
}

#[tokio::test]
async fn real_tenant_concurrent_copies_have_one_preparation_and_one_settlement() -> Result<()> {
    let campaign = Campaign::new(ApiProtocol::Messages, 1).await?;
    let checked: Result<()> = async {
        let tail = campaign.hold(messages_prefix("single-preparation", 20, 4, false))?;
        ended(tail, String::new())?;
        let mut a = campaign
            .attempt("concurrent-copies", Arc::new(AllowAll))
            .await?;
        let request = a.replay.as_ref().context("request")?.clone();
        let (first, second) = tokio::time::timeout(TIMEOUT, async {
            tokio::join!(
                a.pipeline.clone().execute_stream(request.clone()),
                a.pipeline.clone().execute_stream(request)
            )
        })
        .await
        .context("copied preparations did not settle")?;
        a.stream = Some(match (first, second) {
            (Ok(stream), Err(_)) | (Err(_), Ok(stream)) => stream,
            _ => anyhow::bail!("copies did not have exactly one preparation owner"),
        });
        ensure!(
            a.execution.failure().is_none(),
            "rejected copy mutated the admitted owner"
        );
        a.drain().await?;
        drop(a.stream.take());
        drop(a.replay.take());
        campaign.quiescent(&a).await?;
        ensure!(
            a.terminals == 1
                && a.errors == 0
                && campaign.sends()? == 1
                && a.calls.load(Ordering::SeqCst) == 0
        );
        let facts = campaign.facts(&a).await?;
        let receipt: ExecutionReceipt = serde_json::from_slice(&facts.0)?;
        ensure!(
            receipt.outcome == Outcome::Completed
                && receipt.attempts.len() == 1
                && receipt.attempts[0].outcome == Outcome::Completed
        );
        ensure!(receipt.usage.input_tokens == Some(20) && receipt.usage.output_tokens == Some(4));
        {
            let records = a
                .recorded
                .lock()
                .map_err(|_| anyhow::anyhow!("settlement lock"))?;
            ensure!(
                records.len() == 1
                    && records[0].input == 20
                    && records[0].output == 4
                    && !records[0].failed
            );
        }
        let state = campaign.snapshot(&a)?;
        ensure!(
            state.active == 0
                && state.target_active == 0
                && state.pending == 0
                && state.sent == 1
                && state.samples == [Some(4)]
        );
        campaign.stop().await?;
        Ok(())
    }
    .await;
    let stopped = tokio::time::timeout(TIMEOUT, campaign.fixture.service.shutdown()).await;
    checked?;
    stopped??;
    Ok(())
}

async fn after_durable_ack(cancel: bool) -> Result<()> {
    let campaign = Campaign::new(ApiProtocol::Messages, 1).await?;
    let checked: Result<()> = async {
        let tail = campaign.hold(messages_prefix("ack-owner", 10, 3, false))?;
        let mut a = campaign
            .attempt("durable-ack-owner", Arc::new(AllowAll))
            .await?;
        let request = a.replay.as_ref().context("original request")?.clone();
        let mut prepared = tokio::time::timeout(
            TIMEOUT,
            Box::pin(a.pipeline.clone().execute_stream_prepared(request)),
        )
        .await??;
        // Consume data through the real prepared-delivery API, without turning
        // a successful private terminal into public output.
        ended(tail, String::new())?;
        let terminal = tokio::time::timeout(TIMEOUT, async {
            loop {
                let item = prepared.parts.next().await.context("terminal absent")??;
                if item.part.is_terminal() {
                    break anyhow::Ok(item);
                }
            }
        })
        .await??;
        let delivery = terminal
            .delivery
            .context("successful terminal lacks delivery custody")?;
        tokio::time::timeout(TIMEOUT, delivery.deliver()).await??;
        ensure!(
            a.execution.receipt_owner()?.durable(),
            "delivery returned before durable receipt ACK"
        );
        let ack_facts = campaign.facts(&a).await?;
        let receipt: ExecutionReceipt = serde_json::from_slice(&ack_facts.0)?;
        ensure!(
            receipt.outcome == Outcome::Completed
                && receipt.attempts.len() == 1
                && receipt.attempts[0].outcome == Outcome::Completed
        );
        ensure!(receipt.usage.input_tokens == Some(10) && receipt.usage.output_tokens == Some(3));
        ensure!(campaign.snapshot(&a)?.active == 0 && campaign.snapshot(&a)?.samples == [Some(3)]);
        let current_tail = campaign.hold(messages_prefix("after-ack-current", 20, 4, true))?;
        let mut current = campaign
            .attempt("after-ack-current", Arc::new(AllowAll))
            .await?;
        ensure!(current.open().await?);
        current.received(20, 4).await?;
        let held = campaign.snapshot(&current)?;
        ensure!(
            held.active == 1
                && held.target_active == 1
                && held.sent == 2
                && held.samples == [Some(3)]
        );
        if cancel {
            a.execution.receipt_owner()?.cancel();
        }
        // The ACKed terminal is deliberately dropped instead of published.
        drop(terminal.part);
        drop(prepared.parts);
        // Exercise the actual copied registration after the durable ACK. It
        // must not create a second failure proposal or poison admission.
        a.replay_refused().await?;
        campaign.quiescent(&a).await?;
        ensure!(
            campaign.facts(&a).await? == ack_facts
                && campaign.snapshot(&current)? == held
                && campaign.sends()? == 2
        );
        ensure!(a.calls.load(Ordering::SeqCst) == 0 && a.terminals == 0);
        refuse_contender(&campaign, &current, "after-ack-contender").await?;
        drop(current.stream.take());
        let current_facts = campaign
            .closed(&mut current, 20, 4, Outcome::Unknown)
            .await?;
        current.replay_refused().await?;
        campaign.quiescent(&current).await?;
        if current_tail.send(Vec::new()).is_ok() {
            tokio::task::yield_now().await;
        }
        recover(&campaign, &current, 40, 7, 3).await?;
        ensure!(
            campaign.facts(&a).await? == ack_facts
                && campaign.facts(&current).await? == current_facts
        );
        campaign.stop().await?;
        Ok(())
    }
    .await;
    if let Err(error) = &checked {
        eprintln!("post-ACK custody failure before teardown: {error:#}");
    }
    let stopped = tokio::time::timeout(TIMEOUT, campaign.fixture.service.shutdown()).await;
    checked?;
    stopped??;
    Ok(())
}
#[tokio::test]
async fn real_tenant_drop_after_durable_ack_preserves_current_owner_and_receipt() -> Result<()> {
    after_durable_ack(false).await
}
#[tokio::test]
async fn real_tenant_cancel_after_durable_ack_preserves_current_owner_and_receipt() -> Result<()> {
    after_durable_ack(true).await
}

#[tokio::test]
async fn real_tenant_direct_json_validation_cannot_finalize_unclaimed_live_owner() -> Result<()> {
    let campaign = Campaign::new(ApiProtocol::Messages, 1).await?;
    let checked: Result<()> = async {
        let tail = campaign.hold(messages_prefix("direct-json-owner", 10, 3, true))?;
        let gate = Arc::new(ApprovalGate::default());
        let mut original = campaign.attempt("direct-json-owner", gate.clone()).await?;
        ensure!(original.open().await?);
        ended(tail, String::new())?;
        original.approval(&gate).await?;
        let current_tail = campaign.hold(messages_prefix("direct-json-current", 20, 4, true))?;
        let mut current = campaign
            .attempt("direct-json-current", Arc::new(AllowAll))
            .await?;
        ensure!(current.open().await?);
        current.received(20, 4).await?;
        let held = campaign.snapshot(&current)?;
        for mode in [
            "valid copied registration",
            "missing registration",
            "wrong pipeline",
            "missing execution owner",
            "stripped extensions",
        ] {
            for method in ["direct JSON", "detached JSON", "stream"] {
                let mut request = original
                    .replay
                    .as_ref()
                    .context("original request")?
                    .clone();
                match mode {
                    "missing registration" => {
                        request
                            .extensions
                            .remove::<Arc<crate::tenancy::execution::lifecycle::Registration>>();
                    }
                    "missing execution owner" => {
                        request.extensions.remove::<Arc<TenantExecution>>();
                    }
                    "stripped extensions" => {
                        request.extensions = Default::default();
                    }
                    _ => {}
                }
                let pipeline = if mode == "wrong pipeline" {
                    &current.pipeline
                } else {
                    &original.pipeline
                };
                let refused = match method {
                    "direct JSON" => tokio::time::timeout(TIMEOUT, pipeline.execute(request))
                        .await
                        .context("direct JSON validation stalled")?
                        .is_err(),
                    "detached JSON" => {
                        tokio::time::timeout(TIMEOUT, pipeline.clone().execute_detached(request))
                            .await
                            .context("detached JSON validation stalled")?
                            .is_err()
                    }
                    _ => tokio::time::timeout(TIMEOUT, pipeline.clone().execute_stream(request))
                        .await
                        .context("stream validation stalled")?
                        .is_err(),
                };
                ensure!(refused, "{mode}/{method} was accepted");
                ensure!(
                    !original.execution.receipt_owner()?.durable(),
                    "{mode}/{method} finalized a live owner without a preparation claim"
                );
                ensure!(
                    original.execution.failure().is_none() && current.execution.failure().is_none(),
                    "{mode}/{method} mutated current refusal evidence"
                );
                ensure!(
                    original
                        .recorded
                        .lock()
                        .map_err(|_| anyhow::anyhow!("settlement lock"))?
                        .is_empty(),
                    "{mode}/{method} caused an unowned settlement"
                );
                ensure!(campaign.snapshot(&current)? == held && campaign.sends()? == 2);
            }
        }
        drop(original.stream.take());
        let original_facts = campaign
            .closed(&mut original, 10, 3, Outcome::Completed)
            .await?;
        original.replay_refused().await?;
        campaign.quiescent(&original).await?;
        ensure!(campaign.snapshot(&current)? == held && original.calls.load(Ordering::SeqCst) == 0);
        drop(current.stream.take());
        let current_facts = campaign
            .closed(&mut current, 20, 4, Outcome::Unknown)
            .await?;
        current.replay_refused().await?;
        campaign.quiescent(&current).await?;
        if current_tail.send(Vec::new()).is_ok() {
            tokio::task::yield_now().await;
        }
        recover(&campaign, &current, 40, 7, 3).await?;
        ensure!(
            campaign.facts(&original).await? == original_facts
                && campaign.facts(&current).await? == current_facts
        );
        campaign.stop().await?;
        Ok(())
    }
    .await;
    if let Err(error) = &checked {
        eprintln!("direct JSON custody failure before teardown: {error:#}");
    }
    let stopped = tokio::time::timeout(TIMEOUT, campaign.fixture.service.shutdown()).await;
    checked?;
    stopped??;
    Ok(())
}

#[tokio::test]
async fn real_tenant_fresh_direct_json_missing_registration_drains_its_own_lifecycle() -> Result<()>
{
    let campaign = Campaign::new(ApiProtocol::Messages, 1).await?;
    let checked: Result<()> = async {
        let mut fresh = campaign
            .attempt("fresh-missing-direct-registration", Arc::new(AllowAll))
            .await?;
        let mut request = fresh.replay.take().context("fresh request")?;
        request
            .extensions
            .remove::<Arc<crate::tenancy::execution::lifecycle::Registration>>();
        let replay = request.clone();
        ensure!(
            tokio::time::timeout(TIMEOUT, fresh.pipeline.execute(request))
                .await?
                .is_err()
        );
        campaign.quiescent(&fresh).await?;
        wait_until(|| Ok(fresh.execution.receipt_owner()?.durable())).await?;
        let facts = campaign.facts(&fresh).await?;
        let receipt: ExecutionReceipt = serde_json::from_slice(&facts.0)?;
        let evidence: ExecutionUsageEvidenceV1 = serde_json::from_slice(&facts.1)?;
        ensure!(
            receipt.execution_id == *fresh.execution.id()
                && receipt.outcome != Outcome::Completed
                && receipt.attempts.is_empty()
        );
        ensure!(evidence.attempts.is_empty() && evidence.frames.is_empty());
        ensure!(
            campaign.sends()? == 0
                && fresh.calls.load(Ordering::SeqCst) == 0
                && fresh
                    .recorded
                    .lock()
                    .map_err(|_| anyhow::anyhow!("settlement lock"))?
                    .is_empty()
        );
        ensure!(
            tokio::time::timeout(TIMEOUT, fresh.pipeline.execute(replay))
                .await?
                .is_err()
        );
        ensure!(campaign.facts(&fresh).await? == facts && campaign.sends()? == 0);
        campaign.stop().await?;
        Ok(())
    }
    .await;
    if let Err(error) = &checked {
        eprintln!("fresh direct JSON cleanup failure before teardown: {error:#}");
    }
    let stopped = tokio::time::timeout(TIMEOUT, campaign.fixture.service.shutdown()).await;
    checked?;
    stopped??;
    Ok(())
}
