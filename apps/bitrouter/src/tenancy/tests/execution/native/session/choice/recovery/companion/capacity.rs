//! Capacity is reached by real admitted sends and native journals, not fixture
//! INSERTs or a manufactured receipt inventory.
use super::*;
use axum::{extract::State, response::IntoResponse};
use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKind};
use bitrouter_ai::tenancy::execution::ReceiptState;
use std::sync::atomic::AtomicUsize;

const UNKNOWN_MODEL: &str = "recovery-unknown";

struct UnknownWire {
    base: String,
    headers: HeaderMap,
    calls: Arc<AtomicUsize>,
    stop: CancellationToken,
    provider: AbortOnDropHandle<std::io::Result<()>>,
    gateway: AbortOnDropHandle<std::io::Result<()>>,
}
impl UnknownWire {
    fn client() -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .no_proxy()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .build()?)
    }
    async fn request(&self, client: &reqwest::Client) -> Result<reqwest::Response> {
        Ok(client
            .post(format!("{}/v1/chat/completions", self.base))
            .headers(self.headers.clone())
            .json(&json!({"model":UNKNOWN_MODEL,
                "messages":[{"role":"user","content":"observe incomplete provider stream"}],
                "stream":true,"max_tokens":25}))
            .send()
            .await?)
    }
    async fn start(bound: &Bound) -> Result<Self> {
        async fn reply(
            State(calls): State<Arc<AtomicUsize>>,
            headers: HeaderMap,
            Json(input): Json<Value>,
        ) -> Response {
            if input["model"] != "model-a"
                || input["stream"] != true
                || headers
                    .get(http::header::AUTHORIZATION)
                    .is_none_or(|v| v != "Bearer owned-upstream-secret")
            {
                return StatusCode::BAD_REQUEST.into_response();
            }
            calls.fetch_add(1, Ordering::SeqCst);
            // A genuine truncated SSE body with observed usage and no terminal
            // event or [DONE]. The canonical decoder must produce Unknown.
            let body = concat!(
                "data: {\"id\":\"recovery-stream\",\"object\":\"chat.completion.chunk\",\"model\":\"model-a\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"recovery-stream\",\"object\":\"chat.completion.chunk\",\"model\":\"model-a\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n"
            );
            ([("content-type", "text/event-stream")], body).into_response()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let upstream = format!("http://{}/v1", listener.local_addr()?);
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/v1/chat/completions", post(reply))
            .with_state(calls.clone());
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let provider = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        }));
        let reference = |kind, name: &str| -> Result<ResourceRef> {
            Ok(ResourceRef {
                owner_kind: OwnerKind::Tenant,
                owner_id: bound.fixture.tenant.as_str().try_into()?,
                resource_kind: kind,
                resource_id: name.try_into()?,
                revision_id: "choice-r1".try_into()?,
            })
        };
        let route = reference(ResourceKind::Route, UNKNOWN_MODEL)?;
        let credential = reference(ResourceKind::Credential, "generation-key")?;
        bound.fixture.service.resources.provision_bytes(&bound.fixture.db,
            &serde_json::to_vec(&json!({"format":1,"installation_id":bound.fixture.installation,
                "resources":[{"reference":route,"definition":{
                    "kind":"route","selector":"bitrouter/owned","policy":null,
                    "routing":{"providers":{"mock":{"api_base":upstream,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"}]}},
                        "routers":{"owned":{"selection":{"kind":"model","model":"mock:model-a"}}}},
                    "credentials":[{"provider":"mock","account":"default","credential":credential}]
                }}]}))?).await?;
        let mut config = bound
            .fixture
            .configuration_input("unknown-capacity-config")?;
        config.document.routes = vec![route];
        let revision = match bound
            .fixture
            .service
            .execute(
                bound.fixture.operator.clone(),
                Command::Configuration(bound.fixture.tenant.clone(), config),
            )
            .await
            .map_err(failure)?
        {
            crate::tenancy::store::StoredResponse::Configuration(value) => value.revision_id,
            _ => anyhow::bail!("unknown-capacity configuration response"),
        };
        let headers = crate::tenancy::tests::execution::headers(
            &bound.fixture,
            bound.key.secret.as_deref().context("real credential")?,
            &revision,
        )?;
        let gateway_router = Router::new()
            .route(
                "/v1/chat/completions",
                post(|| async { StatusCode::NOT_FOUND }),
            )
            .layer(axum::middleware::from_fn_with_state(
                bound.fixture.service.clone(),
                crate::tenancy::http::dispatch,
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let shutdown = stop.clone();
        let gateway = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, gateway_router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        }));
        Ok(Self {
            base,
            headers,
            calls,
            stop,
            provider,
            gateway,
        })
    }
    async fn populate(&self, bound: &Bound, count: usize) -> Result<BTreeSet<String>> {
        let client = Self::client()?;
        let mut ids = BTreeSet::new();
        for _ in 0..count {
            let response = self.request(&client).await?;
            ensure!(response.status() == StatusCode::OK);
            let id = response
                .headers()
                .get("x-bitrouter-request-id")
                .context("actual admitted execution header")?
                .to_str()?
                .to_owned();
            ensure!(ids.insert(id.clone()));
            let body = response.bytes().await?;
            ensure!(!body.is_empty());
            tokio::time::timeout(TIMEOUT, bound.fixture.service.admission.wait_drained()).await?;
            let saved = receipt(bound, &id).await?;
            ensure!(
                saved.state == ReceiptState::Unknown
                    && saved.attempts.len() == 1
                    && saved.attempts[0].outcome == Outcome::Unknown
                    && !saved.usage.complete
                    && saved.usage.input_tokens == Some(5)
                    && saved.usage.output_tokens == Some(2)
            );
        }
        ensure!(self.calls.load(Ordering::SeqCst) == count);
        Ok(ids)
    }
    async fn close(self) -> Result<()> {
        self.stop.cancel();
        let (provider, gateway) = tokio::join!(
            tokio::time::timeout(TIMEOUT, self.provider),
            tokio::time::timeout(TIMEOUT, self.gateway)
        );
        provider???;
        gateway???;
        Ok(())
    }
}

#[tokio::test]
#[ignore = "actual protected repair and ordinary Unknown refusal; select this parent"]
async fn native_recovery_repairs_above_unknown_policy_without_reopening_admission() -> Result<()> {
    campaign(&["capacity-unknown-fit", "capacity-unknown-over-policy"]).await
}

pub(super) async fn unknown(root: &Path, mode: &str) -> Result<()> {
    eprintln!("native-capacity-phase=producer");
    let stopped = Stopped::produce(root, mode).await?;
    eprintln!("native-capacity-phase=wire_setup");
    let wire = UnknownWire::start(&stopped.bound).await?;
    let result = async {
        let count = if mode == "capacity-unknown-fit" {
            98
        } else {
            99
        };
        eprintln!("native-capacity-phase=populate");
        let ids = wire.populate(&stopped.bound, count).await?;
        let before = terminals(&stopped.bound.fixture.db).await?;
        ensure!(
            before.len() == count
                && before.iter().map(|r| r.0.clone()).collect::<BTreeSet<_>>() == ids
        );
        let journal = fixture::journal(&stopped.bound).await?;
        let mut native_ids = BTreeSet::new();
        for link in journal["owned_choices"]
            .as_object()
            .context("real native choices")?
            .values()
        {
            for field in ["parent_execution", "child_execution"] {
                native_ids.insert(
                    link[field]
                        .as_str()
                        .context("native receipt identity")?
                        .to_owned(),
                );
            }
        }
        ensure!(native_ids.len() == 2 && native_ids.is_disjoint(&ids));
        eprintln!("native-capacity-phase=repair_open");
        let session = fixture::open(
            &stopped.bound,
            &stopped.generation,
            &root.join("workspace"),
            Arc::new(HostSpies::default()),
        )
        .await?;
        let checked = async {
            eprintln!("native-capacity-phase=settled_receipts");
            let all = terminals(&stopped.bound.fixture.db).await?;
            ensure!(all.len() == count + 2 && before.iter().all(|row| all.contains(row)));
            let added: BTreeSet<_> = all
                .iter()
                .filter(|row| !ids.contains(&row.0))
                .map(|row| row.0.clone())
                .collect();
            ensure!(added == native_ids);
            for (id, body, _) in all.iter().filter(|row| native_ids.contains(&row.0)) {
                let value: ExecutionReceipt = serde_json::from_str(body)?;
                ensure!(
                    value.execution_id.as_str() == id
                        && value.state == ReceiptState::Unknown
                        && value.attempts.is_empty()
                        && value.execution_revision == stopped.bound.revision
                );
            }
            Ok::<_, anyhow::Error>(all)
        }
        .await;
        eprintln!("native-capacity-phase=session_close");
        let closed = session.close().await;
        let all = checked?;
        closed?;
        ensure!(terminals(&stopped.bound.fixture.db).await? == all);
        tokio::time::timeout(TIMEOUT, async {
            while stopped.bound.fixture.service.host_slots_for_test() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        eprintln!("native-capacity-phase=settled_accounting");
        let accounting = stopped
            .bound
            .fixture
            .service
            .admission
            .receipt_capacity_for_test()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        ensure!(accounting.0 == 0 && !accounting.2);
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        {
            let observed = tokio::time::timeout(TIMEOUT, async {
                loop {
                    match stopped.bound.fixture.service.admission.observed_capacity() {
                        Ok(value) => break Ok::<_, anyhow::Error>(value),
                        Err(
                            crate::telemetry::receipt_latency::capacity::ReadError::Unavailable,
                        ) => {
                            tokio::task::yield_now().await;
                        }
                        Err(error) => anyhow::bail!("invalid settled repair accounting: {error:?}"),
                    }
                }
            })
            .await??;
            ensure!(observed.accounted > 0 && observed.live_unknown == u64::try_from(count + 2)?);
        }
        // H still owns maintenance transactions here. Stopped::close verifies
        // transaction cleanup after that same actor has drained; a snapshot of
        // its current transaction cannot establish repair completion.
        ensure!(stopped.bound.fixture.service.current().is_ok());
        eprintln!("native-capacity-phase=ordinary_refusal");
        let response = wire.request(&UnknownWire::client()?).await?;
        ensure!(response.status() == StatusCode::TOO_MANY_REQUESTS);
        let rejected: bitrouter_ai::tenancy::operation::Error = response.json().await?;
        ensure!(
            rejected.code == bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded
                && rejected.dispatch_state
                    == bitrouter_ai::tenancy::operation::DispatchState::NotStarted
                && rejected.execution_id.is_none()
        );
        tokio::time::timeout(
            TIMEOUT,
            stopped.bound.fixture.service.admission.wait_drained(),
        )
        .await?;
        ensure!(terminals(&stopped.bound.fixture.db).await? == all);
        ensure!(
            stopped.bound.fixture.service.current().is_ok(),
            "ordinary refusal fenced authority"
        );
        ensure!(
            stopped.generation.calls() == 0
                && stopped.choice.calls() == 1
                && wire.calls.load(Ordering::SeqCst) == count
        );
        Ok(())
    }
    .await;
    eprintln!("native-capacity-phase=shutdown");
    let wire_closed = wire.close().await;
    let closed = stopped.close(result).await;
    closed?;
    wire_closed
}

#[tokio::test]
#[ignore = "actual released history and malformed late peer; select this parent"]
async fn native_recovery_validates_released_peers_without_cumulative_history_limit() -> Result<()> {
    campaign(&[
        "capacity-bytes-control",
        "capacity-bytes-history",
        "capacity-bytes-malformed-peer",
    ])
    .await
}

pub(super) async fn bytes(root: &Path, mode: &str) -> Result<()> {
    let mut stopped = Stopped::produce(root, mode).await?;
    let result = async {
        let history = mode == "capacity-bytes-history";
        let malformed = mode == "capacity-bytes-malformed-peer";
        let app = Arc::new(host_app(&stopped.bound.fixture, &stopped.generation.url,
            Arc::new(HostSpies::default()))?.with_tenancy(stopped.bound.fixture.service.clone()));
        let mut peers = 0usize;
        loop {
            let name = format!("released-peer-{peers}");
            let workspace = root.join(&name); std::fs::create_dir(&workspace)?;
            let mut session = NativeSession::open_authenticated(app.clone(), stopped.bound.headers.clone(),
                &name, &workspace, ToolMode::ReadOnly, &HarnessConfig::default()).await?;
            if history {
                let mut task = input();
                task.text = "bounded retained input ".repeat(24_000);
                task.context_limit_bytes = Some(2 * 1024 * 1024);
                let state = session.run(task, Arc::new(Approval), CancellationToken::new()).await?;
                ensure!(state.run.as_ref().is_some_and(|run| run.status == RunStatus::Completed));
                stopped.generation.receive().await?;
            }
            session.close().await?; peers += 1;
            let row = stopped.bound.fixture.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS rows, COALESCE(SUM(payload_bytes),0) AS bytes, COALESCE(MAX(payload_bytes),0) AS largest FROM bro_managed_sessions WHERE logical_name<>'choice-session'"))
                .await?.context("real released peer inventory")?;
            let bytes = u64::try_from(row.try_get::<i64>("", "bytes")?)?;
            let largest = u64::try_from(row.try_get::<i64>("", "largest")?)?;
            ensure!(row.try_get::<i64>("", "rows")? == i64::try_from(peers)?);
            let page_bytes = u64::try_from(peers.min(32))?.checked_mul(1024).context("peer page bytes")?;
            ensure!(largest.checked_mul(3).and_then(|n| n.checked_add(page_bytes))
                .is_some_and(|n| n <= crate::tenancy::session::recovery::MAX_BYTES as u64),
                "each actual peer must fit its own bounded validation");
            if mode == "capacity-bytes-control" || (malformed && peers == 2) || (history && bytes.checked_mul(3)
                .is_some_and(|n| n > crate::tenancy::session::recovery::MAX_BYTES as u64)) { break; }
            ensure!(peers < 32, "actual journals did not reach the explicit cumulative byte boundary");
        }
        if malformed {
            corrupt_last_peer(&stopped.bound).await?;
        }
        let peer_before = peer_digests(&stopped.bound).await?;
        ensure!(peer_before.len() == peers);
        let before = terminals(&stopped.bound.fixture.db).await?;
        let source = payload_for_name(&stopped.bound, "choice-session").await?;
        let sends = stopped.generation.calls();
        let opened = fixture::open(&stopped.bound, &stopped.generation, &root.join("workspace"),
            Arc::new(HostSpies::default())).await;
        if !malformed {
            let session = opened?;
            let checked = async {
                let all = terminals(&stopped.bound.fixture.db).await?;
                ensure!(all.len() == before.len()+2 && before.iter().all(|row| all.contains(row)));
                ensure!(peer_digests(&stopped.bound).await? == peer_before);
                Ok::<_, anyhow::Error>(all)
            }.await;
            let closed = session.close().await;
            let all = checked?;
            closed?;
            ensure!(terminals(&stopped.bound.fixture.db).await? == all);
            if history { ensure!(peers > 1 && sends == peers); }
        } else {
            if let Ok(session) = opened { session.close().await?; anyhow::bail!("recovery ignored malformed late peer checkpoint"); }
            ensure!(payload_for_name(&stopped.bound, "choice-session").await? == source);
            ensure!(terminals(&stopped.bound.fixture.db).await? == before);
            ensure!(peers == 2 && sends == 0);
        }
        ensure!(peer_digests(&stopped.bound).await? == peer_before);
        ensure!(stopped.generation.calls() == sends && stopped.choice.calls() == 1);
        tokio::time::timeout(TIMEOUT, async {
            while stopped.bound.fixture.service.host_slots_for_test() != 1 { tokio::task::yield_now().await; }
        }).await?;
        stopped.bound.fixture.service.cleanup.ensure_clean()?;
        ensure!(stopped.bound.fixture.service.admission.receipt_capacity_for_test()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?.0 == 0);
        Ok(())
    }.await;
    stopped.close(result).await
}
async fn peer_digests(bound: &Bound) -> Result<Vec<(String, i64, String)>> {
    let rows = bound.fixture.db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT id,payload,payload_bytes FROM bro_managed_sessions WHERE logical_name<>'choice-session' ORDER BY id"))
        .await?;
    rows.into_iter()
        .map(|row| {
            let body: String = row.try_get("", "payload")?;
            let bytes: i64 = row.try_get("", "payload_bytes")?;
            ensure!(bytes == i64::try_from(body.len())?);
            Ok((
                row.try_get("", "id")?,
                bytes,
                crate::storage::checkpoint::sha256(body.as_bytes()),
            ))
        })
        .collect()
}
async fn corrupt_last_peer(bound: &Bound) -> Result<()> {
    // This is the last peer in the actual indexed scan order, after a valid
    // released peer. Keep descriptor, release flags and indexed byte length valid.
    let row = bound.fixture.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT id,payload FROM bro_managed_sessions WHERE logical_name<>'choice-session' ORDER BY id DESC LIMIT 1"))
        .await?.context("actual late released peer")?;
    let id: String = row.try_get("", "id")?;
    let body: String = row.try_get("", "payload")?;
    let mut value: Value = serde_json::from_str(&body)?;
    ensure!(value["journal"]["released"] == true && !value["journal"]["ack"].is_null());
    let descriptor = serde_json::from_value(value["descriptor"].clone())?;
    crate::harness::resources::managed::store::recovery::validate_released_owner(
        &serde_json::to_vec(&value["journal"])?,
        &descriptor,
    )?;
    value["journal"]["ack"] = Value::Null;
    ensure!(
        crate::harness::resources::managed::store::recovery::validate_released_owner(
            &serde_json::to_vec(&value["journal"])?,
            &descriptor,
        )
        .is_err()
    );
    let body = serde_json::to_string(&value)?;
    let updated = bound
        .fixture
        .db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE bro_managed_sessions SET payload=?,payload_bytes=? WHERE id=?",
            [
                body.clone().into(),
                i64::try_from(body.len())?.into(),
                id.into(),
            ],
        ))
        .await?;
    ensure!(updated.rows_affected() == 1);
    Ok(())
}
async fn payload_for_name(bound: &Bound, name: &str) -> Result<String> {
    let row = bound.fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT payload FROM bro_managed_sessions WHERE installation_id=? AND tenant_id=? AND session_principal_id=? AND logical_name=?",
        [bound.fixture.installation.as_str().into(),bound.fixture.tenant.as_str().into(),
            bound.fixture.principal.as_str().into(),name.into()])).await?.context("actual owned native row")?;
    Ok(row.try_get("", "payload")?)
}
