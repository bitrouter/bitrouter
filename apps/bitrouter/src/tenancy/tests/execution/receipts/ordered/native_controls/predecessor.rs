//! Real predecessor discovery and direct wire refusals without inferred support.
use super::super::predecessor as prior;
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

#[tokio::test]
#[ignore = "requires BITROUTER_NATIVE_CONTROLS_PREDECESSOR_BRO pointing to actual 89033c1bc32fc63598cd8090ee6fabefa57cd219 CLI"]
async fn native_controls_actual_predecessor_has_no_discovery_and_refuses_nonnull_without_send()
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
        "BITROUTER_NATIVE_CONTROLS_PREDECESSOR_BRO",
        "89033c1bc32fc63598cd8090ee6fabefa57cd219",
    )
    .await?;
    let caps = server.ready().await?;
    let control = server.control(HttpVersion::Http1Only)?;
    let provisioned = prior::provision(&control, &caps, &base).await?;
    let token = provisioned.credential.secret.as_deref().context("token")?;
    let caps = control.ordered_capabilities().await?;
    let binding = caps.bind(
        provisioned.revision.revision_id,
        "native-predecessor".try_into()?,
        OrderedCandidateSelectionV1 {
            schema_version: 1,
            admission_mode: bitrouter_ai::tenancy::admission::AdmissionMode::Legacy,
            route: provisioned.route,
            candidate_indices: vec![1],
            generation_defaults_digest: None,
        },
    )?;
    ensure!(
        control
            .ordered_native_controls_capabilities()
            .await
            .is_err()
    );
    ensure!(prior::observed(&provider)?.is_empty());
    let mut headers = HeaderMap::new();
    let encoded = serde_json::to_value(&binding)?;
    for (name, value) in encoded.as_object().context("binding")? {
        if name != "selection" {
            headers.insert(
                ::http::HeaderName::from_bytes(name.as_bytes())?,
                value.as_str().context("header")?.parse()?,
            );
        }
    }
    headers.insert("authorization", format!("Bearer {token}").parse()?);
    headers.insert(
        "bitrouter-candidate-selection",
        URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&binding.selection)?)
            .parse()?,
    );
    headers.insert(HEADER, PROVIDER_NATIVE_V1.parse()?);
    let clients = vec![reqwest::Client::builder().http1_only().build()?];
    #[cfg(feature = "http2")]
    let clients = {
        let mut clients = clients;
        if server.http2 {
            clients.push(reqwest::Client::builder().http2_prior_knowledge().build()?);
        }
        clients
    };
    let mut sent = 0;
    for client in clients {
        for stream in [false, true] {
            // An empty/null request makes no new custody promise: the predecessor
            // may ignore the opt-in header and execute ordinary v2 successfully.
            let reply = client.post(format!("{}/v1/responses", server.endpoint)).headers(headers.clone())
                .json(&json!({"model":MODEL,"input":"hello","stream":stream,"previous_response_id":null})).send().await?;
            ensure!(reply.status().is_success());
            reply.bytes().await?;
            sent += 1;
            ensure!(prior::observed(&provider)?.len() == sent);
            for field in [
                "previous_response_id",
                "conversation",
                "context_management",
                "cachedContent",
            ] {
                let mut body = json!({"model":MODEL,"input":"next","stream":stream});
                body[field] = json!("opaque-provider-value");
                let response = client
                    .post(format!("{}/v1/responses", server.endpoint))
                    .headers(headers.clone())
                    .json(&body)
                    .send()
                    .await?;
                ensure!(response.status() == StatusCode::CONFLICT);
                ensure!(!response.headers().contains_key("x-bitrouter-request-id"));
                let error: bitrouter_ai::tenancy::operation::Error = response.json().await?;
                ensure!(
                    error.dispatch_state
                        == bitrouter_ai::tenancy::operation::DispatchState::NotStarted
                );
                ensure!(error.execution_id.is_none() && prior::observed(&provider)?.len() == sent);
            }
        }
    }
    server.stop().await?;
    Ok(())
}
