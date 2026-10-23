//! P1-A01/A02 process candidates. All authority writes cross stock CLI/HTTP.
//!
//! Registered below `bootstrap` to reuse its private, UUID-owned empty database
//! and child cleanup. No in-process admission, schema initialization or SQL
//! mutation is used by this campaign.

#[path = "admission/backend_loss.rs"]
mod backend_loss;

#[path = "admission/recovery.rs"]
mod recovery;

use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{FromRequest, Request, State},
    http::Method,
    response::{IntoResponse, Response},
};
use bitrouter_ai::gateway::{
    execution::{AttemptState, ExecutionState, GatewayExecution, TerminalOutcome},
    operator::{Command as MutationCommand, Mutation, OperationLookup, ResourcePut, TenantPut},
};
use bitrouter_sdk::operator::OperatorClient;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use super::*;

#[path = "admission/guards.rs"]
mod guards;

#[path = "admission/authority.rs"]
mod authority;

#[path = "admission/outbox_delivery.rs"]
mod outbox_delivery;

#[path = "admission/overload.rs"]
mod overload;

#[path = "admission/continuation.rs"]
mod continuation;

#[path = "admission_tests.rs"]
mod tests;

const OPERATOR_TOKEN: &str = "fixture-admission-operator-0123456789abcdef";
const TENANT_POOL: &str = "tenant-pool";
const ACCOUNT_POOL: &str = "account-pool";
const OTHER_POOL: &str = "other-pool";
const SECRETS: [&str; 3] = [
    "fixture-tenant-key",
    "fixture-account-key",
    "fixture-other-key",
];
type Requests = JoinSet<Result<(String, Wire)>>;

#[derive(Clone, Copy)]
enum Campaign {
    Concurrency,
    Identity,
    Guards,
    GuardCost,
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_tenant_and_aliased_account_concurrency_cross_replicas() -> Result<()> {
    run(Campaign::Concurrency).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_duplicates_conflicts_and_unknown_status_cross_replicas() -> Result<()> {
    run(Campaign::Identity).await
}

async fn run(case: Campaign) -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let mut fixture = match empty_fixture(&mut database).await {
        Ok(fixture) => fixture,
        Err(error) => return database.finish(Err(error)).await,
    };
    let mut expected = ExitExpectations::new();
    let mut result = campaign(&mut fixture, case, &mut expected).await;
    if result.is_err() {
        let prefix = std::env::temp_dir().join(format!(
            "bitrouter-admission-failure-{}",
            uuid::Uuid::new_v4(),
        ));
        let mut copies = Vec::new();
        for (index, child) in fixture.children.iter().enumerate() {
            let path = prefix.with_extension(format!("{index}.log"));
            copies.push(
                std::fs::copy(&child.log, &path)
                    .map(|_| ())
                    .with_context(|| format!("retaining failed child log {}", path.display())),
            );
        }
        result = combine_results(result, copies)
            .with_context(|| format!("fixture child logs: {}.*.log", prefix.display()));
    }
    finish_empty_fixture_with_database(database, fixture, result, &expected).await
}

async fn campaign(
    fixture: &mut Fixture,
    case: Campaign,
    expected: &mut ExitExpectations,
) -> Result<()> {
    let mut mock = Mock::start().await?;
    let mut requests = Requests::new();
    let result = async {
        let world = match case {
            Campaign::Guards => World::start_seeded(fixture, &mock.base, guards::seed_plan).await?,
            Campaign::GuardCost => {
                World::start_seeded(fixture, &mock.base, guards::cost_seed_plan).await?
            }
            _ => World::start(fixture, &mock.base).await?,
        };
        for child in world.children {
            expected.insert(child, ExpectedExit::CleanServe);
        }
        if !matches!(case, Campaign::Guards | Campaign::GuardCost) {
            world.configure_limits().await?;
        }
        match case {
            Campaign::Concurrency => concurrency(&world, &mut mock, &mut requests).await?,
            Campaign::Identity => identity(&world, &mut mock, &mut requests, expected).await?,
            Campaign::Guards => guards::campaign(fixture, &world, &mut mock, &mut requests).await?,
            Campaign::GuardCost => {
                guards::cost_campaign(fixture, &world, &mut mock, &mut requests).await?
            }
        }
        ensure!(
            requests.is_empty(),
            "campaign left an unjoined client request"
        );
        fixture.no_local_files()?;
        Ok(())
    }
    .await;
    // Every spawned client is retained, cancelled and joined even on an assertion
    // error. Mock shutdown wakes every handler, then joins its serving root.
    requests.abort_all();
    let mut cleanup = Vec::new();
    while let Some(joined) = requests.join_next().await {
        match joined {
            Ok(Ok((label, wire))) => cleanup.push(Err(anyhow::anyhow!(
                "unconsumed client {label}: HTTP {}, execution={:?}, correlation={:?}, location={:?}: {}",
                wire.status,
                wire.headers.get("x-bitrouter-execution-id"),
                wire.headers.get("x-bitrouter-request-id"),
                wire.headers.get("location"),
                String::from_utf8_lossy(&wire.body),
            ))),
            Ok(Err(error)) => cleanup.push(Err(error).context("joining fixture client")),
            Err(error) if error.is_cancelled() => {}
            Err(error) => cleanup.push(Err(error).context("joining fixture client task")),
        }
    }
    cleanup.push(mock.finish().await.context("joining mock provider"));
    combine_results(result, cleanup)
}

struct World {
    http: reqwest::Client,
    replicas: [SocketAddr; 2],
    children: [usize; 2],
    operators: [OperatorClient; 2],
    // First two principals share a tenant. Last two have distinct tenants.
    keys: [crate::auth::keys::GeneratedKey; 4],
}

impl World {
    async fn start(fixture: &mut Fixture, base: &str) -> Result<Self> {
        Self::start_seeded(fixture, base, |base, keys| Ok(seed_plan(base, keys))).await
    }

    async fn start_seeded(
        fixture: &mut Fixture,
        base: &str,
        plan: fn(&str, &[crate::auth::keys::GeneratedKey; 4]) -> Result<Value>,
    ) -> Result<Self> {
        let keys = std::array::from_fn(|_| crate::auth::keys::generate());
        seed(fixture, &plan(base, &keys)?).await?;
        let mut replicas = Vec::new();
        let mut children = Vec::new();
        let mut operators = Vec::new();
        // Allocate the second pair after the first pair is bound, avoiding
        // accidentally reusing our own released ephemeral reservation.
        for _ in 0..2 {
            let inference = available_address()?;
            let operator = loop {
                let candidate = available_address()?;
                if candidate != inference {
                    break candidate;
                }
            };
            let child = fixture
                .spawn_with_operator("serve", inference, &[], Some((operator, OPERATOR_TOKEN)))
                .await?;
            children.push(child);
            fixture.child(child)?.ready().await?;
            replicas.push(inference);
            operators.push(OperatorClient::new(
                &format!("http://{operator}"),
                OPERATOR_TOKEN,
            )?);
        }
        let replicas = replicas
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing replica pair"))?;
        let children = children
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing child pair"))?;
        let operators = operators
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing operator pair"))?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(8))
            .build()?;
        Ok(Self {
            http,
            replicas,
            children,
            operators,
            keys,
        })
    }

    async fn configure_limits(&self) -> Result<()> {
        // Ordinary authenticated operator writes, with cross-replica receipts
        // and reads, establish the limits used below. Bootstrap starts at eight.
        let mut tenant = self.operators[0].tenant("limited").await?;
        tenant.limits.concurrent_executions = 1;
        let mutations = [
            Mutation {
                operation_id: "limit-tenant".into(),
                command: MutationCommand::PutTenant(TenantPut {
                    tenant_id: tenant.tenant_id,
                    expected_revision: Some(tenant.revision),
                    limits: tenant.limits,
                }),
            },
            Mutation {
                operation_id: "limit-account".into(),
                command: MutationCommand::PutResource(ResourcePut {
                    resource_pool_id: ACCOUNT_POOL.into(),
                    expected_revision: Some(
                        self.operators[0].resource(ACCOUNT_POOL).await?.revision,
                    ),
                    concurrent_attempts: 1,
                }),
            },
        ];
        for mutation in mutations {
            let receipt = self.operators[0].apply(&mutation).await?;
            ensure!(
                matches!(self.operators[1].operation(&mutation.operation_id).await?,
                OperationLookup::Committed { receipt: other } if *other == receipt)
            );
        }
        ensure!(
            self.operators[1]
                .tenant("limited")
                .await?
                .limits
                .concurrent_executions
                == 1
        );
        ensure!(
            self.operators[1]
                .resource(ACCOUNT_POOL)
                .await?
                .concurrent_attempts
                == 1
        );
        Ok(())
    }

    fn post(
        &self,
        replica: usize,
        principal: usize,
        model: &str,
        tag: &str,
        idempotency: &str,
        correlation: &str,
    ) -> reqwest::RequestBuilder {
        self.http
            .post(format!(
                "http://{}/v1/chat/completions",
                self.replicas[replica]
            ))
            .bearer_auth(&self.keys[principal].secret)
            .header("idempotency-key", idempotency)
            .header("x-bitrouter-request-id", correlation)
            .json(&json!({"model":model,"messages":[{"role":"user","content":tag}]}))
    }

    async fn status(&self, replica: usize, principal: usize, id: &str) -> Result<Wire> {
        Wire::read(
            self.http
                .get(format!(
                    "http://{}/v1/gateway/executions/{id}",
                    self.replicas[replica]
                ))
                .bearer_auth(&self.keys[principal].secret)
                .send()
                .await?,
        )
        .await
    }

    async fn wait_state(
        &self,
        replica: usize,
        principal: usize,
        id: &str,
        predicate: fn(&ExecutionState) -> bool,
    ) -> Result<GatewayExecution> {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let wire = self.status(replica, principal, id).await?;
                wire.expect_status(StatusCode::OK)?;
                let view = wire.execution()?;
                wire.identity(id, &view.correlation_id)?;
                if predicate(&view.state) {
                    return Ok(view);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("public execution state did not converge")?
    }
}

fn seed_plan(base: &str, keys: &[crate::auth::keys::GeneratedKey; 4]) -> Value {
    let pools = [TENANT_POOL, ACCOUNT_POOL, OTHER_POOL];
    let tenants = ["limited", "tenant-x", "tenant-y"];
    let principal_tenants = ["limited", "limited", "tenant-x", "tenant-y"];
    let mut providers = serde_json::Map::new();
    let mut bindings = Vec::new();
    for (provider, account) in [
        ("tenant", "tenant"),
        ("account-a", "account"),
        ("account-b", "account"),
        ("other", "other"),
    ] {
        providers.insert(
            provider.into(),
            json!({"api_base":format!("{base}/{account}"),
            "active":true,"auto_discover":false,"api_protocol":[{"*":"chat_completions"}],
            "models":[{"id":"mock"}]}),
        );
        bindings.push(json!({"provider":provider,"account":"default",
            "credential_provider":"fixture","credential_account":account}));
    }
    json!({
        "resources": pools.iter().map(|pool| json!({"resource_pool_id":pool,
            "expected_revision":null,"concurrent_attempts":8})).collect::<Vec<_>>(),
        "tenants": tenants.iter().map(|tenant| json!({"tenant_id":tenant,"expected_revision":null,
            "limits":{"concurrent_executions":8,"requests_per_minute":1000,"resource_pools":pools}
        })).collect::<Vec<_>>(),
        "principals": keys.iter().zip(principal_tenants).enumerate().map(|(i,(key,tenant))| json!({
            "principal_id":format!("fixture-principal-{i}"),"tenant_id":tenant,
            "expected_authorization_revision":null,"key_hash":key.hash,"active":true,
            "expires_at_ms":null,"requests_per_minute":null,"policy_id":null
        })).collect::<Vec<_>>(),
        "credentials": (["tenant","account","other"].iter().zip(pools).zip(SECRETS)
            .map(|((account,pool),secret)| json!({"provider":"fixture","account":account,
                "resource_pool_id":pool,"expected_version":null,
                "credential":{"type":"api_key","value":secret}})).collect::<Vec<_>>()),
        "configuration":{"format":1,"routing":{"providers":providers},"policies":[],
            "policy_lock":null,"credentials":bindings,"require_known_pricing":false}
    })
}

async fn seed(fixture: &mut Fixture, plan: &Value) -> Result<()> {
    let config = fixture.root.path().join("gateway-admission-init.json");
    let plan_path = fixture.root.path().join("gateway-admission-seed.json");
    tokio::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "database":{"url":"${FIXTURE_SHARED_DATABASE}"},
            "gateway_state":{"mode":"shared","namespace":fixture.authority.store().namespace(),
                "deployment_key_env":"FIXTURE_SHARED_KEY","transaction_timeout_ms":5000}
        }))?,
    )
    .await?;
    tokio::fs::write(&plan_path, serde_json::to_vec(plan)?).await?;
    let index = fixture.children.len();
    let report = fixture
        .root
        .path()
        .join(format!("gateway-init-{index}.json"));
    let log = fixture
        .root
        .path()
        .join(format!("gateway-init-{index}.log"));
    // Unlike the empty seed helper, this seed explicitly creates global
    // principals, so exercise the real CLI's opt-in flag (not private writers).
    let child = Command::new(&fixture.binary)
        .args(["shared", "init", "--config"])
        .arg(config)
        .arg("--plan")
        .arg(plan_path)
        .args([
            "--operator-id",
            "admission-fixture",
            "--operation-id",
            "seed",
            "--global-principals",
        ])
        .current_dir(fixture.root.path().join("launch"))
        .env("FIXTURE_SHARED_DATABASE", &fixture.url)
        .env("FIXTURE_SHARED_KEY", STANDARD.encode(fixture.key))
        .env(
            "BITROUTER_HOME",
            fixture.root.path().join("unexpected-home"),
        )
        .env("HOME", fixture.root.path().join("unexpected-user-home"))
        .env_remove("RUST_LOG")
        .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(&report)?))
        .stderr(Stdio::from(std::fs::File::create(&log)?))
        .kill_on_drop(true)
        .spawn()?;
    fixture.children.push(Running {
        child,
        log,
        address: "127.0.0.1:0".parse()?,
    });
    let receipt = completed_init(fixture, (index, report)).await?;
    ensure!(receipt.namespace == fixture.authority.store().namespace());
    ensure!(receipt.revision.config_generation == 1 && receipt.revision.key_generation == 1);
    Ok(())
}

struct Wire {
    status: StatusCode,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}
impl Wire {
    async fn read(response: reqwest::Response) -> Result<Self> {
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await?.to_vec();
        ensure!(body.len() <= 65536, "unexpected fixture response size");
        Ok(Self {
            status,
            headers,
            body,
        })
    }
    fn expect_status(&self, status: StatusCode) -> Result<()> {
        ensure!(
            self.status == status,
            "expected {status}, got {}: {}",
            self.status,
            String::from_utf8_lossy(&self.body)
        );
        Ok(())
    }
    fn header(&self, name: &str) -> Result<&str> {
        ensure!(
            self.headers.get_all(name).iter().count() == 1,
            "missing/duplicate {name}"
        );
        Ok(self
            .headers
            .get(name)
            .context("missing response header")?
            .to_str()?)
    }
    fn identity(&self, id: &str, correlation: &str) -> Result<()> {
        ensure!(self.header("x-bitrouter-execution-id")? == id);
        ensure!(self.header("x-bitrouter-request-id")? == correlation);
        ensure!(self.header("location")? == format!("/v1/gateway/executions/{id}"));
        Ok(())
    }
    fn execution(&self) -> Result<GatewayExecution> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

fn start(requests: &mut Requests, label: &str, request: reqwest::RequestBuilder) {
    let label = label.to_owned();
    requests.spawn(async move { Ok((label, Wire::read(request.send().await?).await?)) });
}
async fn completed(requests: &mut Requests, label: &str) -> Result<Wire> {
    let (actual, wire) = completed_any(requests).await?;
    ensure!(
        actual == label,
        "unexpected request completed: {actual} instead of {label}"
    );
    Ok(wire)
}

async fn completed_any(requests: &mut Requests) -> Result<(String, Wire)> {
    tokio::time::timeout(Duration::from_secs(5), requests.join_next())
        .await
        .context("client request did not finish")?
        .context("missing client request")??
}

#[derive(Clone, Copy)]
enum Reply {
    Success,
    MalformedSuccess,
}
struct Arrival {
    tag: String,
    model: String,
    path: String,
    authorization: String,
    execution: String,
    release: oneshot::Sender<Reply>,
}
impl Arrival {
    fn release(self, reply: Reply) -> Result<()> {
        self.release
            .send(reply)
            .map_err(|_| anyhow::anyhow!("upstream handler ended before release"))
    }
}
#[derive(Clone)]
struct MockState {
    arrivals: mpsc::Sender<Arrival>,
    stop: CancellationToken,
    calls: Arc<AtomicUsize>,
    rejected: Arc<AtomicUsize>,
    rejection_kinds: Arc<AtomicUsize>,
}
struct Mock {
    base: String,
    arrivals: mpsc::Receiver<Arrival>,
    state: MockState,
    roots: JoinSet<std::io::Result<()>>,
}
impl Mock {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let (state, arrivals) = MockState::new();
        let app = mock_router(state.clone());
        let stop = state.stop.clone();
        let mut roots = JoinSet::new();
        roots.spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        });
        Ok(Self {
            base,
            arrivals,
            state,
            roots,
        })
    }
    fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }
    async fn arrival(&mut self, tag: &str, account: &str, secret: &str) -> Result<Arrival> {
        let arrival = tokio::time::timeout(Duration::from_secs(4), self.arrivals.recv())
            .await
            .with_context(|| format!(
                "stock gateway never reached mock upstream for {tag}; HTTP arrivals={}, rejected={}, rejection bitset={:#x}",
                self.calls(), self.state.rejected.load(Ordering::SeqCst),
                self.state.rejection_kinds.load(Ordering::SeqCst),
            ))?
            .context("mock arrival channel ended")?;
        ensure!(
            arrival.tag == tag,
            "unexpected provider dispatch: {} instead of {tag}",
            arrival.tag
        );
        ensure!(arrival.path == format!("/{account}/chat/completions"));
        ensure!(
            arrival.authorization == format!("Bearer {secret}"),
            "wrong imported credential"
        );
        ensure!(
            uuid::Uuid::parse_str(&arrival.execution).is_ok(),
            "provider did not receive execution identity"
        );
        Ok(arrival)
    }
    async fn finish(mut self) -> Result<()> {
        self.state.stop.cancel();
        let joined = async {
            match tokio::time::timeout(Duration::from_secs(3), self.roots.join_next()).await {
                Ok(root) => root.context("missing mock serving root")???,
                Err(_) => {
                    self.roots.abort_all();
                    while self.roots.join_next().await.is_some() {}
                    anyhow::bail!("mock graceful shutdown timed out");
                }
            }
            Ok(())
        }
        .await;
        // Do not turn a rejected request into a silent missing Arrival, even
        // if the serving root itself also failed or timed out.
        combine_results(joined, vec![self.state.validate()])
    }
}

// A fixed bitset and counter retain validation failures without storing caller
// bodies/credentials or growing an unbounded error queue.
const INVALID_METHOD: usize = 1;
const INVALID_PATH: usize = 2;
const INVALID_JSON: usize = 4;
const INVALID_TAG: usize = 8;

impl MockState {
    fn new() -> (Self, mpsc::Receiver<Arrival>) {
        let (arrivals, receiver) = mpsc::channel(64);
        (
            Self {
                arrivals,
                stop: CancellationToken::new(),
                calls: Arc::default(),
                rejected: Arc::default(),
                rejection_kinds: Arc::default(),
            },
            receiver,
        )
    }

    fn reject(&self, kind: usize) {
        self.rejected.fetch_add(1, Ordering::SeqCst);
        self.rejection_kinds.fetch_or(kind, Ordering::SeqCst);
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.rejected.load(Ordering::SeqCst) == 0,
            "mock rejected {} HTTP requests (method/path/json/tag bitset={:#x})",
            self.rejected.load(Ordering::SeqCst),
            self.rejection_kinds.load(Ordering::SeqCst)
        );
        Ok(())
    }
}

fn mock_router(state: MockState) -> Router {
    Router::new().fallback(mock_request).with_state(state)
}

async fn mock_request(State(state): State<MockState>, request: Request) -> Response {
    // Request is infallible; count before method/path/body/media extraction.
    // https://docs.rs/axum/0.8/axum/extract/index.html#the-order-of-extractors
    state.calls.fetch_add(1, Ordering::SeqCst);
    if request.method() != Method::POST {
        state.reject(INVALID_METHOD);
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if request.uri().query().is_some()
        || !matches!(
            request.uri().path(),
            "/tenant/chat/completions" | "/account/chat/completions" | "/other/chat/completions"
        )
    {
        state.reject(INVALID_PATH);
        return StatusCode::NOT_FOUND.into_response();
    }
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let Json(body) = match Json::<Value>::from_request(request, &()).await {
        Ok(body) => body,
        Err(error) => {
            state.reject(INVALID_JSON);
            return error.into_response();
        }
    };
    let Some(tag) = body["messages"][0]["content"]
        .as_str()
        .filter(|tag| !tag.is_empty())
    else {
        state.reject(INVALID_TAG);
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (release, released) = oneshot::channel();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    let arrival = Arrival {
        tag: tag.to_owned(),
        model: body["model"].as_str().unwrap_or("").to_owned(),
        path: uri.path().to_owned(),
        authorization: header("authorization"),
        execution: header("x-bitrouter-request-id"),
        release,
    };
    let sent = tokio::select! {
        _ = state.stop.cancelled() => false,
        sent = state.arrivals.send(arrival) => sent.is_ok(),
    };
    if !sent {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let reply = tokio::select! {
        _ = state.stop.cancelled() => None,
        reply = released => reply.ok(),
    };
    match reply {
        Some(Reply::Success) => Json(json!({"id":"mock-result","object":"chat.completion",
            "created":1,"model":"mock","choices":[{"index":0,
                "message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}))
        .into_response(),
        // An observed HTTP 200 with invalid model JSON establishes no provider
        // terminal. This is the same ambiguity used by dispatched::ambiguous.
        Some(Reply::MalformedSuccess) => (
            [("content-type", "application/json")],
            Body::from("not-json"),
        )
            .into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn concurrency(w: &World, mock: &mut Mock, jobs: &mut Requests) -> Result<()> {
    for (owner, contender) in [(0, 1), (1, 0)] {
        let held = format!("tenant-held-{owner}");
        start(
            jobs,
            &held,
            w.post(owner, 0, "tenant:mock", &held, &held, &held),
        );
        let arrival = mock.arrival(&held, "tenant", SECRETS[0]).await?;
        let before = mock.calls();
        let denied = format!("tenant-denied-{owner}");
        let wire = Wire::read(
            w.post(contender, 1, "tenant:mock", &denied, &denied, &denied)
                .send()
                .await?,
        )
        .await?;
        wire.expect_status(StatusCode::TOO_MANY_REQUESTS)?;
        ensure!(
            !wire.headers.contains_key("x-bitrouter-execution-id"),
            "tenant rejection admitted work"
        );
        ensure!(mock.calls() == before);
        // A different tenant using the SAME account is admitted while held:
        // this rejection was tenant-scoped, not a process/global/account cap.
        let control = format!("tenant-control-{owner}");
        start(
            jobs,
            &control,
            w.post(contender, 2, "tenant:mock", &control, &control, &control),
        );
        let control_arrival = mock.arrival(&control, "tenant", SECRETS[0]).await?;
        control_arrival.release(Reply::Success)?;
        completed(jobs, &control)
            .await?
            .expect_status(StatusCode::OK)?;
        arrival.release(Reply::Success)?;
        completed(jobs, &held)
            .await?
            .expect_status(StatusCode::OK)?;
        // Tenant rejection did not consume/bind its idempotency key.
        start(
            jobs,
            &denied,
            w.post(contender, 1, "tenant:mock", &denied, &denied, &denied),
        );
        mock.arrival(&denied, "tenant", SECRETS[0])
            .await?
            .release(Reply::Success)?;
        completed(jobs, &denied)
            .await?
            .expect_status(StatusCode::OK)?;
        ensure!(mock.calls() == before + 2);
    }
    for (owner, contender) in [(0, 1), (1, 0)] {
        let held = format!("account-held-{owner}");
        start(
            jobs,
            &held,
            w.post(owner, 2, "account-a:mock", &held, &held, &held),
        );
        let arrival = mock.arrival(&held, "account", SECRETS[1]).await?;
        let before = mock.calls();
        let denied = format!("account-denied-{owner}");
        let wire = Wire::read(
            w.post(contender, 3, "account-b:mock", &denied, &denied, &denied)
                .send()
                .await?,
        )
        .await?;
        wire.expect_status(StatusCode::TOO_MANY_REQUESTS)?;
        ensure!(
            mock.calls() == before,
            "route alias multiplied account capacity"
        );
        // Same contending tenant, independent actual account: succeeds while
        // original provider handler remains blocked on its one-shot release.
        let control = format!("account-control-{owner}");
        start(
            jobs,
            &control,
            w.post(contender, 3, "other:mock", &control, &control, &control),
        );
        mock.arrival(&control, "other", SECRETS[2])
            .await?
            .release(Reply::Success)?;
        completed(jobs, &control)
            .await?
            .expect_status(StatusCode::OK)?;
        arrival.release(Reply::Success)?;
        completed(jobs, &held)
            .await?
            .expect_status(StatusCode::OK)?;
        // Account rejection can already own an execution. Use an explicitly NEW
        // idempotency key; never replay its inference under the rejected key.
        let after = format!("account-after-{owner}");
        start(
            jobs,
            &after,
            w.post(contender, 3, "account-b:mock", &after, &after, &after),
        );
        mock.arrival(&after, "account", SECRETS[1])
            .await?
            .release(Reply::Success)?;
        completed(jobs, &after)
            .await?
            .expect_status(StatusCode::OK)?;
        ensure!(mock.calls() == before + 2);
    }
    ensure!(mock.calls() == 12);
    Ok(())
}

fn succeeded(state: &ExecutionState) -> bool {
    matches!(state, ExecutionState::Terminal { result } if result.outcome == TerminalOutcome::Succeeded)
}
fn unknown(state: &ExecutionState) -> bool {
    matches!(state, ExecutionState::Unknown { .. })
}

async fn duplicate(
    w: &World,
    id: &str,
    model: &str,
    principal: usize,
    tag: &str,
    key: &str,
    correlation: &str,
) -> Result<GatewayExecution> {
    let wire = Wire::read(
        w.post(1, principal, model, tag, key, correlation)
            .send()
            .await?,
    )
    .await?;
    wire.expect_status(StatusCode::ACCEPTED)?;
    wire.identity(id, correlation)?;
    let view = wire.execution()?;
    ensure!(view.execution_id == id && view.attempts.len() == 1);
    Ok(view)
}

// Both first submissions are live concurrently; neither waits for an HTTP
// response or a private admission signal from the other replica.
async fn raced_duplicate(w: &World, mock: &mut Mock, jobs: &mut Requests) -> Result<()> {
    for (replica, label) in [(0, "race-a"), (1, "race-b")] {
        start(
            jobs,
            label,
            w.post(replica, 0, "tenant:mock", "raced", "race-id", label),
        );
    }
    let arrival = mock.arrival("raced", "tenant", SECRETS[0]).await?;
    let id = arrival.execution.clone();
    let (loser, duplicate) = completed_any(jobs).await?;
    duplicate.expect_status(StatusCode::ACCEPTED)?;
    duplicate.identity(&id, &loser)?;
    let view = duplicate.execution()?;
    let winner = if loser == "race-a" {
        "race-b"
    } else {
        "race-a"
    };
    ensure!(loser == "race-a" || loser == "race-b");
    ensure!(view.execution_id == id && view.correlation_id == winner);
    // The duplicate may have snapshotted admission before provider dispatch.
    ensure!(matches!(
        view.state,
        ExecutionState::Accepted | ExecutionState::Running
    ));
    ensure!(view.attempts.len() <= 1);
    let current = w.status(1, 0, &id).await?;
    current.expect_status(StatusCode::OK)?;
    let current = current.execution()?;
    ensure!(matches!(current.state, ExecutionState::Running) && current.attempts.len() == 1);
    ensure!(matches!(
        current.attempts[0].state,
        AttemptState::Dispatched
    ));
    ensure!(
        mock.calls() == 1,
        "racing first submissions dispatched twice"
    );
    arrival.release(Reply::Success)?;
    let response = completed(jobs, winner).await?;
    response.expect_status(StatusCode::OK)?;
    response.identity(&id, winner)?;
    w.wait_state(1, 0, &id, succeeded).await?;
    Ok(())
}

fn same_facts(a: &GatewayExecution, b: &GatewayExecution) -> Result<()> {
    // Reconciliation may advance revision/update time between two HTTP reads.
    ensure!(a.execution_id == b.execution_id && a.correlation_id == b.correlation_id);
    ensure!(a.operation == b.operation && a.config_generation == b.config_generation);
    ensure!(a.accepted_at_ms == b.accepted_at_ms && a.state == b.state && a.attempts == b.attempts);
    Ok(())
}

async fn identity(
    w: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
    expected: &mut ExitExpectations,
) -> Result<()> {
    raced_duplicate(w, mock, jobs).await?;
    start(
        jobs,
        "original",
        w.post(
            0,
            0,
            "tenant:mock",
            "original",
            "same-id",
            "first-correlation",
        ),
    );
    let arrival = mock.arrival("original", "tenant", SECRETS[0]).await?;
    let id = arrival.execution.clone();
    let view = duplicate(
        w,
        &id,
        "tenant:mock",
        0,
        "original",
        "same-id",
        "duplicate-correlation",
    )
    .await?;
    ensure!(
        view.correlation_id == "first-correlation" && matches!(view.state, ExecutionState::Running)
    );
    ensure!(matches!(view.attempts[0].state, AttemptState::Dispatched));
    ensure!(view.attempts[0].resource_pool_id == TENANT_POOL);
    for replica in 0..2 {
        let wire = w.status(replica, 0, &id).await?;
        wire.expect_status(StatusCode::OK)?;
        wire.identity(&id, "first-correlation")?;
        same_facts(&wire.execution()?, &view)?;
    }
    // Same tenant but different principal may read tenant status, yet cannot
    // reuse the original principal's idempotency binding.
    w.status(1, 1, &id).await?.expect_status(StatusCode::OK)?;
    w.status(1, 3, &id)
        .await?
        .expect_status(StatusCode::NOT_FOUND)?;
    w.status(1, 0, &uuid::Uuid::new_v4().to_string())
        .await?
        .expect_status(StatusCode::NOT_FOUND)?;
    let unauthenticated = Wire::read(
        w.http
            .get(format!(
                "http://{}/v1/gateway/executions/{id}",
                w.replicas[1]
            ))
            .send()
            .await?,
    )
    .await?;
    unauthenticated.expect_status(StatusCode::UNAUTHORIZED)?;
    for (principal, tag) in [(0, "changed-body"), (1, "original")] {
        let wire = Wire::read(
            w.post(1, principal, "tenant:mock", tag, "same-id", "conflict")
                .send()
                .await?,
        )
        .await?;
        wire.expect_status(StatusCode::CONFLICT)?;
        let body: Value = serde_json::from_slice(&wire.body)?;
        ensure!(body["error"]["code"] == "idempotency_conflict");
    }
    let changed_operation = Wire::read(
        w.http
            .post(format!("http://{}/v1/decisions", w.replicas[1]))
            .bearer_auth(&w.keys[0].secret)
            .header("idempotency-key", "same-id")
            .json(&json!({"model":"tenant:mock","input":"evidence",
            "questions":[{"type":"predicate","name":"q","instructions":"evaluate"}]}))
            .send()
            .await?,
    )
    .await?;
    changed_operation.expect_status(StatusCode::CONFLICT)?;
    ensure!(mock.calls() == 2);
    arrival.release(Reply::Success)?;
    let original = completed(jobs, "original").await?;
    original.expect_status(StatusCode::OK)?;
    original.identity(&id, "first-correlation")?;
    let terminal = w.wait_state(1, 0, &id, succeeded).await?;
    let after = duplicate(
        w,
        &id,
        "tenant:mock",
        0,
        "original",
        "same-id",
        "terminal-duplicate",
    )
    .await?;
    same_facts(&after, &terminal)?;
    ensure!(after.correlation_id == "first-correlation");
    ensure!(mock.calls() == 2);
    // Correlation is not an execution/idempotency identity.
    start(
        jobs,
        "independent",
        w.post(
            1,
            0,
            "tenant:mock",
            "original",
            "new-id",
            "first-correlation",
        ),
    );
    let independent = mock.arrival("original", "tenant", SECRETS[0]).await?;
    ensure!(independent.execution != id);
    independent.release(Reply::Success)?;
    completed(jobs, "independent")
        .await?
        .expect_status(StatusCode::OK)?;

    start(
        jobs,
        "uncertain",
        w.post(
            0,
            2,
            "account-a:mock",
            "uncertain",
            "unknown-id",
            "unknown-original",
        ),
    );
    let uncertain = mock.arrival("uncertain", "account", SECRETS[1]).await?;
    let unknown_id = uncertain.execution.clone();
    uncertain.release(Reply::MalformedSuccess)?;
    let failed = completed(jobs, "uncertain").await?;
    ensure!(
        failed.status.is_server_error(),
        "ambiguous provider success was not rejected"
    );
    failed.identity(&unknown_id, "unknown-original")?;
    let retained = w.wait_state(1, 2, &unknown_id, unknown).await?;
    ensure!(retained.attempts.len() == 1);
    ensure!(retained.attempts[0].resource_pool_id == ACCOUNT_POOL);
    ensure!(matches!(
        retained.attempts[0].state,
        AttemptState::Unknown { .. }
    ));
    same_facts(&w.wait_state(0, 2, &unknown_id, unknown).await?, &retained)?;
    // Only replica 0 dispatched this exact Unknown execution. Permit its narrow
    // truthful force outcome only after both public status reads established it;
    // init, the peer, and every other campaign still require successful exits.
    expected.insert(w.children[0], ExpectedExit::ObservedUnknown);

    let existing = duplicate(
        w,
        &unknown_id,
        "account-a:mock",
        2,
        "uncertain",
        "unknown-id",
        "unknown-duplicate",
    )
    .await?;
    same_facts(&existing, &retained)?;
    ensure!(existing.correlation_id == "unknown-original");
    Wire::read(
        w.post(
            1,
            2,
            "account-a:mock",
            "changed",
            "unknown-id",
            "unknown-conflict",
        )
        .send()
        .await?,
    )
    .await?
    .expect_status(StatusCode::CONFLICT)?;
    // A fresh execution on another tenant/alias cannot spend capacity retained
    // by the unknown provider attempt. No operator resolution is fabricated.
    Wire::read(
        w.post(
            1,
            3,
            "account-b:mock",
            "unknown-capacity",
            "fresh-id",
            "unknown-capacity",
        )
        .send()
        .await?,
    )
    .await?
    .expect_status(StatusCode::TOO_MANY_REQUESTS)?;
    ensure!(
        mock.calls() == 4,
        "duplicate/conflict/unknown recovery dispatched again"
    );
    Ok(())
}
