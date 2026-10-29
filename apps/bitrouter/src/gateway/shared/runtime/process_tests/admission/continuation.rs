//! Ordinary Responses native continuation across stock shared gateway processes.
//!
//! Every authority mutation uses the stock bootstrap or authenticated operator
//! HTTP. The mock checks the actual provider parent, endpoint and credential.

use bitrouter_ai::gateway::operator as admin;

use super::*;
use crate::gateway::shared::transport::{Limits, owned::Transport};

const REPLACEMENT: &str = "fixture-native-replacement-secret";

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_native_responses_cross_replicas_preserve_authority_and_tenant_scope() -> Result<()>
{
    let mut database = EmptyDatabase::create().await?;
    let mut fixture = match empty_fixture(&mut database).await {
        Ok(fixture) => fixture,
        Err(error) => return database.finish(Err(error)).await,
    };
    let mut expected = ExitExpectations::new();
    let result = run_native(&mut fixture, &mut expected).await;
    let result = database.finish(result).await;
    retain_failed_logs(
        fixture.children.iter().map(|child| child.log.as_path()),
        result,
        &failure_log_prefix(),
    )
}

async fn run_native(fixture: &mut Fixture, expected: &mut ExitExpectations) -> Result<()> {
    let mut provider = Responses::start().await?;
    let result = tokio::time::timeout(Duration::from_secs(75), async {
        let world = start_world(fixture, &provider.base, expected).await?;
        let original = success(&world, &mut provider, 0, "root", None, (1, SECRETS[0], 1)).await?;
        for replica in 0..2 {
            denied(
                &world,
                &provider,
                replica,
                2,
                &original.public_id,
                "foreign-root",
                "mapping is unavailable",
            )
            .await?;
            let foreign = world
                .status(replica, 2, &original.execution.execution_id)
                .await?;
            foreign.expect_status(StatusCode::NOT_FOUND)?;
            ensure!(!foreign.headers.contains_key("x-bitrouter-execution-id"));
            ensure!(
                !String::from_utf8_lossy(&foreign.body).contains(&original.execution.execution_id)
            );
        }
        // The second principal has the SAME tenant but a distinct key. The
        // provider sees the private native ID, never the public gateway handle.
        let peer = success(
            &world,
            &mut provider,
            1,
            "peer",
            Some(&original),
            (1, SECRETS[0], 1),
        )
        .await?;

        let revision = world.operators[0].revision().await?;
        let rotated = apply(
            &world,
            1,
            "native-rotate-correlation",
            admin::Command::RotateCorrelation {
                expected_generation: revision.key_generation,
            },
        )
        .await?;
        let admin::Outcome::CorrelationRotated(next) = rotated.outcome else {
            anyhow::bail!("wrong rotation receipt");
        };
        ensure!(
            next.key_generation == revision.key_generation + 1
                && next.config_generation == revision.config_generation
        );
        let rotated = success(
            &world,
            &mut provider,
            0,
            "after-rotation",
            Some(&peer),
            (1, SECRETS[0], 1),
        )
        .await?;

        let before = world.operators[0].credential("fixture", "tenant").await?;
        let changed = apply(
            &world,
            0,
            "native-replace-credential",
            admin::Command::ImportCredential(admin::CredentialImport {
                provider: "fixture".into(),
                account: "tenant".into(),
                resource_pool_id: TENANT_POOL.into(),
                expected_version: Some(before.version),
                credential: admin::CredentialInput::ApiKey {
                    value: REPLACEMENT.into(),
                },
            }),
        )
        .await?;
        let admin::Outcome::Credential(credential) = changed.outcome else {
            anyhow::bail!("wrong credential receipt");
        };
        ensure!(credential.version == before.version + 1);
        for replica in 0..2 {
            ensure!(
                world.operators[replica]
                    .credential("fixture", "tenant")
                    .await?
                    == credential
            );
            denied(
                &world,
                &provider,
                replica,
                0,
                &rotated.public_id,
                "old-credential",
                "target is unavailable or changed",
            )
            .await?;
        }
        let fresh = success(
            &world,
            &mut provider,
            1,
            "new-credential",
            None,
            (1, REPLACEMENT, 1),
        )
        .await?;
        let continued = success(
            &world,
            &mut provider,
            0,
            "new-credential-peer",
            Some(&fresh),
            (1, REPLACEMENT, 1),
        )
        .await?;
        ensure!(
            fresh.execution.attempts[0].credential_revision == credential.version
                && continued.execution.attempts[0].credential_revision == credential.version
        );

        let mut publication: admin::Publication = serde_json::from_value(
            native_plan(&provider.base, &world.keys)["configuration"].clone(),
        )?;
        publication.routing["providers"]["tenant"]["api_base"] =
            json!(format!("{}/v2", provider.base));
        let published = apply(
            &world,
            1,
            "native-change-endpoint",
            admin::Command::Publish(admin::PublicationPut {
                expected_generation: next.config_generation,
                configuration: publication,
            }),
        )
        .await?;
        let admin::Outcome::Published(next) = published.outcome else {
            anyhow::bail!("wrong publication receipt");
        };
        ensure!(next.config_generation == 2 && next.key_generation == 2);
        // A successfully loaded new generation is the positive control for
        // endpoint invalidation; do not accept readiness loss as the rejection.
        let moved = success(
            &world,
            &mut provider,
            0,
            "new-endpoint",
            None,
            (2, REPLACEMENT, 2),
        )
        .await?;
        for replica in 0..2 {
            denied(
                &world,
                &provider,
                replica,
                0,
                &continued.public_id,
                "old-endpoint",
                "target is unavailable or changed",
            )
            .await?;
        }
        success(
            &world,
            &mut provider,
            1,
            "new-endpoint-peer",
            Some(&moved),
            (2, REPLACEMENT, 2),
        )
        .await?;
        for replica in 0..2 {
            let old = world
                .status(replica, 1, &original.execution.execution_id)
                .await?;
            old.expect_status(StatusCode::OK)?;
            same_facts(&old.execution()?, &original.execution)?;
            ensure!(
                fixture
                    .child(world.children[replica])?
                    .child
                    .try_wait()?
                    .is_none()
            );
        }
        ensure!(provider.calls() == 7);
        fixture.no_local_files()
    })
    .await
    .context("native continuation process campaign deadline exceeded")
    .and_then(|result| result);
    let completed_campaign = result.is_ok();
    // Both owners survive cancellation of the campaign. Leave the provider up
    // through stock pipeline/native drain, then join its driver AND connections.
    let result = finish_children(
        &mut fixture.children,
        result,
        expected,
        &failure_log_prefix(),
    )
    .await;
    let joined = provider.finish().await;
    let count = if !completed_campaign || provider.calls() == 7 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "late native dispatch: {} calls",
            provider.calls()
        ))
    };
    combine_results(result, vec![joined, count])
}

struct Retained {
    public_id: String,
    native_id: String,
    execution: GatewayExecution,
}

fn post(
    w: &World,
    replica: usize,
    principal: usize,
    label: &str,
    previous: Option<&str>,
) -> reqwest::RequestBuilder {
    // A suffix-only turn requires a real native pin. Supplying the entire
    // visible prefix could legitimately detach and would not prove this path.
    let mut body = json!({"model":"tenant:mock","stream":false,"input":label});
    if let Some(previous) = previous {
        body["previous_response_id"] = json!(previous);
    }
    w.http
        .post(format!("http://{}/v1/responses", w.replicas[replica]))
        .bearer_auth(&w.keys[principal].secret)
        .header("idempotency-key", label)
        .header("x-bitrouter-request-id", label)
        .json(&body)
}

async fn success(
    w: &World,
    provider: &mut Responses,
    replica: usize,
    label: &str,
    previous: Option<&Retained>,
    route: (u64, &str, u64),
) -> Result<Retained> {
    let (endpoint, credential, generation) = route;
    let calls = provider.calls();
    // Publication/key rotation can briefly reject a stale serving snapshot.
    // Retry only this explicitly new, definitely unadmitted 503; never replay
    // an uncertain admission or provider attempt.
    tokio::time::timeout(Duration::from_secs(10), async {
        for probe in 0..64 {
            let tag = format!("native-{label}-{replica}-{probe}");
            let wire = Wire::read(
                post(
                    w,
                    replica,
                    replica,
                    &tag,
                    previous.map(|prior| prior.public_id.as_str()),
                )
                .send()
                .await?,
            )
            .await?;
            if wire.status == StatusCode::SERVICE_UNAVAILABLE {
                ensure!(
                    !wire.headers.contains_key("x-bitrouter-execution-id")
                        && provider.calls() == calls,
                    "cannot retry an admitted or dispatched continuation request"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            wire.expect_status(StatusCode::OK)?;
            let captured = provider.arrival().await?;
            ensure!(captured.path == format!("/v{endpoint}/responses"));
            ensure!(captured.authorization == format!("Bearer {credential}"));
            ensure!(captured.body["model"] == "mock" && captured.body["stream"] == false);
            ensure!(
                captured.body["input"]
                    .as_array()
                    .is_some_and(|items| items.len() == 1),
                "native continuation unexpectedly replayed history"
            );
            ensure!(captured.body["input"][0]["role"] == "user");
            ensure!(captured.body["input"][0]["content"][0]["text"] == tag);
            ensure!(
                captured
                    .body
                    .get("previous_response_id")
                    .and_then(Value::as_str)
                    == previous.map(|prior| prior.native_id.as_str()),
                "wrong native provider parent"
            );
            wire.identity(&captured.execution, &tag)?;
            let body: Value = serde_json::from_slice(&wire.body)?;
            let public_id = body["id"]
                .as_str()
                .context("missing public Responses ID")?
                .to_owned();
            ensure!(body["status"] == "completed" && public_id != captured.native_id);
            ensure!(
                !String::from_utf8_lossy(&wire.body).contains(&captured.native_id),
                "native ID leaked in response"
            );
            let execution = w
                .wait_state(1 - replica, 1, &captured.execution, succeeded)
                .await?;
            ensure!(execution.config_generation == generation && execution.attempts.len() == 1);
            ensure!(provider.calls() == calls + 1);
            return Ok(Retained {
                public_id,
                native_id: captured.native_id,
                execution,
            });
        }
        anyhow::bail!("native request did not converge after publication");
    })
    .await
    .context("native success deadline exceeded")?
}

async fn denied(
    w: &World,
    provider: &Responses,
    replica: usize,
    principal: usize,
    previous: &str,
    label: &str,
    reason: &str,
) -> Result<()> {
    let calls = provider.calls();
    tokio::time::timeout(Duration::from_secs(10), async {
        for probe in 0..64 {
            let tag = format!("native-denied-{label}-{replica}-{probe}");
            let wire = Wire::read(post(w, replica, principal, &tag, Some(previous)).send().await?).await?;
            ensure!(provider.calls() == calls, "rejected continuation dispatched upstream");
            if wire.status == StatusCode::SERVICE_UNAVAILABLE {
                ensure!(!wire.headers.contains_key("x-bitrouter-execution-id"), "unexpected uncertain admission");
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            wire.expect_status(StatusCode::BAD_REQUEST)?;
            ensure!(String::from_utf8_lossy(&wire.body).contains(reason), "wrong native rejection: {}", String::from_utf8_lossy(&wire.body));
            let id = wire.header("x-bitrouter-execution-id")?;
            wire.identity(id, &tag)?;
            let view = w.wait_state(1 - replica, principal, id, |state| {
                matches!(state, ExecutionState::Terminal { result } if matches!(result.outcome, TerminalOutcome::Failed { .. }))
            }).await?;
            ensure!(view.attempts.is_empty(), "continuation rejected after reserving an attempt");
            return Ok(());
        }
        anyhow::bail!("never observed definitive continuation rejection");
    }).await.context("native rejection deadline exceeded")?
}

async fn apply(
    w: &World,
    replica: usize,
    id: &str,
    command: admin::Command,
) -> Result<admin::Receipt> {
    let receipt = w.operators[replica]
        .apply(&Mutation {
            operation_id: id.into(),
            command,
        })
        .await?;
    ensure!(matches!(w.operators[1-replica].operation(id).await?,
        OperationLookup::Committed { receipt: other } if *other == receipt));
    Ok(receipt)
}

fn native_plan(base: &str, keys: &[crate::auth::keys::GeneratedKey; 4]) -> Value {
    let mut plan = seed_plan(base, keys);
    plan["configuration"]["routing"]["providers"]["tenant"]["api_base"] =
        json!(format!("{base}/v1"));
    plan["configuration"]["routing"]["providers"]["tenant"]["api_protocol"] =
        json!([{"*":"responses"}]);
    plan
}

async fn start_world(
    fixture: &mut Fixture,
    base: &str,
    expected: &mut ExitExpectations,
) -> Result<World> {
    let keys = std::array::from_fn(|_| crate::auth::keys::generate());
    seed(fixture, &native_plan(base, &keys)).await?;
    let mut replicas = Vec::new();
    let mut children = Vec::new();
    let mut operators = Vec::new();
    for _ in 0..2 {
        let inference = available_address()?;
        let operator = loop {
            let address = available_address()?;
            if address != inference {
                break address;
            }
        };
        let child = spawn(fixture, inference, operator).await?;
        expected.insert(child, ExpectedExit::CleanServe);
        children.push(child);
        fixture.child(child)?.ready().await?;
        replicas.push(inference);
        operators.push(OperatorClient::new(
            &format!("http://{operator}"),
            OPERATOR_TOKEN,
        )?);
    }
    Ok(World {
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(5))
            .build()?,
        replicas: replicas
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing replicas"))?,
        children: children
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing children"))?,
        operators: operators
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing operators"))?,
        keys,
    })
}

async fn spawn(fixture: &mut Fixture, address: SocketAddr, operator: SocketAddr) -> Result<usize> {
    let index = fixture.children.len();
    let config = fixture.root.path().join(format!("gateway-{index}.json"));
    tokio::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "server":{"listen":address.to_string(),"log_level":"info"},
            "database":{"url":"${FIXTURE_SHARED_DATABASE}"},
            "gateway_state":{"mode":"shared","namespace":fixture.authority.store().namespace(),
                "deployment_key_env":"FIXTURE_SHARED_KEY","pool_connections":3,
                "transaction_timeout_ms":1000,"owner_lease_ms":60000,"heartbeat_ms":250,
                "worker_interval_ms":100,"drain_timeout_ms":2000},
            "operator":{"listen":operator.to_string(),"credentials":[{
                "id":"fixture-operator","token_env":"FIXTURE_OPERATOR_TOKEN",
                "grants":["read","rotate_correlation","credential_write","publish"]}]}
        }))?,
    )
    .await?;
    let log = fixture.root.path().join(format!("gateway-{index}.log"));
    let output = std::fs::File::create(&log)?;
    let child = Command::new(&fixture.binary)
        .args(["serve", "--config"])
        .arg(config)
        .current_dir(fixture.root.path().join("launch"))
        .env("FIXTURE_SHARED_DATABASE", &fixture.url)
        .env("FIXTURE_SHARED_KEY", STANDARD.encode(fixture.key))
        .env("FIXTURE_OPERATOR_TOKEN", OPERATOR_TOKEN)
        .env(
            "BITROUTER_HOME",
            fixture.root.path().join("unexpected-home"),
        )
        .env("HOME", fixture.root.path().join("unexpected-user-home"))
        .env_remove("RUST_LOG")
        .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output))
        .kill_on_drop(true)
        .spawn()?;
    fixture.children.push(Running {
        child,
        log,
        address,
    });
    Ok(index)
}

struct Captured {
    path: String,
    authorization: String,
    execution: String,
    native_id: String,
    body: Value,
}

#[derive(Clone)]
struct ProviderState {
    sender: mpsc::Sender<Result<Captured>>,
    calls: Arc<AtomicUsize>,
    malformed: Arc<AtomicUsize>,
}

struct Responses {
    base: String,
    state: ProviderState,
    arrivals: mpsc::Receiver<Result<Captured>>,
    transport: Transport,
}

impl Responses {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let (sender, arrivals) = mpsc::channel(32);
        let state = ProviderState {
            sender,
            calls: Arc::default(),
            malformed: Arc::default(),
        };
        let app = Router::new().fallback(handler).with_state(state.clone());
        let config: crate::config::gateway_state::SharedGatewayConfig =
            serde_json::from_value(json!({
                "namespace":"native-fixture", "deployment_key_env":"UNUSED_FIXTURE_KEY",
                "max_connections":16, "max_request_bytes":65536,
                "header_timeout_ms":2000,"request_timeout_ms":2000,"write_stall_timeout_ms":2000
            }))?;
        let transport = Transport::start(listener, app, Limits::from(&config))?;
        Ok(Self {
            base,
            state,
            arrivals,
            transport,
        })
    }

    fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }

    async fn arrival(&mut self) -> Result<Captured> {
        tokio::time::timeout(Duration::from_secs(2), self.arrivals.recv())
            .await
            .context("Responses provider never observed the request")?
            .context("Responses provider arrival channel ended")?
    }

    async fn finish(&self) -> Result<()> {
        let drained = self
            .transport
            .drain_until(tokio::time::Instant::now() + Duration::from_secs(3))
            .await;
        if !self.transport.report().is_quiescent() {
            self.transport.request_force();
            let joined = self.transport.join_forced().await;
            return combine_results(
                Err(anyhow::anyhow!("Responses mock required force")),
                vec![
                    drained.map(|_| ()),
                    joined.and_then(|report| {
                        ensure!(report.is_quiescent(), "Responses mock retained roots");
                        Ok(())
                    }),
                ],
            );
        }
        ensure!(drained?.is_clean(), "Responses mock did not drain cleanly");
        ensure!(
            self.state.malformed.load(Ordering::SeqCst) == 0,
            "mock received malformed provider HTTP"
        );
        Ok(())
    }
}

async fn handler(State(state): State<ProviderState>, request: Request) -> Response {
    let call = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    let result = capture(request, call).await;
    match result {
        Ok(captured) => {
            let id = captured.native_id.clone();
            if state.sender.try_send(Ok(captured)).is_err() {
                state.malformed.fetch_add(1, Ordering::SeqCst);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            // Ordinary non-streaming OpenAI Responses object, deliberately
            // retaining a private provider ID for the next native request.
            // https://platform.openai.com/docs/api-reference/responses/object
            Json(json!({"id":id,"object":"response","created_at":1,
                "model":"mock","status":"completed", "output":[{
                    "id":format!("msg-fixture-{call}"),"type":"message","status":"completed",
                    "role":"assistant","content":[{"type":"output_text","text":"fixture answer","annotations":[]}]
                }],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}
            })).into_response()
        }
        Err(error) => {
            state.malformed.fetch_add(1, Ordering::SeqCst);
            let _ = state.sender.try_send(Err(error));
            StatusCode::BAD_REQUEST.into_response()
        }
    }
}

async fn capture(request: Request, call: usize) -> Result<Captured> {
    let (parts, body) = request.into_parts();
    ensure!(parts.method == Method::POST && parts.uri.query().is_none());
    ensure!(matches!(
        parts.uri.path(),
        "/v1/responses" | "/v2/responses"
    ));
    let header = |name: &str| -> Result<String> {
        ensure!(
            parts.headers.get_all(name).iter().count() == 1,
            "missing/duplicate provider header"
        );
        Ok(parts
            .headers
            .get(name)
            .context("missing provider header")?
            .to_str()?
            .to_owned())
    };
    let authorization = header("authorization")?;
    let execution = header("x-bitrouter-request-id")?;
    uuid::Uuid::parse_str(&execution)?;
    ensure!(header("content-type")? == "application/json");
    let bytes = axum::body::to_bytes(body, 65536).await?;
    let body: Value = serde_json::from_slice(&bytes)?;
    Ok(Captured {
        path: parts.uri.path().to_owned(),
        authorization,
        execution,
        native_id: format!("resp-private-fixture-{call}"),
        body,
    })
}
