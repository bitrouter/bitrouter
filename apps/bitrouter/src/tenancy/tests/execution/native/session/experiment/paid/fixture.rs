use super::*;
use crate::core::evolution::rubric::RubricEvaluation;
use axum::{Json, Router, extract::State, response::IntoResponse, routing::post};
use std::sync::Mutex;
use tokio::{net::TcpListener, sync::mpsc};

#[derive(Clone)]
struct JudgeState {
    answer: Arc<Mutex<Option<RubricEvaluation>>>,
    release: CancellationToken,
    requests: mpsc::Sender<Value>,
}
async fn judge(
    State(state): State<JudgeState>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if state.requests.try_send(body).is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    state.release.cancelled().await;
    let answer = state
        .answer
        .lock()
        .ok()
        .and_then(|v| v.as_ref().and_then(|a| serde_json::to_string(a).ok()));
    let Some(answer) = answer else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    Json(json!({"id":"actual-native-judge","object":"chat.completion","created":1,"model":"judge-model",
        "choices":[{"index":0,"message":{"role":"assistant","content":answer},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6,
            "prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
            "completion_tokens_details":{"reasoning_tokens":0}}
    })).into_response()
}
pub(super) struct Judge {
    pub url: String,
    pub release: CancellationToken,
    answer: Arc<Mutex<Option<RubricEvaluation>>>,
    requests: mpsc::Receiver<Value>,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}
impl Judge {
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let (send, requests) = mpsc::channel(4);
        let answer = Arc::new(Mutex::new(None));
        let release = CancellationToken::new();
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let router = Router::new()
            .route("/v1/chat/completions", post(judge))
            .with_state(JudgeState {
                answer: answer.clone(),
                release: release.clone(),
                requests: send,
            });
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Ok(Self {
            url,
            release,
            answer,
            requests,
            stop,
            task: Some(task),
        })
    }
    pub fn answer(
        &self,
        view: &crate::harness::resources::managed::session::assessment::NativeRunReview,
    ) -> Result<()> {
        *self
            .answer
            .lock()
            .map_err(|_| anyhow::anyhow!("judge answer poisoned"))? =
            Some(super::super::human::label(view, "judge-grounded-label", true)?.evaluation);
        Ok(())
    }
    pub fn retained_answer(&self, evaluation: RubricEvaluation) -> Result<()> {
        *self
            .answer
            .lock()
            .map_err(|_| anyhow::anyhow!("judge answer poisoned"))? = Some(evaluation);
        Ok(())
    }
    pub async fn received(&mut self) -> Result<Value> {
        tokio::time::timeout(TIMEOUT, self.requests.recv())
            .await?
            .context("actual paid request missing")
    }
    pub fn extra_request(&mut self) -> bool {
        self.requests.try_recv().is_ok()
    }
    pub async fn finish(mut self) -> Result<()> {
        self.release.cancel();
        self.stop.cancel();
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(TIMEOUT, &mut task).await {
                Ok(result) => result??,
                Err(error) => {
                    task.abort();
                    let reaped = tokio::time::timeout(TIMEOUT, task).await;
                    ensure!(reaped.is_ok(), "judge listener task did not terminate");
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }
}
impl Drop for Judge {
    fn drop(&mut self) {
        self.release.cancel();
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub(super) async fn ready(
    directory: &std::path::Path,
    subject: &str,
    judge: &str,
) -> Result<(Fixture, HeaderMap)> {
    let database = crate::db::connect_owned_tenancy(&format!(
        "sqlite://{}",
        directory.join("native-paid.db").display()
    ))
    .await?;
    let mut fixture = Fixture::with_database(
        database.database().clone(),
        OperationLimits::default(),
        &mut |_| {},
    )
    .await?;
    let result = async {
        fixture.service.shutdown().await?;
        let reference = |kind, name: &str| -> Result<ResourceRef> {
            Ok(ResourceRef { owner_kind: bitrouter_ai::tenancy::authority::OwnerKind::Tenant,
                owner_id: fixture.tenant.as_str().try_into()?, resource_kind: kind,
                resource_id: name.try_into()?, revision_id: "native-paid-r1".try_into()? })
        };
        use bitrouter_ai::tenancy::authority::ResourceKind;
        let route = reference(ResourceKind::Route, MODEL)?;
        let evaluator = reference(ResourceKind::Route, "literal:native-judge")?;
        let policy = reference(ResourceKind::Policy, "native-paid-policy")?;
        let subject_key = reference(ResourceKind::Credential, "native-subject-slot")?;
        let judge_key = reference(ResourceKind::Credential, "native-judge-slot")?;
        let resources = Resources::with_key(fixture.installation.clone(), [29;32])?;
        let manifest = json!({"format":1,"installation_id":fixture.installation,"resources":[
            {"reference":policy,"definition":{"kind":"policy","name":"selected","document":super::super::fixture::signed_experiment("selected",1_000_000,4)?}},
            {"reference":route,"definition":{"kind":"route","selector":"bitrouter/observed","policy":policy,
                "assessment":{"profile":"assessment_v1","route":evaluator},
                "routing":{"trajectory":{"enabled":true},"providers":{"mock":{"api_base":subject,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"},{"id":"model-b"}]}},
                    "routers":{"observed":{"selection":{"kind":"policy","policy":"selected","base_model":"mock:model-a"}}}},
                "credentials":[{"provider":"mock","account":"default","credential":subject_key}]}},
            {"reference":evaluator,"definition":{"kind":"route","selector":"judge:judge-model","policy":null,
                "routing":{"providers":{"judge":{"api_base":judge,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"judge-model"}]}}},
                "credentials":[{"provider":"judge","account":"default","credential":judge_key}]}},
            {"reference":subject_key,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}},
            {"reference":judge_key,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"explicit-judge-secret"}}}}
        ]});
        resources.provision_bytes(&fixture.db, &serde_json::to_vec(&manifest)?).await?;
        let (db, owner, claim) = database.claim()?;
        let snapshot = store::load(&db, Utc::now()).await?;
        fixture.service = Service::start_resources_with_continuations(db, Some(owner), OperationLimits::default(), snapshot,
            Arc::new(resources), (None, &crate::config::ContinuationConfig::default(), Some(claim), crate::telemetry::receipt_latency::Sink::default()), None).await?;
        let mut configured = fixture.configuration_input("native-paid-config")?;
        configured.document.routes = vec![route, evaluator]; configured.document.policy = Some(policy);
        configured.document.learning.mode = LearningMode::Explore;
        configured.document.learning.max_evaluations = 1;
        configured.document.limits.active_executions = 2;
        configured.document.limits.learning_jobs = 1;
        let StoredResponse::Configuration(saved) = fixture.service.execute(fixture.operator.clone(), Command::Configuration(fixture.tenant.clone(), configured)).await.map_err(failure)? else { anyhow::bail!("paid configuration reply"); };
        let credential = fixture.issue("native-paid-key").await?;
        fixture.service.dispatch_test_ready.store(true, Ordering::Release);
        crate::tenancy::tests::execution::headers(&fixture, credential.secret.as_deref().context("credential")?, &saved.revision_id)
    }.await;
    match result {
        Ok(headers) => Ok((fixture, headers)),
        Err(error) => {
            let stopped = fixture.service.shutdown().await;
            stopped?;
            Err(error)
        }
    }
}

pub(super) async fn restart(fixture: Fixture, directory: &std::path::Path) -> Result<Fixture> {
    let Fixture {
        service,
        db,
        operator,
        installation,
        tenant,
        principal,
        binding,
        binding_result,
    } = fixture;
    let stopped = tokio::time::timeout(TIMEOUT, service.shutdown()).await;
    stopped??;
    let closed = db.clone().close().await;
    drop(service);
    drop(db);
    closed?;
    let database = crate::db::connect_owned_tenancy(&format!(
        "sqlite://{}",
        directory.join("native-paid.db").display()
    ))
    .await?;
    let (db, owner, claim) = database.claim()?;
    let state = store::load(&db, Utc::now()).await?;
    let service = Service::start_resources_with_continuations(
        db.clone(),
        Some(owner),
        OperationLimits::default(),
        state,
        Arc::new(Resources::with_key(installation.clone(), [29; 32])?),
        (
            None,
            &crate::config::ContinuationConfig::default(),
            Some(claim),
            crate::telemetry::receipt_latency::Sink::default(),
        ),
        None,
    )
    .await?;
    service.dispatch_test_ready.store(true, Ordering::Release);
    Ok(Fixture {
        service,
        db,
        operator,
        installation,
        tenant,
        principal,
        binding,
        binding_result,
    })
}
