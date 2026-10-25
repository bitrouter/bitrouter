//! Real retained policy consumer, atomic terminal closure and custody failures.
use super::*;
use crate::trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy};
use bitrouter_ai::eval::{AdmissionStatus, EvalScope, EvalVerdict};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};
mod capacity;
mod custody;
mod preparation;
mod replay;
#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
mod server;
mod sockets;

const PRIVATE_PROMPT: &str = "trajectory-private-fixture-sentinel";
fn document() -> crate::policy_lock::PolicyLock {
    let mut document = crate::policy_lock::PolicyLock::default();
    document.policies.insert(
        "selected".into(),
        crate::policy_lock::PolicyDefinition {
            tiers: [(
                "economy".into(),
                bitrouter_ai::routing::PolicyModelTarget::model_only("mock:model-a".into()),
            )]
            .into(),
            default_tier: Some("economy".into()),
            progress_guard: Some(guard()),
            ..Default::default()
        },
    );
    document
}
fn guard() -> ProgressGuardPolicy {
    ProgressGuardPolicy {
        escalation_tier: "economy".into(),
        protected_tiers: ["economy".into()].into(),
        max_consecutive_unprotected: None,
        max_same_projection_unprotected: None,
        max_recovery_count: None,
        max_episode_requests: Some(1),
        max_episode_elapsed_ms: None,
        max_episode_cost_micro_usd: None,
        hold_for_requests: 1,
        incomplete_history: IncompleteHistoryAction::Observe,
    }
}
async fn fixture(url: &str, mode: LearningMode) -> Result<(Fixture, RevisionId)> {
    policy_fixture_with_trajectory(url, mode, document(), true, Some(json!({"enabled":true}))).await
}
async fn credential(fixture: &Fixture, revision: &RevisionId) -> Result<::http::HeaderMap> {
    let issued = fixture.issue("trajectory-execution").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    headers(
        fixture,
        issued.secret.as_deref().context("secret")?,
        revision,
    )
}
fn router(fixture: &Fixture) -> Router {
    Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ))
}
async fn send(
    fixture: &Fixture,
    headers: &::http::HeaderMap,
    body: serde_json::Value,
) -> Result<(StatusCode, Vec<u8>)> {
    let mut request =
        Request::post("/v1/chat/completions").body(Body::from(serde_json::to_vec(&body)?))?;
    *request.headers_mut() = headers.clone();
    let response = tokio::time::timeout(TIMEOUT, router(fixture).oneshot(request)).await??;
    let status = response.status();
    let bytes = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 1024 * 1024),
    )
    .await??;
    Ok((status, bytes.to_vec()))
}
async fn count(db: &sea_orm::DatabaseConnection, table: &str, predicate: &str) -> Result<i64> {
    let row = db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            format!("SELECT COUNT(*) AS total FROM {table} WHERE {predicate}"),
        ))
        .await?
        .context("count")?;
    Ok(row.try_get("", "total")?)
}
async fn upstream(fail: bool, stream: bool) -> MockServer {
    let server = MockServer::start().await;
    let response = if fail {
        ResponseTemplate::new(400).set_body_json(
            json!({"error":{"message":"fixture rejected","type":"invalid_request_error"}}),
        )
    } else if stream {
        // https://platform.openai.com/docs/api-reference/chat/streaming
        let first = json!({"id":"trajectory-stream","object":"chat.completion.chunk","created":0,"model":"model-a","choices":[{"index":0,"delta":{"content":"observed"},"finish_reason":null}]});
        let last = json!({"id":"trajectory-stream","object":"chat.completion.chunk","created":0,"model":"model-a","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}});
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"))
    } else {
        // https://platform.openai.com/docs/api-reference/chat/object
        ResponseTemplate::new(200).set_body_json(json!({"id":"trajectory-response","object":"chat.completion","created":0,"model":"model-a","choices":[{"index":0,"message":{"role":"assistant","content":"observed"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}))
    };
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn retained_guard_settles_off_observe_explore_error_and_stream() -> Result<()> {
    for (mode, fail, stream) in [
        (LearningMode::Off, false, false),
        (LearningMode::Observe, false, false),
        (LearningMode::Explore, false, false),
        (LearningMode::Observe, true, false),
        (LearningMode::Observe, false, true),
    ] {
        let server = upstream(fail, stream).await;
        let (fixture, revision) = fixture(&format!("{}/v1", server.uri()), mode).await?;
        let headers = credential(&fixture, &revision).await?;
        let execution = admitted(&fixture, &headers).await?;
        let eval = execution.eval_store();
        drop(execution);
        let (status,bytes)=send(&fixture,&headers,json!({"model":MODEL,"stream":stream,"messages":[{"role":"user","content":PRIVATE_PROMPT}]})).await?;
        ensure!(
            status.is_success() != fail,
            "unexpected response {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        fixture.service.pipelines.close_and_drain().await?;
        ensure!(server.received_requests().await.context("requests")?.len() == 1);
        ensure!(count(&fixture.db, "trajectory_requests", "status <> 'started'").await? == 1);
        ensure!(count(&fixture.db, "trajectory_events", "1=1").await? >= 3);
        let observed = i64::from(mode != LearningMode::Off);
        for table in [
            "trajectory_outbox",
            "eval_subjects",
            "eval_results",
            "eval_admission_events",
        ] {
            ensure!(
                count(&fixture.db, table, "1=1").await? == observed,
                "unexpected {table}"
            );
        }
        ensure!(count(&fixture.db, "trajectory_outbox", "delivered_at IS NULL").await? == 0);
        let subjects = eval.list_subjects().await?;
        for subject in &subjects {
            ensure!(
                subject.scope == EvalScope::Episode && subject.eval_id.starts_with("trajectory:")
            );
            let results = eval.results_for_subject(&subject.eval_id).await?;
            ensure!(results.len() == 1 && results[0].result.verdict == EvalVerdict::Inconclusive);
            ensure!(results[0].result.evaluator.authority_id == "bitrouter.builtin");
            ensure!(!results[0].result.metrics.contains_key("quality.pass"));
            ensure!(
                eval.latest_admissions()
                    .await?
                    .get(&results[0].result_id)
                    .context("admission")?
                    .status
                    == AdmissionStatus::Admitted
            );
        }
        ensure!(
            crate::core::eval::store::EvalStore::new(fixture.db.clone())
                .list_subjects()
                .await?
                .is_empty()
        );
        let rows=fixture.db.query_all(Statement::from_string(fixture.db.get_database_backend(),"SELECT event_json,installation_id,tenant_id,creator_principal_id FROM trajectory_events".to_owned())).await?;
        for row in rows {
            ensure!(
                !row.try_get::<String>("", "event_json")?
                    .contains(PRIVATE_PROMPT)
            );
            ensure!(row.try_get::<String>("", "installation_id")? == fixture.installation.as_str());
            ensure!(row.try_get::<String>("", "tenant_id")? == fixture.tenant.as_str());
            ensure!(
                row.try_get::<String>("", "creator_principal_id")? == fixture.principal.as_str()
            );
        }
        fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn failed_atomic_publication_rolls_back_every_terminal_stage_and_retains_custody()
-> Result<()> {
    for (index, trigger) in [
        "BEFORE INSERT ON trajectory_outbox",
        "BEFORE INSERT ON eval_subjects",
        "BEFORE INSERT ON eval_results",
        "BEFORE INSERT ON eval_admission_events",
        "BEFORE UPDATE ON trajectory_outbox WHEN NEW.delivered_at IS NOT NULL",
    ]
    .into_iter()
    .enumerate()
    {
        let server = upstream(false, false).await;
        let (fixture, revision) =
            fixture(&format!("{}/v1", server.uri()), LearningMode::Observe).await?;
        let headers = credential(&fixture, &revision).await?;
        fixture.db.execute_unprepared(&format!("CREATE TRIGGER trajectory_atomic_failure_{index} {trigger} BEGIN SELECT RAISE(ABORT, 'fixture terminal failure'); END")).await?;
        // Transport may already have produced output. Required custody, not an
        // HTTP status or provider receipt, determines whether drain can succeed.
        let _response = send(
            &fixture,
            &headers,
            json!({"model":MODEL,"messages":[{"role":"user","content":PRIVATE_PROMPT}]}),
        )
        .await;
        ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
        ensure!(
            fixture.service.cleanup.disposition()
                == crate::tenancy::transaction::Disposition::RolledBack
        );
        ensure!(fixture.service.current().is_err());
        ensure!(server.received_requests().await.context("requests")?.len() == 1);
        ensure!(count(&fixture.db, "trajectory_requests", "status = 'started'").await? == 1);
        ensure!(
            count(
                &fixture.db,
                "trajectory_requests",
                "settlement_event_id IS NOT NULL"
            )
            .await?
                == 0
        );
        for table in [
            "trajectory_outbox",
            "eval_subjects",
            "eval_results",
            "eval_admission_events",
        ] {
            ensure!(
                count(&fixture.db, table, "1=1").await? == 0,
                "partial terminal write in {table}"
            );
        }
        fixture
            .db
            .execute_unprepared(&format!("DROP TRIGGER trajectory_atomic_failure_{index}"))
            .await?;
        ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
        ensure!(fixture.service.shutdown().await.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn unqualified_parent_rejects_without_start_provider_or_missing_observation() -> Result<()> {
    let server = upstream(false, false).await;
    let (fixture, revision) =
        fixture(&format!("{}/v1", server.uri()), LearningMode::Observe).await?;
    let headers = credential(&fixture, &revision).await?;
    let (status,_)=send(&fixture,&headers,json!({"model":MODEL,"previous_response_id":"unqualified-parent","messages":[{"role":"user","content":PRIVATE_PROMPT}]})).await?;
    ensure!(!status.is_success());
    fixture.service.pipelines.close_and_drain().await?;
    ensure!(fixture.service.current().is_ok());
    ensure!(
        server
            .received_requests()
            .await
            .context("requests")?
            .is_empty()
    );
    for table in ["trajectory_requests", "trajectory_events", "eval_subjects"] {
        ensure!(count(&fixture.db, table, "1=1").await? == 0);
    }
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn same_retained_profile_correlates_across_restart_and_creators_without_source_import()
-> Result<()> {
    use bitrouter_ai::tenancy::authority::{BindingPut, PrincipalPut, TenantRole};
    let server = upstream(false, false).await;
    let (mut fixture, revision) =
        fixture(&format!("{}/v1", server.uri()), LearningMode::Observe).await?;
    let first_creator = fixture.principal.clone();
    let first = credential(&fixture, &revision).await?;
    let first_body = json!({"model":MODEL,"messages":[{"role":"user","content":PRIVATE_PROMPT}]});
    ensure!(send(&fixture, &first, first_body).await?.0.is_success());
    fixture.service.shutdown().await?;
    // Reopen retained definitions with only the explicit key, no import source.
    let resources =
        crate::tenancy::resources::Resources::with_key(fixture.installation.clone(), [29; 32])?;
    let snapshot = crate::tenancy::store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        crate::tenancy::actor::OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    let principal: bitrouter_ai::tenancy::identity::PrincipalId = "second-principal".try_into()?;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Principal(
                    principal.clone(),
                    PrincipalPut {
                        operation_id: "second-principal".try_into()?,
                        expected_revision: None,
                        issuer: "test".into(),
                        subject: "second-subject".into(),
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let binding: bitrouter_ai::tenancy::identity::BindingId = "second-binding".try_into()?;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    binding.clone(),
                    BindingPut {
                        operation_id: "second-binding".try_into()?,
                        expected_revision: None,
                        principal_id: principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: crate::tenancy::mutation::tenant_scope(),
                        expires_at: expiry()?,
                        expected_management_owner: None,
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    fixture.principal = principal.clone();
    fixture.binding = binding;
    let issued = fixture.issue("second-key").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let second = headers(
        &fixture,
        issued.secret.as_deref().context("second secret")?,
        &revision,
    )?;
    ensure!(send(&fixture,&second,json!({"model":MODEL,"messages":[{"role":"user","content":PRIVATE_PROMPT},{"role":"assistant","content":"observed"},{"role":"user","content":"continue"}]})).await?.0.is_success());
    fixture.service.pipelines.close_and_drain().await?;
    ensure!(count(&fixture.db, "trajectory_episodes", "1=1").await? == 1);
    ensure!(count(&fixture.db, "trajectory_requests", "status = 'settled'").await? == 2);
    ensure!(count(&fixture.db, "eval_subjects", "1=1").await? == 2);
    let creators = fixture
        .db
        .query_all(Statement::from_string(
            fixture.db.get_database_backend(),
            "SELECT creator_principal_id FROM trajectory_requests".to_owned(),
        ))
        .await?
        .into_iter()
        .map(|row| row.try_get::<String>("", "creator_principal_id"))
        .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()?;
    ensure!(creators == [first_creator.to_string(), principal.to_string()].into());
    let episode = fixture
        .db
        .query_one(Statement::from_string(
            fixture.db.get_database_backend(),
            "SELECT creator_principal_id FROM trajectory_episodes".to_owned(),
        ))
        .await?
        .context("episode creator")?;
    ensure!(episode.try_get::<String>("", "creator_principal_id")? == first_creator.as_str());
    ensure!(server.received_requests().await.context("requests")?.len() == 2);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_rejection_after_guard_selection_still_settles_operational_obligation() -> Result<()>
{
    use crate::gateway::{
        assembly::App,
        caller::CallerContext,
        error::{BitrouterError, Result as GatewayResult},
        pipeline::native::{NativeExecutionControl, NativePlan, NativePlanAdmission},
    };
    use bitrouter_ai::{
        core::evidence::NativeAttemptReport,
        types::{Message, Prompt, Role},
    };
    struct RejectPlan(AtomicUsize);
    #[async_trait::async_trait]
    impl NativeExecutionControl for RejectPlan {
        async fn plan(&self, _: NativePlan) -> GatewayResult<NativePlanAdmission> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(BitrouterError::bad_request(
                "fixture stops after guarded plan",
            ))
        }
        async fn before_attempt(&self, _: &str, _: u32) -> GatewayResult<()> {
            Err(BitrouterError::bad_request(
                "fixture never permits provider work",
            ))
        }
        async fn after_attempt(&self, _: NativeAttemptReport) {}
    }
    for mode in [LearningMode::Off, LearningMode::Observe] {
        let server = upstream(false, false).await;
        let (fixture, revision) = fixture(&format!("{}/v1", server.uri()), mode).await?;
        let headers = credential(&fixture, &revision).await?;
        let config = crate::config::parse_resolved("inherit_defaults: false")?;
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(
                        crate::config::routing_table::ConfigRoutingTable::from_config(config),
                    ))
                    .executor(fixture.service.executor.clone());
            })
            .build()?
            .with_tenancy(fixture.service.clone());
        let control = Arc::new(RejectPlan(AtomicUsize::new(0)));
        let prompt = Prompt {
            model: MODEL.into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, PRIVATE_PROMPT)],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        ensure!(
            tokio::time::timeout(
                TIMEOUT,
                app.execute_native_controlled_with_headers(
                    prompt,
                    CallerContext::local(),
                    headers,
                    control.clone()
                )
            )
            .await?
            .is_err()
        );
        ensure!(control.0.load(Ordering::SeqCst) == 1);
        fixture.service.pipelines.close_and_drain().await?;
        ensure!(count(&fixture.db, "trajectory_requests", "status = 'failed'").await? == 1);
        ensure!(
            count(&fixture.db, "eval_subjects", "1=1").await?
                == i64::from(mode != LearningMode::Off)
        );
        ensure!(
            server
                .received_requests()
                .await
                .context("requests")?
                .is_empty()
        );
        ensure!(fixture.service.current().is_ok());
        fixture.service.shutdown().await?;
    }
    Ok(())
}
