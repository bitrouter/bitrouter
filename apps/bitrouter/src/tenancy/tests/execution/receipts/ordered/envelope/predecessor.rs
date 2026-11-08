//! Actual canonical-only predecessor: an envelope never downgrades or forwards.
use super::super::predecessor as prior;
use super::*;
use bitrouter_ai::tenancy::operation::DispatchState;

#[tokio::test]
#[ignore = "requires BITROUTER_DEFAULTS_PREDECESSOR_BRO pointing to the actual b3e5a3af74b57cedf85759f3a6c67841665f7cb2 CLI"]
async fn envelope_actual_canonical_only_predecessor_refuses_all_transports_without_send()
-> Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let provider = Provider {
        observed: Arc::new(Mutex::new(Vec::new())),
        held_provider: Arc::new(Mutex::new(None)),
        malformed: None,
        controls: Arc::new(ProviderControls::default()),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(ordered_provider))
        .with_state(provider.clone());
    let _provider =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, app).await },
        ));
    let mut server = prior::Predecessor::start_at(
        "BITROUTER_DEFAULTS_PREDECESSOR_BRO",
        "b3e5a3af74b57cedf85759f3a6c67841665f7cb2",
    )
    .await?;
    let caps = server.ready().await?;
    let control = server.control(HttpVersion::Http1Only)?;
    let provisioned = prior::provision(&control, &caps, &base).await?;
    let token = provisioned.credential.secret.as_deref().context("token")?;
    let caps = control.ordered_capabilities().await?;
    let original = caps.bind(
        provisioned.revision.revision_id.clone(),
        "envelope-predecessor".try_into()?,
        OrderedCandidateSelectionV1 {
            schema_version: 1,
            admission_mode: bitrouter_ai::tenancy::admission::AdmissionMode::Legacy,
            route: provisioned.route.clone(),
            candidate_indices: vec![1],
            generation_defaults_digest: None,
        },
    )?;
    let values = defaults(json!({"vendor_knob":{"preset":true}}))?;
    let binding = original
        .clone()
        .with_generation_defaults_envelope(&values)?;
    let versions = vec![HttpVersion::Http1Only];
    #[cfg(feature = "http2")]
    let versions = {
        let mut versions = versions;
        if server.http2 {
            versions.push(HttpVersion::Http2PriorKnowledge);
        }
        versions
    };
    for version in versions {
        let client = GatewayClient::with_options(
            &server.endpoint,
            None,
            TransportOptions {
                http_version: version,
                ..Default::default()
            },
        )?;
        // Positive controls prove the same live route accepts both preceding APIs.
        let request: ChatRequest = serde_json::from_value(
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]}),
        )?;
        client
            .ordered_tenant(token, &original)?
            .chat_reply(&request)
            .await?;
        let canonical = values.canonical.clone().context("canonical")?;
        let canonical_binding = original.clone().with_generation_defaults(&canonical)?;
        client
            .ordered_tenant(token, &canonical_binding)?
            .with_generation_defaults(canonical)?
            .chat_reply(&request)
            .await?;
        let before = prior::observed(&provider)?.len();
        for protocol in [
            GenerationProtocol::ChatCompletions,
            GenerationProtocol::Messages,
            GenerationProtocol::Responses,
        ] {
            for stream in [false, true] {
                let call = client
                    .ordered_tenant(token, &binding)?
                    .with_generation_defaults_envelope(values.clone())?;
                let request = match protocol {
                    GenerationProtocol::Responses => {
                        json!({"model":MODEL,"input":"hello","stream":stream})
                    }
                    _ => {
                        json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25,"stream":stream})
                    }
                };
                let error = match protocol {
                    GenerationProtocol::ChatCompletions => call
                        .chat_reply(&serde_json::from_value::<ChatRequest>(request)?)
                        .await
                        .err(),
                    GenerationProtocol::Messages => call
                        .messages_reply(&serde_json::from_value::<MessagesRequest>(request)?)
                        .await
                        .err(),
                    GenerationProtocol::Responses => call
                        .responses_reply(&serde_json::from_value::<ResponsesRequest>(request)?)
                        .await
                        .err(),
                }
                .context("predecessor accepted envelope")?;
                ensure!(error.dispatch_state() == DispatchState::NotStarted);
                let bitrouter_sdk::gateway::tenancy::Error::Response(error) = error else {
                    anyhow::bail!("expected HTTP refusal");
                };
                ensure!(error.metadata.status == 400 && error.metadata.execution_id.is_none());
                ensure!(prior::observed(&provider)?.len() == before);
            }
        }
    }
    server.stop().await?;
    Ok(())
}
