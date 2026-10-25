use super::*;
use crate::tenancy::{Service, actor::OperationLimits, resources::Resources};
use bitrouter_ai::{
    tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
        execution::Outcome,
        operation::{DispatchState, ErrorCode},
    },
    types::ApiProtocol,
};

#[tokio::test]
async fn eight_actual_fallback_sends_are_recorded_and_ninth_never_reaches_provider() -> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let (received, mut received_rx) = mpsc::channel(16);
    let calls = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/v1/chat/completions", post(provider))
        .with_state(Reply {
            status: StatusCode::TOO_MANY_REQUESTS,
            content_type: "application/json",
            body: serde_json::to_string(
                &json!({"error":{"message":"quota"},"usage":full_usage("chat_completions")}),
            )?,
            received,
            calls: calls.clone(),
            gate: None,
        });
    let _provider =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let mut fixture = Fixture::new().await?;
    fixture.service.shutdown().await?;
    let resources = Resources::with_key(fixture.installation.clone(), [61; 32])?;
    let reference = |kind, id: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "r1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, MODEL)?;
    let credential = reference(ResourceKind::Credential, "owned-static-slot")?;
    let mut providers = serde_json::Map::new();
    let mut endpoints = Vec::new();
    let mut credentials = Vec::new();
    for index in 0..9 {
        let provider = format!("provider-{index}");
        let model = format!("model-{index}");
        providers.insert(provider.clone(),json!({"api_base":base,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":model}]}));
        endpoints.push(json!({"provider":provider,"service_id":model}));
        credentials.push(json!({"provider":provider,"account":"default","credential":credential}));
    }
    resources.provision_bytes(&fixture.db,&serde_json::to_vec(&json!({"format":1,"installation_id":fixture.installation,"resources":[
        {"reference":credential,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}},
        {"reference":route,"definition":{"kind":"route","selector":"attempt-chain","policy":null,"routing":{"providers":providers,"models":{"attempt-chain":{"strategy":"priority","endpoints":endpoints}}},"credentials":credentials}}
    ]}))?).await?;
    let snapshot = crate::tenancy::store::load(&fixture.db, chrono::Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    let revision = draft(&fixture, &route, "eight-attempt-config", 25).await?;
    let issued = fixture.issue("eight-attempt-client").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let token = issued.secret.as_deref().context("token")?;
    let headers = headers(&fixture, token, &revision.revision_id)?;
    let execution = fixture
        .service
        .admit_execution(
            fixture.service.submission(&headers).map_err(failure)?,
            MODEL,
            &ApiProtocol::ChatCompletions,
        )
        .await
        .map_err(failure)?;
    let id = execution.id().clone();
    let input = crate::gateway::http::parse_input(
        &ApiProtocol::ChatCompletions,
        json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]}),
    )?;
    let crate::gateway::pipeline::types::PipelineInput::Generation(prompt) = input else {
        anyhow::bail!("generation input")
    };
    let mut request =
        crate::gateway::pipeline::types::PipelineRequest::new(MODEL, execution.caller(), *prompt);
    request.request_id = id.to_string();
    request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
    request.headers = headers;
    let pipeline = execution.pipeline()?;
    execution.attach(pipeline.clone(), &mut request)?;
    ensure!(
        tokio::time::timeout(TIMEOUT, pipeline.execute_detached(request))
            .await?
            .is_err()
    );
    let rejection = execution.failure().context("typed capacity rejection")?;
    ensure!(
        rejection.0.code == ErrorCode::CapacityExceeded
            && rejection.0.dispatch_state == DispatchState::Started
    );
    drop(execution);
    let caller = fixture.service.authenticate(token).map_err(failure)?;
    let bytes = fixture
        .service
        .read_receipt(
            caller,
            fixture.tenant.clone(),
            read::Request::One(id.clone()),
        )
        .await
        .map_err(failure)?;
    let receipt: ExecutionReceipt = serde_json::from_slice(&bytes)?;
    ensure!(
        receipt.execution_id == id
            && receipt.attempts.len() == 8
            && receipt.outcome == Outcome::Failed
    );
    for (index, attempt) in receipt.attempts.iter().enumerate() {
        ensure!(attempt.outcome == Outcome::Failed);
        let target = attempt.target.as_ref().context("actual target")?;
        ensure!(
            target.provider_id.as_str() == format!("provider-{index}")
                && target.model_id == format!("model-{index}")
        );
        let observed = received_rx.try_recv().context("actual POST per attempt")?;
        ensure!(observed["model"] == target.model_id);
    }
    ensure!(calls.load(Ordering::SeqCst) == 8 && received_rx.try_recv().is_err());
    ensure!(receipt.usage.input_tokens == Some(88) && receipt.usage.output_tokens == Some(56));
    fixture.service.shutdown().await?;
    Ok(())
}
