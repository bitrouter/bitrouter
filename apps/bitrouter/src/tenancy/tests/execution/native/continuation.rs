//! Real native provider consumers of tenant artifacts; no mock sealing authority.
use super::*;
use crate::gateway::pipeline::{
    native_context::{ORIGIN_FIELD, ORIGIN_NAMESPACE, metadata, metadata_mut},
    native_continuation::{CONTINUATION_FIELD, NativeContinuationSource},
};
use crate::tenancy::store::StoredResponse;
use bitrouter_ai::{
    core::evidence::{NativeContinuationInput, NativeContinuationOutput},
    types::GenerateResult,
};

#[derive(Clone)]
struct Endpoint {
    reply: Arc<std::sync::Mutex<Value>>,
    received: mpsc::Sender<(HeaderMap, Value)>,
}

async fn respond(
    State(state): State<Endpoint>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let streaming = body.get("stream") == Some(&Value::Bool(true));
    if state.received.try_send((headers, body)).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match state.reply.lock() {
        Ok(reply) if streaming => {
            let created = json!({"type":"response.created","response":{"id":reply["id"],"status":"in_progress"}});
            let delta = json!({"type":"response.output_text.delta","delta":"native reply"});
            let terminal = json!({"type":"response.completed","response":reply.clone()});
            (
                [("content-type", "text/event-stream")],
                format!("data: {created}\n\ndata: {delta}\n\ndata: {terminal}\n\n"),
            )
                .into_response()
        }
        Ok(reply) => Json(reply.clone()).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Default)]
struct Observe {
    attempts: Mutex<Vec<NativeAttemptReport>>,
}

#[async_trait]
impl NativeExecutionControl for Observe {
    fn observe_stream(&self) -> bool {
        true
    }
    async fn plan(&self, _plan: NativePlan) -> GatewayResult<NativePlanAdmission> {
        Ok(NativePlanAdmission {
            route_indices: vec![0],
        })
    }
    async fn before_attempt(&self, _id: &str, _index: u32) -> GatewayResult<()> {
        Ok(())
    }
    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.attempts.lock().await.push(report);
    }
}

#[tokio::test]
async fn observed_native_stream_resumes_as_sse_and_json_with_evidence_and_usage() -> Result<()> {
    use sea_orm::ConnectionTrait;
    let mut server = Server::start().await?;
    let (fixture, route, _) = provision_protocol(&server.url, "responses").await?;
    let checked = async {
        let (ingress, _) = ready(&fixture, &route).await?;
        let first_control = Arc::new(Observe::default());
        let first = invoke(
            &fixture,
            &server,
            ingress.clone(),
            prompt(),
            first_control.clone(),
        )
        .await?;
        handle(&first)?;
        ensure!(first.response_id.is_none());
        let usage = first.usage.as_ref().context("first SSE usage")?;
        ensure!(usage.prompt_tokens == 4 && usage.completion_tokens == 2);
        let first_reports = first_control.attempts.lock().await;
        ensure!(first_reports.len() == 1);
        let first_report = first_reports.first().context("first SSE report")?;
        ensure!(matches!(
            first_report.continuation.input,
            NativeContinuationInput::FullHistory {
                reason: bitrouter_ai::core::evidence::FullHistoryReason::NoHandle,
            }
        ));
        ensure!(matches!(
            first_report.continuation.output,
            NativeContinuationOutput::Issued
        ));
        ensure!(
            first_report
                .result
                .as_ref()
                .context("first report result")?
                .usage
                == first.usage
        );
        drop(first_reports);
        ensure!(server.receive().await?.1["stream"] == true);
        let second_control = Arc::new(Observe::default());
        let second = invoke(
            &fixture,
            &server,
            ingress.clone(),
            next(&first),
            second_control.clone(),
        )
        .await?;
        handle(&second)?;
        ensure!(second.usage == first.usage && second.response_id.is_none());
        let second_reports = second_control.attempts.lock().await;
        ensure!(second_reports.len() == 1);
        let second_report = second_reports.first().context("resumed SSE report")?;
        ensure!(matches!(
            second_report.continuation.input,
            NativeContinuationInput::Resumed { prefix_messages: 2 }
        ));
        ensure!(matches!(
            second_report.continuation.output,
            NativeContinuationOutput::Issued
        ));
        ensure!(
            second_report
                .result
                .as_ref()
                .context("resumed SSE result")?
                .usage
                == second.usage
        );
        drop(second_reports);
        let wire = server.receive().await?.1;
        ensure!(wire["stream"] == true);
        ensure!(wire["previous_response_id"] == "resp-owned-private");
        ensure!(wire["input"].as_array().context("suffix")?.len() == 1);
        // The same opaque SSE artifact is independent of the next transport's
        // delivery mode. Its canonical history and authority stay identical.
        let json_control = Arc::new(Control::default());
        let json_result = invoke(
            &fixture,
            &server,
            ingress,
            next(&first),
            json_control.clone(),
        )
        .await?;
        handle(&json_result)?;
        ensure!(json_result.usage == first.usage && json_result.response_id.is_none());
        let reports = json_control.attempts.lock().await;
        ensure!(reports.len() == 1);
        let report = reports.first().context("resumed JSON report")?;
        ensure!(matches!(
            report.continuation.input,
            NativeContinuationInput::Resumed { prefix_messages: 2 }
        ));
        ensure!(matches!(
            report.continuation.output,
            NativeContinuationOutput::Issued
        ));
        ensure!(report.result.as_ref().context("resumed JSON result")?.usage == json_result.usage);
        drop(reports);
        let wire = server.receive().await?.1;
        ensure!(wire.get("stream") != Some(&Value::Bool(true)));
        ensure!(wire["previous_response_id"] == "resp-owned-private");
        ensure!(wire["input"].as_array().context("JSON suffix")?.len() == 1);
        let row = fixture
            .db
            .query_one(sea_orm::Statement::from_string(
                fixture.db.get_database_backend(),
                "SELECT COUNT(*) AS count FROM provider_continuations".to_owned(),
            ))
            .await?
            .context("mapping count")?;
        ensure!(row.try_get::<i64>("", "count")? == 0);
        Ok(())
    }
    .await;
    fixture.service.shutdown().await?;
    server.finish().await?;
    checked
}

struct Server {
    url: String,
    state: Endpoint,
    received: mpsc::Receiver<(HeaderMap, Value)>,
    stop: CancellationToken,
    task: AbortOnDropHandle<std::io::Result<()>>,
}

fn reply() -> Value {
    json!({"id":"resp-owned-private","object":"response","status":"completed","store":true,"model":"model-a",
        "output":[{"id":"message-owned","type":"message","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"native reply","annotations":[]}]}],
        "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}})
}

impl Server {
    async fn start() -> Result<Self> {
        let (received, observed) = mpsc::channel(16);
        let state = Endpoint {
            reply: Arc::new(std::sync::Mutex::new(reply())),
            received,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let router = Router::new()
            .route("/v1/responses", post(respond))
            .with_state(state.clone());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
        }));
        Ok(Self {
            url,
            state,
            received: observed,
            stop,
            task,
        })
    }
    fn set_reply(&self, reply: Value) -> Result<()> {
        *self
            .state
            .reply
            .lock()
            .map_err(|_| anyhow::anyhow!("reply lock"))? = reply;
        Ok(())
    }
    async fn receive(&mut self) -> Result<(HeaderMap, Value)> {
        tokio::time::timeout(TIMEOUT, self.received.recv())
            .await?
            .context("provider request missing")
    }
    async fn finish(self) -> Result<()> {
        self.stop.cancel();
        tokio::time::timeout(TIMEOUT, self.task).await???;
        Ok(())
    }
}

async fn invoke(
    fixture: &Fixture,
    server: &Server,
    ingress: HeaderMap,
    input: Prompt,
    control: Arc<dyn NativeExecutionControl>,
) -> Result<GenerateResult> {
    let app = host_app(fixture, &server.url, Arc::new(HostSpies::default()))?
        .with_tenancy(fixture.service.clone());
    let response = tokio::time::timeout(
        TIMEOUT,
        app.execute_native_controlled_with_headers(
            input,
            CallerContext::new("forged", "forged"),
            ingress,
            control,
        ),
    )
    .await??;
    Ok(response
        .result
        .generation()
        .context("generation missing")?
        .clone())
}

fn next(output: &GenerateResult) -> Prompt {
    let mut next = prompt();
    next.messages.push(Message {
        role: Role::Assistant,
        content: output.content.clone(),
    });
    next.messages.push(Message::text(Role::User, "next"));
    next
}

fn handle(output: &GenerateResult) -> Result<&str> {
    metadata(output.content.first().context("output missing")?)
        .get(ORIGIN_NAMESPACE)
        .and_then(|fields| fields.get(CONTINUATION_FIELD))
        .and_then(Value::as_str)
        .context("owned handle missing")
}

#[tokio::test]
async fn two_turn_resume_survives_caller_rotation_and_restart_without_import_source() -> Result<()>
{
    let mut server = Server::start().await?;
    let (mut fixture, route, _) = provision_protocol(&server.url, "responses").await?;
    let checked = async {
        let (ingress, old) = ready(&fixture, &route).await?;
        let revision = ingress
            .get("Bitrouter-Execution-Revision")
            .context("revision")?
            .to_str()?
            .try_into()?;
        let control = Arc::new(Control::default());
        let output = invoke(&fixture, &server, ingress, prompt(), control.clone()).await?;
        ensure!(output.response_id.is_none() && handle(&output)?.starts_with("btct2."));
        ensure!(!handle(&output)?.contains("resp-owned-private"));
        let public = serde_json::to_string(&output)?;
        ensure!(
            !public.contains("resp-owned-private") && !public.contains("owned-upstream-secret")
        );
        ensure!(matches!(
            control
                .attempts
                .lock()
                .await
                .last()
                .context("attempt")?
                .continuation
                .output,
            NativeContinuationOutput::Issued
        ));
        let (first_headers, first) = server.receive().await?;
        ensure!(first.get("previous_response_id").is_none());
        let mut rotation = fixture.issuance("rotate-native-caller")?;
        rotation.rotate_credential_id = Some(old.credential_id);
        let rotated = fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), rotation),
            )
            .await
            .map_err(failure)?;
        let StoredResponse::Issued(rotated) = rotated else {
            anyhow::bail!("rotation failed");
        };
        fixture.service.shutdown().await?;
        // Fresh resources and cache, same explicit key, no import path/environment.
        let resources = Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?);
        let initial = store::load(&fixture.db, chrono::Utc::now()).await?;
        fixture.service = Service::start_prepared(
            fixture.db.clone(),
            None,
            OperationLimits::default(),
            initial,
            resources,
        )
        .await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let ingress = headers(
            &fixture,
            rotated.secret.as_deref().context("new secret")?,
            &revision,
        )?;
        let observed = Arc::new(Control::default());
        let second = invoke(&fixture, &server, ingress, next(&output), observed.clone()).await?;
        ensure!(handle(&second)?.starts_with("btct2."));
        let (second_headers, second) = server.receive().await?;
        ensure!(second["previous_response_id"] == "resp-owned-private");
        ensure!(second["input"].as_array().context("suffix input")?.len() == 1);
        ensure!(second_headers.get("authorization") == first_headers.get("authorization"));
        ensure!(!serde_json::to_string(&second)?.contains("btct2."));
        ensure!(matches!(
            observed
                .attempts
                .lock()
                .await
                .last()
                .context("second attempt")?
                .continuation
                .input,
            NativeContinuationInput::Resumed { prefix_messages: 2 }
        ));
        Ok(())
    }
    .await;
    fixture.service.shutdown().await?;
    server.finish().await?;
    checked
}

async fn another_principal(
    fixture: &Fixture,
    revision: &bitrouter_ai::tenancy::identity::RevisionId,
) -> Result<HeaderMap> {
    use bitrouter_ai::tenancy::authority::PrincipalPut;
    let principal: bitrouter_ai::tenancy::identity::PrincipalId = "principal-other".try_into()?;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Principal(
                    principal.clone(),
                    PrincipalPut {
                        operation_id: "other-principal".try_into()?,
                        expected_revision: None,
                        issuer: "fixture".into(),
                        subject: "other".into(),
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let binding: bitrouter_ai::tenancy::identity::BindingId = "binding-other".try_into()?;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    binding.clone(),
                    BindingPut {
                        operation_id: "other-binding".try_into()?,
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
    let mut issuance = fixture.issuance("other-credential")?;
    issuance.principal_id = principal;
    issuance.binding_id = binding;
    let StoredResponse::Issued(credential) = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Issue(fixture.tenant.clone(), issuance),
        )
        .await
        .map_err(failure)?
    else {
        anyhow::bail!("credential missing");
    };
    headers(
        fixture,
        credential.secret.as_deref().context("secret")?,
        revision,
    )
}

#[tokio::test]
async fn principal_installation_and_message_transplants_reject_before_send() -> Result<()> {
    let mut server = Server::start().await?;
    let (fixture, route, _) = provision_protocol(&server.url, "responses").await?;
    let (other, other_route, _) = provision_protocol(&server.url, "responses").await?;
    let checked = async {
        let (ingress, _) = ready(&fixture, &route).await?;
        let revision = ingress
            .get("Bitrouter-Execution-Revision")
            .context("revision")?
            .to_str()?
            .try_into()?;
        let output = invoke(
            &fixture,
            &server,
            ingress.clone(),
            prompt(),
            Arc::new(Control::default()),
        )
        .await?;
        handle(&output)?;
        server.receive().await?;
        let principal = another_principal(&fixture, &revision).await?;
        ensure!(
            invoke(
                &fixture,
                &server,
                principal,
                next(&output),
                Arc::new(Control::default())
            )
            .await
            .is_err()
        );
        let (foreign, _) = ready(&other, &other_route).await?;
        ensure!(
            invoke(
                &other,
                &server,
                foreign,
                next(&output),
                Arc::new(Control::default())
            )
            .await
            .is_err()
        );
        let mut edited = next(&output);
        if let Content::Text { text, .. } = &mut edited.messages[1].content[0] {
            text.push_str(" forged");
        }
        ensure!(
            invoke(
                &fixture,
                &server,
                ingress.clone(),
                edited,
                Arc::new(Control::default())
            )
            .await
            .is_err()
        );
        let mut oversized = next(&output);
        metadata_mut(&mut oversized.messages[1].content[0])
            .entry(ORIGIN_NAMESPACE.into())
            .or_insert_with(|| json!({}))[CONTINUATION_FIELD] = "x".repeat(16385).into();
        ensure!(
            invoke(
                &fixture,
                &server,
                ingress,
                oversized,
                Arc::new(Control::default())
            )
            .await
            .is_err()
        );
        ensure!(
            server.received.try_recv().is_err(),
            "rejected history reached provider"
        );
        Ok(())
    }
    .await;
    fixture.service.shutdown().await?;
    other.service.shutdown().await?;
    server.finish().await?;
    checked
}

#[tokio::test]
async fn private_origin_and_failed_sealing_preserve_received_output_and_usage() -> Result<()> {
    let mut server = Server::start().await?;
    let (fixture, route, _) = provision_protocol(&server.url, "responses").await?;
    let checked = async {
        let (ingress, _) = ready(&fixture, &route).await?;
        let mut private = reply();
        private["output"].as_array_mut().context("output")?.insert(0, json!({"type":"reasoning","id":"reasoning-private","summary":[],"encrypted_content":"opaque-reasoning"}));
        server.set_reply(private.clone())?;
        let output = invoke(&fixture, &server, ingress.clone(), prompt(), Arc::new(Control::default())).await?;
        ensure!(output.content.iter().any(|part| metadata(part).get(ORIGIN_NAMESPACE).and_then(|fields| fields.get(ORIGIN_FIELD)).and_then(Value::as_str).is_some_and(|token| token.starts_with("btpo2."))));
        handle(&output)?;
        server.receive().await?;
        let resumed = invoke(&fixture, &server, ingress.clone(), next(&output), Arc::new(Control::default())).await?;
        ensure!(resumed.usage.as_ref().context("usage")?.completion_tokens == 2);
        ensure!(server.receive().await?.1["previous_response_id"] == "resp-owned-private");
        // Valid terminal and billed text, but the provider ID cannot fit the format.
        let mut too_long = reply(); too_long["id"] = "r".repeat(513).into();
        server.set_reply(too_long)?;
        let control = Arc::new(Control::default());
        let output = invoke(&fixture, &server, ingress.clone(), prompt(), control.clone()).await?;
        ensure!(handle(&output).is_err() && output.response_id.is_none());
        ensure!(output.usage.as_ref().context("usage")?.completion_tokens == 2);
        ensure!(output.content.iter().any(|part| matches!(part, Content::Text { text, .. } if text == "native reply")));
        ensure!(matches!(control.attempts.lock().await.last().context("attempt")?.continuation.output, NativeContinuationOutput::Unverified { .. }));
        server.receive().await?;
        let mut many = reply();
        let parts = many["output"].as_array_mut().context("output")?;
        for index in 0..130 {
            parts.insert(0, json!({"type":"reasoning","id":format!("reasoning-{index}"),"summary":[],"encrypted_content":"opaque"}));
        }
        server.set_reply(many)?;
        let bounded = invoke(&fixture, &server, ingress.clone(), prompt(), Arc::new(Control::default())).await?;
        ensure!(bounded.content.len() >= 130);
        ensure!(bounded.usage.as_ref().context("bounded usage")?.completion_tokens == 2);
        ensure!(handle(&bounded).is_err(), "failed origin sealing must not issue an unusable handle");
        ensure!(bounded.content.iter().all(|part| metadata(part).get(ORIGIN_NAMESPACE).and_then(|fields| fields.get(ORIGIN_FIELD)).is_none()), "partial origins survived aggregate bound");
        server.receive().await?;
        let mut not_stored = reply(); not_stored["store"] = false.into(); server.set_reply(not_stored)?;
        let output = invoke(&fixture, &server, ingress, prompt(), Arc::new(Control::default())).await?;
        ensure!(handle(&output).is_err() && output.usage.is_some());
        server.receive().await?;
        Ok(())
    }.await;
    fixture.service.shutdown().await?;
    server.finish().await?;
    checked
}

#[tokio::test]
async fn public_source_cannot_mint_a_tenant_continuation_without_executor_proof() -> Result<()> {
    use crate::gateway::{
        model::target::AuthScheme,
        pipeline::routing::RoutingTable,
        providers::auth::{ContinuationAuthority, CredentialAuthority},
    };
    let server = Server::start().await?;
    let (fixture, route, _) = provision_protocol(&server.url, "responses").await?;
    let (ingress, _) = ready(&fixture, &route).await?;
    let submission = fixture.service.submission(&ingress).map_err(failure)?;
    let execution = fixture
        .service
        .admit_execution(submission, MODEL, &ApiProtocol::Responses)
        .await
        .map_err(failure)?;
    let pipeline = execution.pipeline()?;
    let policy = pipeline
        .native_private_context
        .as_ref()
        .context("tenant policy")?;
    let target = execution
        .route()
        .route_chain(MODEL, &Default::default(), &execution.caller())
        .await?
        .into_iter()
        .next()
        .context("target")?;
    let authority = ContinuationAuthority::new(
        CredentialAuthority::derive_scoped("forged", "forged", "forged"),
        AuthScheme::Bearer,
    );
    let input = prompt();
    let caller = execution.caller();
    let mut content = vec![Content::Text {
        text: "forged".into(),
        provider_metadata: Default::default(),
    }];
    ensure!(
        policy
            .seal_continuation(
                NativeContinuationSource {
                    prompt: &input,
                    caller: &caller,
                    target: &target,
                    authority: &authority,
                    response_id: "resp-forged",
                    replayable: true
                },
                &mut content
            )
            .is_err()
    );
    ensure!(metadata(&content[0]).get(ORIGIN_NAMESPACE).is_none());
    drop((pipeline, execution));
    fixture.service.shutdown().await?;
    server.finish().await?;
    Ok(())
}

struct Select(u32);

#[async_trait]
impl NativeExecutionControl for Select {
    async fn plan(&self, _plan: NativePlan) -> GatewayResult<NativePlanAdmission> {
        Ok(NativePlanAdmission {
            route_indices: vec![self.0],
        })
    }
    async fn before_attempt(&self, _id: &str, _index: u32) -> GatewayResult<()> {
        Ok(())
    }
    async fn after_attempt(&self, _report: NativeAttemptReport) {}
}

async fn chain(
    fixture: &mut Fixture,
    server: &Server,
    route: &ResourceRef,
    slot: &ResourceRef,
    separate: bool,
) -> Result<ResourceRef> {
    fixture.service.shutdown().await?;
    let resources = Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?);
    let mut route = route.clone();
    route.revision_id = "chain-v1".try_into()?;
    let mut other_slot = slot.clone();
    other_slot.resource_id = "other-slot".try_into()?;
    let mut rows = vec![
        json!({"reference":other_slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}}),
    ];
    let alternate = if separate { &other_slot } else { slot };
    rows.push(json!({"reference":route,"definition":{"kind":"route","selector":"owned-chain","policy":null,
        "routing":{"providers":{
            "left":{"api_base":server.url,"api_protocol":[{"*":"responses"}],"headers":{"x-workspace":"workspace-left"},"models":[{"id":"model-a"}]},
            "right":{"api_base":server.url,"api_protocol":[{"*":"responses"}],"headers":{"x-workspace":"workspace-right"},"models":[{"id":"model-b"}]}},
            "models":{"owned-chain":{"strategy":"priority","endpoints":[{"provider":"left","service_id":"model-a"},{"provider":"right","service_id":"model-b"}]}}},
        "credentials":[{"provider":"left","account":"default","credential":slot},{"provider":"right","account":"default","credential":alternate}]}}));
    resources
        .provision_bytes(
            &fixture.db,
            &serde_json::to_vec(
                &json!({"format":1,"installation_id":fixture.installation,"resources":rows}),
            )?,
        )
        .await?;
    let initial = store::load(&fixture.db, chrono::Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        OperationLimits::default(),
        initial,
        resources,
    )
    .await?;
    Ok(route)
}

#[tokio::test]
async fn retained_fixed_account_header_cannot_be_changed_by_native_ingress() -> Result<()> {
    let mut server = Server::start().await?;
    let (mut fixture, route, slot) = provision_protocol(&server.url, "responses").await?;
    let checked = async {
        let route = chain(&mut fixture, &server, &route, &slot, false).await?;
        let (mut ingress, _) = ready(&fixture, &route).await?;
        ingress.insert("x-workspace", "workspace-a".parse()?);
        let first = invoke(
            &fixture,
            &server,
            ingress.clone(),
            prompt(),
            Arc::new(Select(0)),
        )
        .await?;
        handle(&first)?;
        ensure!(
            server
                .receive()
                .await?
                .0
                .get("x-workspace")
                .context("first workspace")?
                == "workspace-left"
        );
        ingress.insert("x-workspace", "workspace-b".parse()?);
        let second = invoke(
            &fixture,
            &server,
            ingress.clone(),
            next(&first),
            Arc::new(Select(0)),
        )
        .await?;
        handle(&second)?;
        let (sent, wire) = server.receive().await?;
        ensure!(sent.get("x-workspace").context("resumed workspace")? == "workspace-left");
        ensure!(wire["previous_response_id"] == "resp-owned-private");
        // This is the admitted fixed-header path. Dynamic passthrough remains
        // compiler/decoder-rejected and is not qualified by this consumer test.
        Ok(())
    }
    .await;
    fixture.service.shutdown().await?;
    server.finish().await?;
    checked
}

#[tokio::test]
async fn changed_candidate_uses_full_replay_but_equal_secret_different_ref_never_resumes()
-> Result<()> {
    for separate in [false, true] {
        let mut server = Server::start().await?;
        let (mut fixture, route, slot) = provision_protocol(&server.url, "responses").await?;
        let checked = async {
            let route = chain(&mut fixture, &server, &route, &slot, separate).await?;
            let (ingress, _) = ready(&fixture, &route).await?;
            let first = invoke(
                &fixture,
                &server,
                ingress.clone(),
                prompt(),
                Arc::new(Select(0)),
            )
            .await?;
            handle(&first)?;
            let (sent, wire) = server.receive().await?;
            ensure!(wire["model"] == "model-a");
            ensure!(sent.get("x-workspace").context("first account")? == "workspace-left");
            let second = invoke(
                &fixture,
                &server,
                ingress,
                next(&first),
                Arc::new(Select(1)),
            )
            .await;
            if separate {
                ensure!(
                    second.is_err(),
                    "equal secret must not transfer credential ownership"
                );
                ensure!(server.received.try_recv().is_err());
            } else {
                ensure!(second?.usage.is_some());
                let (sent, body) = server.receive().await?;
                ensure!(sent.get("x-workspace").context("second account")? == "workspace-right");
                ensure!(body["model"] == "model-b" && body.get("previous_response_id").is_none());
                ensure!(body["input"].as_array().context("full input")?.len() >= 3);
            }
            Ok(())
        }
        .await;
        fixture.service.shutdown().await?;
        server.finish().await?;
        checked?;
    }
    Ok(())
}
