//! Real Cargo-built bro eligibility, protected dispatch and immediate durable receipts.
use super::*;
use bitrouter_ai::tenancy::{
    authority::{CredentialIssued, CredentialState},
    execution::ExecutionReceipt,
    identity::{CorrelationId, ExecutionId},
    operation::{
        DispatchBinding, DispatchState, ErrorCode, Mutation, OperationResult, OperationStatus,
        TenantCapabilities,
    },
    validation::Validate,
};
use bitrouter_sdk::tenancy::TenancyClient;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

#[path = "tenancy_dispatch/evidence.rs"]
mod evidence;
#[path = "tenancy_dispatch/fixture.rs"]
mod fixture;
use evidence::{body, eligible, execution_id, original_usage, receipt, reject, request, sent_once};

const MODEL: &str = "visible:opaque";
const BODY_LIMIT: usize = 64 * 1024;

fn applied(result: &OperationResult) -> Result<()> {
    result.validate()?;
    ensure!(
        result.status == OperationStatus::Applied,
        "management command was not applied: {:?}",
        result.status
    );
    Ok(())
}

#[tokio::test]
async fn actual_owned_http_json_receipt_restart_and_revoke() -> Result<()> {
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", format!("Bearer {PROVIDER_KEY}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"chatcmpl-production", "object":"chat.completion", "created":0,
            "model":"model", "choices":[{"index":0,"message":{"role":"assistant","content":"owned reply"},"finish_reason":"stop"}],
            "usage":original_usage(),
        })))
        .mount(&provider).await;
    let mut host = Host::start_profile(&provider.uri(), true)?;
    let result = tokio::time::timeout(CASE_TIMEOUT, async {
        let owned = fixture::provision(&mut host, &provider.uri()).await?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http1_only()
            .pool_max_idle_per_host(0)
            .connect_timeout(Duration::from_secs(1))
            .timeout(STEP_TIMEOUT)
            .build()?;
        let before = owned.reader.capabilities().await?;
        let correlation: CorrelationId = "production-json".try_into()?;
        let binding = before.bind(owned.revision.revision_id.clone(), correlation.clone())?;
        let token = owned
            .issued
            .secret
            .as_deref()
            .context("tenant credential")?;
        let response = request(&host, &client, &binding, token, false)?
            .send()
            .await?;
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "JSON dispatch returned {}",
            response.status()
        );
        ensure!(response.version() == reqwest::Version::HTTP_11);
        let id = execution_id(&response)?;
        let reply: Value = serde_json::from_slice(&body(response).await?)?;
        ensure!(reply["choices"][0]["message"]["content"] == "owned reply");
        // No polling/retry: terminal delivery must already have durable facts.
        let original = receipt(&owned, &correlation, &id).await?;
        sent_once(&provider, false).await?;
        let retained = serde_json::to_vec(&owned.revision)?;
        fixture::stop(&mut host).await?;
        fs::remove_file(host.home.join("resources.json"))?;
        let config = host.home.join("private.yaml");
        let source = fs::read_to_string(&config)?;
        ensure!(
            source
                .matches("  resource_import: resources.json\n")
                .count()
                == 1
        );
        fs::write(
            &config,
            source.replace("  resource_import: resources.json\n", ""),
        )?;
        host.spawn_with_wrapping_key(&config, Some(&owned.key))?;
        host.ready().await?;
        let after = owned.reader.capabilities().await?;
        eligible(&after)?;
        ensure!(
            after.installation_id == before.installation_id
                && after.instance_id != before.instance_id
        );
        ensure!(
            serde_json::to_vec(
                &owned
                    .reader
                    .configuration(
                        &after.installation_id,
                        &owned.tenant,
                        &owned.revision.revision_id
                    )
                    .await?
            )? == retained
        );
        ensure!(
            owned
                .reader
                .active_revision(&after.installation_id, &owned.tenant)
                .await?
                .revision_id
                .as_ref()
                == Some(&owned.revision.revision_id)
        );
        ensure!(
            serde_json::to_vec(&receipt(&owned, &correlation, &id).await?)?
                == serde_json::to_vec(&original)?
        );
        reject(
            request(&host, &client, &binding, token, false)?
                .send()
                .await?,
            ErrorCode::CapabilityMismatch,
        )
        .await?;
        sent_once(&provider, false).await?;
        let fresh = after.bind(
            owned.revision.revision_id.clone(),
            "operator-is-not-tenant".try_into()?,
        )?;
        reject(
            request(&host, &client, &fresh, CONTROL_TOKEN, false)?
                .send()
                .await?,
            ErrorCode::Unauthorized,
        )
        .await?;
        sent_once(&provider, false).await?;
        let current = owned
            .operator
            .credential(
                &after.installation_id,
                &owned.tenant,
                &owned.issued.credential_id,
            )
            .await?;
        let revoked = owned
            .operator
            .revoke_credential(
                &owned.tenant,
                &owned.issued.credential_id,
                &Mutation {
                    operation_id: "dispatch-revoke".try_into()?,
                    expected_revision: current.credential_revision,
                    installation_id: after.installation_id.clone(),
                },
            )
            .await?;
        applied(&revoked)?;
        ensure!(
            owned
                .operator
                .credential(
                    &after.installation_id,
                    &owned.tenant,
                    &owned.issued.credential_id
                )
                .await?
                .state
                == CredentialState::Revoked
        );
        let fresh = owned.operator.capabilities().await?.bind(
            owned.revision.revision_id.clone(),
            "revoked-no-send".try_into()?,
        )?;
        reject(
            request(&host, &client, &fresh, token, false)?
                .send()
                .await?,
            ErrorCode::Unauthorized,
        )
        .await?;
        sent_once(&provider, false).await?;
        // Revocation did not erase already committed owner facts.
        ensure!(
            serde_json::to_vec(
                &owned
                    .operator
                    .execution(&after.installation_id, &owned.tenant, &id)
                    .await?
            )? == serde_json::to_vec(&original)?
        );
        let page = owned
            .operator
            .executions(&after.installation_id, &owned.tenant, &correlation, None, 2)
            .await?;
        ensure!(
            page.execution_ids.as_slice() == std::slice::from_ref(&id)
                && !page.multiple_executions
                && page.next_after.is_none()
        );
        for rejected in ["operator-is-not-tenant", "revoked-no-send"] {
            let page = owned
                .operator
                .executions(
                    &after.installation_id,
                    &owned.tenant,
                    &rejected.try_into()?,
                    None,
                    2,
                )
                .await?;
            ensure!(
                page.execution_ids.is_empty()
                    && !page.multiple_executions
                    && page.next_after.is_none()
            );
        }
        fixture::stop(&mut host).await
    })
    .await
    .context("actual owned JSON dispatch lifecycle timed out")
    .and_then(|result| result);
    host.finish(result)
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn actual_owned_h2_sse_receipt_is_durable_at_done() -> Result<()> {
    let provider = MockServer::start().await;
    let first = json!({"id":"chatcmpl-production-sse","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{"role":"assistant","content":"owned stream"},"finish_reason":null}]});
    let last = json!({"id":"chatcmpl-production-sse","object":"chat.completion.chunk","created":0,"model":"model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":original_usage()});
    let stream = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::to_string(&first)?,
        serde_json::to_string(&last)?
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", format!("Bearer {PROVIDER_KEY}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(&provider)
        .await;
    let mut host = Host::start_profile(&provider.uri(), true)?;
    let result = tokio::time::timeout(CASE_TIMEOUT, async {
        let owned = fixture::provision(&mut host, &provider.uri()).await?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http2_prior_knowledge()
            .connect_timeout(Duration::from_secs(1))
            .timeout(STEP_TIMEOUT)
            .build()?;
        let correlation: CorrelationId = "production-h2-sse".try_into()?;
        let binding = owned
            .reader
            .capabilities()
            .await?
            .bind(owned.revision.revision_id.clone(), correlation.clone())?;
        let response = request(
            &host,
            &client,
            &binding,
            owned
                .issued
                .secret
                .as_deref()
                .context("tenant credential")?,
            true,
        )?
        .send()
        .await?;
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "SSE dispatch returned {}",
            response.status()
        );
        ensure!(
            response.version() == reqwest::Version::HTTP_2,
            "h2c must not fall back to HTTP/1.1"
        );
        ensure!(
            response
                .headers()
                .get("content-type")
                .context("SSE content type")?
                .to_str()?
                .starts_with("text/event-stream")
        );
        let id = execution_id(&response)?;
        let bytes = body(response).await?;
        let text = std::str::from_utf8(&bytes)?;
        let events = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect::<Vec<_>>();
        ensure!(
            events.last() == Some(&"[DONE]"),
            "SSE must finish at the terminal marker"
        );
        ensure!(events.iter().filter(|&&event| event == "[DONE]").count() == 1);
        let mut content = String::new();
        for event in events.into_iter().filter(|event| *event != "[DONE]") {
            let event: Value = serde_json::from_str(event)?;
            ensure!(event.get("error").is_none(), "SSE returned a failure event");
            if let Some(delta) = event["choices"][0]["delta"]["content"].as_str() {
                content.push_str(delta);
            }
        }
        ensure!(content == "owned stream");
        // A finish_reason alone is insufficient: consume terminal framing and EOF,
        // then require the original receipt on the very first protected read.
        receipt(&owned, &correlation, &id).await?;
        sent_once(&provider, true).await?;
        fixture::stop(&mut host).await
    })
    .await
    .context("actual owned h2 SSE dispatch lifecycle timed out")
    .and_then(|result| result);
    host.finish(result)
}
