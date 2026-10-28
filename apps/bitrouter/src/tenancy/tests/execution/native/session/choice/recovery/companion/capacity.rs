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
        let client = reqwest::Client::builder()
            .no_proxy()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .build()?;
        let mut ids = BTreeSet::new();
        for _ in 0..count {
            let response = client
                .post(format!("{}/v1/chat/completions", self.base))
                .headers(self.headers.clone())
                .json(&json!({"model":UNKNOWN_MODEL,
                    "messages":[{"role":"user","content":"observe incomplete provider stream"}],
                    "stream":true,"max_tokens":25}))
                .send()
                .await?;
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
#[ignore = "actual admitted Unknown receipt limit and native repair; select this parent"]
async fn native_recovery_reserves_aggregate_unknown_room_before_any_repair_write() -> Result<()> {
    campaign(&["capacity-unknown-fit", "capacity-unknown-reject"]).await
}

pub(super) async fn unknown(root: &Path, mode: &str) -> Result<()> {
    let stopped = Stopped::produce(root, mode).await?;
    let wire = UnknownWire::start(&stopped.bound).await?;
    let result = async {
        let count = if mode == "capacity-unknown-fit" {
            98
        } else {
            99
        };
        let ids = wire.populate(&stopped.bound, count).await?;
        let before = terminals(&stopped.bound.fixture.db).await?;
        ensure!(
            before.len() == count
                && before.iter().map(|r| r.0.clone()).collect::<BTreeSet<_>>() == ids
        );
        let native_before = payload(&stopped.bound.fixture.db).await?;
        let opened = fixture::open(
            &stopped.bound,
            &stopped.generation,
            &root.join("workspace"),
            Arc::new(HostSpies::default()),
        )
        .await;
        if mode == "capacity-unknown-fit" {
            let session = opened?;
            let all = terminals(&stopped.bound.fixture.db).await?;
            ensure!(all.len() == 100 && before.iter().all(|row| all.contains(row)));
            for (id, body, _) in all.iter().filter(|row| !ids.contains(&row.0)) {
                let value: ExecutionReceipt = serde_json::from_str(body)?;
                ensure!(
                    value.execution_id.as_str() == id
                        && value.state == ReceiptState::Unknown
                        && value.attempts.is_empty()
                        && value.execution_revision == stopped.bound.revision
                );
            }
            session.close().await?;
            ensure!(terminals(&stopped.bound.fixture.db).await? == all);
        } else {
            if let Ok(session) = opened {
                session.close().await?;
                anyhow::bail!("repair exceeded tenant Unknown limit");
            }
            ensure!(payload(&stopped.bound.fixture.db).await? == native_before);
            ensure!(terminals(&stopped.bound.fixture.db).await? == before);
            ensure!(
                stopped.bound.fixture.service.current().is_ok(),
                "ordinary capacity refusal fenced authority"
            );
        }
        tokio::time::timeout(TIMEOUT, async {
            while stopped.bound.fixture.service.host_slots_for_test() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        ensure!(
            stopped
                .bound
                .fixture
                .service
                .admission
                .receipt_capacity_for_test()
                .map_err(|e| anyhow::anyhow!("{e:?}"))?
                .0
                == 0
        );
        ensure!(
            stopped.generation.calls() == 0
                && stopped.choice.calls() == 1
                && wire.calls.load(Ordering::SeqCst) == count
        );
        Ok(())
    }
    .await;
    let wire_closed = wire.close().await;
    let closed = stopped.close(result).await;
    closed?;
    wire_closed
}

#[tokio::test]
#[ignore = "actual released native journals exercise cumulative recovery bytes; select this parent"]
async fn native_recovery_counts_released_peer_rows_in_one_cumulative_budget() -> Result<()> {
    campaign(&["capacity-bytes-control", "capacity-bytes-reject"]).await
}

pub(super) async fn bytes(root: &Path, mode: &str) -> Result<()> {
    let mut stopped = Stopped::produce(root, mode).await?;
    let result = async {
        let app = Arc::new(host_app(&stopped.bound.fixture, &stopped.generation.url,
            Arc::new(HostSpies::default()))?.with_tenancy(stopped.bound.fixture.service.clone()));
        let mut peers = 0usize;
        loop {
            let name = format!("released-peer-{peers}");
            let workspace = root.join(&name); std::fs::create_dir(&workspace)?;
            let mut session = NativeSession::open_authenticated(app.clone(), stopped.bound.headers.clone(),
                &name, &workspace, ToolMode::ReadOnly, &HarnessConfig::default()).await?;
            if mode == "capacity-bytes-reject" {
                let mut task = input();
                task.text = "bounded retained input ".repeat(24_000);
                task.context_limit_bytes = Some(2 * 1024 * 1024);
                let state = session.run(task, Arc::new(Approval), CancellationToken::new()).await?;
                ensure!(state.run.as_ref().is_some_and(|run| run.status == RunStatus::Completed));
                stopped.generation.receive().await?;
            }
            session.close().await?; peers += 1;
            let row = stopped.bound.fixture.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS rows, COALESCE(SUM(payload_bytes),0) AS bytes FROM bro_managed_sessions"))
                .await?.context("real native inventory")?;
            let bytes = u64::try_from(row.try_get::<i64>("", "bytes")?)?;
            ensure!(row.try_get::<i64>("", "rows")? == i64::try_from(peers+1)?);
            if mode == "capacity-bytes-control" || bytes.checked_mul(3)
                .is_some_and(|n| n > crate::tenancy::session::recovery::MAX_BYTES as u64) { break; }
            ensure!(peers < 32, "actual journals did not reach the explicit cumulative byte boundary");
        }
        let before = terminals(&stopped.bound.fixture.db).await?;
        let source = payload_for_name(&stopped.bound, "choice-session").await?;
        let sends = stopped.generation.calls();
        let opened = fixture::open(&stopped.bound, &stopped.generation, &root.join("workspace"),
            Arc::new(HostSpies::default())).await;
        if mode == "capacity-bytes-control" {
            let session = opened?;
            ensure!(terminals(&stopped.bound.fixture.db).await?.len() == before.len()+2);
            session.close().await?;
        } else {
            if let Ok(session) = opened { session.close().await?; anyhow::bail!("recovery ignored peer-row byte budget"); }
            ensure!(payload_for_name(&stopped.bound, "choice-session").await? == source);
            ensure!(terminals(&stopped.bound.fixture.db).await? == before);
            ensure!(peers > 1 && sends == peers);
        }
        ensure!(stopped.generation.calls() == sends && stopped.choice.calls() == 1);
        tokio::time::timeout(TIMEOUT, async {
            while stopped.bound.fixture.service.host_slots_for_test() != 1 { tokio::task::yield_now().await; }
        }).await?;
        Ok(())
    }.await;
    stopped.close(result).await
}
async fn payload_for_name(bound: &Bound, name: &str) -> Result<String> {
    let row = bound.fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT payload FROM bro_managed_sessions WHERE installation_id=? AND tenant_id=? AND session_principal_id=? AND logical_name=?",
        [bound.fixture.installation.as_str().into(),bound.fixture.tenant.as_str().into(),
            bound.fixture.principal.as_str().into(),name.into()])).await?.context("actual owned native row")?;
    Ok(row.try_get("", "payload")?)
}
