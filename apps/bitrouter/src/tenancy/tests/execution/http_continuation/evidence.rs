//! Real rendered requests distinguish exact provider state from optional replay.
use super::*;

mod wire;

#[tokio::test]
async fn fresh_rendered_body_gate_checks_reserved_fields_for_cross_wire_targets() -> Result<()> {
    use crate::{
        continuation::ContinuationRuntime,
        gateway::pipeline::{
            context::PipelineContext,
            hooks::PreRequestHook,
            routing::RoutingTable,
            types::{PipelineInput, PipelineRequest},
        },
    };
    use bitrouter_ai::types::{ApiProtocol, Prompt};
    let provider = Server::start().await?;
    let ready = ready(&provider).await?;
    let outcome: Result<()> = async {
        for inbound in [ApiProtocol::Responses, ApiProtocol::ChatCompletions] {
            let submission = ready
                .fixture
                .service
                .submission(&ready.headers)
                .map_err(failure)?;
            let execution = ready
                .fixture
                .service
                .admit_execution(submission, MODEL, &inbound)
                .await
                .map_err(failure)?;
            let target = execution
                .route()
                .route_chain(MODEL, &Default::default(), &execution.caller())
                .await?
                .into_iter()
                .next()
                .context("approved target")?;
            ensure!(target.api_protocol == ApiProtocol::Responses);
            let runtime = ContinuationRuntime::for_tenant(
                ready.fixture.service.http_registry.clone(),
                execution.clone(),
            )?;
            let mut ctx = PipelineContext::new(PipelineRequest {
                extensions: Default::default(),
                request_id: execution.id().as_str().into(),
                original_model: MODEL.into(),
                model: MODEL.into(),
                caller: execution.caller(),
                headers: ready.headers.clone(),
                inbound_protocol: Some(inbound),
                input: PipelineInput::Generation(Box::new(Prompt {
                    model: MODEL.into(),
                    system: None,
                    system_provider_metadata: Default::default(),
                    messages: vec![],
                    tools: vec![],
                    params: Default::default(),
                    response_format: None,
                    tool_choice: None,
                    stream: false,
                })),
            });
            ctx.insert_extension(execution.clone());
            runtime.check(&mut ctx).await?;
            let proof = ctx
                .extension::<crate::continuation::tenant::runtime::Runtime>()
                .context("render gate missing for inbound wire")?;
            // The actual provider fixture above covers profile qualification.
            // This separately exercises the rendered-body gate: source extras
            // and resolved previous are absent, exactly the old bypass state.
            ensure!(
                proof
                    .substitute(&mut json!({"model":"model-a"}), &target, &ctx)?
                    .is_none()
            );
            for field in ["previous_response_id", "conversation"] {
                let mut rendered = json!({"model":"model-a"});
                rendered[field] = "foreign-provider-state".into();
                ensure!(proof.substitute(&mut rendered, &target, &ctx).is_err());
            }
            drop((proof, ctx, runtime, execution));
        }
        ensure!(provider.state.calls.load(Ordering::SeqCst) == 0);
        ensure!(row_count(&ready).await? == 0);
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    provider.close().await?;
    outcome
}

#[tokio::test]
async fn reserved_router_defaults_are_rejected_without_poisoning_other_execution() -> Result<()> {
    use crate::tenancy::{Service, command::Command, resources::Resources};
    use bitrouter_ai::tenancy::{authority::ResourceKind, operation::ErrorCode};
    let mut provider = Server::start().await?;
    let mut ready = ready(&provider).await?;
    ready.fixture.service.shutdown().await?;
    let resources = Resources::with_key(ready.fixture.installation.clone(), [29; 32])?;
    let mut slot = ready.route.clone();
    slot.resource_kind = ResourceKind::Credential;
    slot.resource_id = "upstream-slot".try_into()?;
    let mut references = Vec::new();
    let mut imports = Vec::new();
    for (index, field) in ["previous_response_id", "conversation"].iter().enumerate() {
        let mut reference = ready.route.clone();
        reference.revision_id = format!("reserved-{index}").try_into()?;
        let mut params = serde_json::Map::new();
        params.insert((*field).into(), "foreign-provider-state".into());
        imports.push(json!({"reference":reference,"definition":{"kind":"route","selector":"bitrouter/owned","policy":null,
            "routing":{"providers":{"mock":{"api_base":provider.url,"api_protocol":[{"*":"responses"}],"models":[{"id":"model-a"}]}},
                "routers":{"owned":{"selection":{"kind":"model","model":"mock:model-a"},"defaults":{"params":params}}}},
            "credentials":[{"provider":"mock","account":"default","credential":slot}]}}));
        references.push(reference);
    }
    resources.provision_bytes(&ready.fixture.db, &serde_json::to_vec(&json!({"format":1,"installation_id":ready.fixture.installation,"resources":imports}))?).await?;
    let initial = crate::tenancy::store::load(&ready.fixture.db, chrono::Utc::now()).await?;
    ready.fixture.service = Service::start_prepared(
        ready.fixture.db.clone(),
        None,
        Default::default(),
        initial,
        Arc::new(resources),
    )
    .await?;
    ready
        .fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let issued = ready.fixture.issue("reserved-default-good-caller").await?;
    ready.headers = headers(
        &ready.fixture,
        issued.secret.as_deref().context("secret")?,
        &ready.revision.revision_id,
    )?;
    let outcome: Result<()> = async {
        for (index, reference) in references.into_iter().enumerate() {
            let mut input = ready
                .fixture
                .configuration_input(&format!("reserved-default-{index}"))?;
            input.document.routes = vec![reference];
            let result = ready
                .fixture
                .service
                .execute(
                    ready.fixture.operator.clone(),
                    Command::Configuration(ready.fixture.tenant.clone(), input),
                )
                .await;
            ensure!(matches!(result, Err(ref error) if error.0.code == ErrorCode::InvalidRequest));
            ensure!(ready.fixture.service.current().is_ok());
        }
        ensure!(provider.state.calls.load(Ordering::SeqCst) == 0);
        let (status, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
        ensure!(status == StatusCode::OK);
        let public = public_id(&bytes, false)?;
        provider.wire().await?;
        let (status, bytes) =
            send(&ready, ready.headers.clone(), body(true, Some(&public))).await?;
        ensure!(status == StatusCode::OK);
        public_id(&bytes, true)?;
        ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    provider.close().await?;
    outcome
}

#[tokio::test]
async fn image_input_keeps_provider_backed_json_and_sse_continuation() -> Result<()> {
    for stream in [false, true] {
        let mut provider = Server::start().await?;
        let ready = ready(&provider).await?;
        let outcome: Result<()> = async {
            let mut input = body(stream, None);
            input["input"] = json!([{"role":"user","content":[
                {"type":"input_text","text":"describe this"},
                {"type":"input_image","image_url":"data:image/png;base64,aW1hZ2U="}
            ]}]);
            let (status, bytes) = send(&ready, ready.headers.clone(), input).await?;
            ensure!(status == StatusCode::OK);
            let public = public_id(&bytes, stream)?;
            let wire = provider.wire().await?;
            ensure!(wire.to_string().contains("input_image"));
            let (crypto, row) = store::retained(&ready, &public).await?;
            ensure!(
                row.open(&crypto)?
                    .live
                    .context("live mapping")?
                    .causal_prefix
                    .is_none()
            );
            let (status, bytes) =
                send(&ready, ready.headers.clone(), body(!stream, Some(&public))).await?;
            ensure!(status == StatusCode::OK);
            public_id(&bytes, !stream)?;
            ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
            ensure!(row_count(&ready).await? == 2);
            Ok(())
        }
        .await;
        ready.fixture.service.shutdown().await?;
        provider.close().await?;
        outcome?;
    }
    Ok(())
}

#[tokio::test]
async fn lossy_raw_output_never_authorizes_visible_history_detachment() -> Result<()> {
    for stream in [false, true] {
        for extra in [
            json!({"type":"mcp_list_tools","id":"mcp-private","server_label":"server","tools":[]}),
            json!({"type":"future_state_item","id":"opaque-private","state":"provider-held"}),
        ] {
            let mut provider = Server::start().await?;
            provider
                .state
                .options
                .lock()
                .map_err(|_| anyhow::anyhow!("options"))?
                .extra_output = Some(extra);
            let ready = ready(&provider).await?;
            let outcome: Result<()> = async {
                let (status, bytes) =
                    send(&ready, ready.headers.clone(), body(stream, None)).await?;
                ensure!(status == StatusCode::OK);
                let public = public_id(&bytes, stream)?;
                provider.wire().await?;
                let (crypto, row) = store::retained(&ready, &public).await?;
                ensure!(
                    row.open(&crypto)?
                        .live
                        .context("live mapping")?
                        .causal_prefix
                        .is_none()
                );
                let mut follow = body(stream, Some(&public));
                // Exactly the otherwise-reconstructible visible transcript. A
                // lossy JSON projection formerly detached this provider state.
                follow["input"] = json!([
                    {"role":"user","content":"follow up"},
                    {"role":"assistant","content":"owned reply"},
                    {"role":"user","content":"continue"}
                ]);
                let (status, bytes) = send(&ready, ready.headers.clone(), follow).await?;
                ensure!(status == StatusCode::OK);
                public_id(&bytes, stream)?;
                ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
                Ok(())
            }
            .await;
            ready.fixture.service.shutdown().await?;
            provider.close().await?;
            outcome?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn incomplete_json_and_sse_terminals_keep_exact_resume_and_usage() -> Result<()> {
    for stream in [false, true] {
        let mut provider = Server::start().await?;
        provider
            .state
            .options
            .lock()
            .map_err(|_| anyhow::anyhow!("options"))?
            .incomplete = true;
        let ready = ready(&provider).await?;
        let outcome: Result<()> = async {
            let (status, bytes) = send(&ready, ready.headers.clone(), body(stream, None)).await?;
            ensure!(status == StatusCode::OK);
            let public = public_id(&bytes, stream)?;
            ensure!(String::from_utf8_lossy(&bytes).contains("incomplete"));
            provider.wire().await?;
            ensure!(row_count(&ready).await? == 1);
            let (status, bytes) =
                send(&ready, ready.headers.clone(), body(!stream, Some(&public))).await?;
            ensure!(status == StatusCode::OK);
            public_id(&bytes, !stream)?;
            ensure!(provider.wire().await?["previous_response_id"] == "resp-http-private-1");
            if stream {
                provider
                    .state
                    .options
                    .lock()
                    .map_err(|_| anyhow::anyhow!("options"))?
                    .trailing_error = true;
                let (_, bytes) = send(&ready, ready.headers.clone(), body(true, None)).await?;
                provider.wire().await?;
                ensure!(
                    !String::from_utf8_lossy(&bytes).contains("\"type\":\"response.incomplete\"")
                );
                ensure!(row_count(&ready).await? == 2);
            }
            Ok(())
        }
        .await;
        ready.fixture.service.shutdown().await?;
        provider.close().await?;
        outcome?;
    }
    Ok(())
}

#[tokio::test]
async fn literal_ingress_binding_allows_prepared_selector_but_rejects_forged_entry() -> Result<()> {
    use crate::gateway::{
        model::protocol::{InboundAdapter, responses::ResponsesAdapter},
        pipeline::types::PipelineRequest,
    };
    use bitrouter_ai::types::ApiProtocol;
    let mut provider = Server::start().await?;
    let ready = ready(&provider).await?;
    let outcome: Result<()> = async {
        // The private retained selector is bitrouter/owned, not the literal
        // public resource name containing ':'. The normal planner changes the
        // canonical prompt model before034's final route/auth checks.
        for stream in [false, true] {
            let (status, bytes) = send(&ready, ready.headers.clone(), body(stream, None)).await?;
            ensure!(
                status == StatusCode::OK,
                "prepared selector was rejected: {}",
                String::from_utf8_lossy(&bytes)
            );
            public_id(&bytes, stream)?;
            ensure!(provider.wire().await?["model"] == "model-a");
        }
        for (original, prompt_model) in [("other:literal-route", MODEL), (MODEL, "bitrouter/owned")]
        {
            let execution = ready
                .fixture
                .service
                .admit_execution(
                    ready
                        .fixture
                        .service
                        .submission(&ready.headers)
                        .map_err(failure)?,
                    MODEL,
                    &ApiProtocol::Responses,
                )
                .await
                .map_err(failure)?;
            let pipeline = execution.pipeline()?;
            let mut prompt = ResponsesAdapter.parse_request(body(false, None))?;
            prompt.model = prompt_model.into();
            let mut request = PipelineRequest::new(MODEL, execution.caller(), prompt);
            request.request_id = execution.id().to_string();
            request.original_model = original.into();
            request.headers = ready.headers.clone();
            request.inbound_protocol = Some(ApiProtocol::Responses);
            execution.attach(pipeline.clone(), &mut request)?;
            ensure!(
                pipeline
                    .clone()
                    .execute_detached_prepared(request)
                    .await
                    .is_err()
            );
            ensure!(
                execution.failure().is_some_and(|error| error.0.code
                    == bitrouter_ai::tenancy::operation::ErrorCode::InvalidRequest)
            );
            drop((pipeline, execution));
            tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
        }
        ensure!(provider.state.calls.load(Ordering::SeqCst) == 2);
        ensure!(row_count(&ready).await? == 2);
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    provider.close().await?;
    outcome
}
