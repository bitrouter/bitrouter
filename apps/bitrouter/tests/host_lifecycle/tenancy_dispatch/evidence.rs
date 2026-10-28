//! Bounded wire observations from actual inference and SDK receipt consumers.
use super::*;
use bitrouter_ai::tenancy::execution::{Outcome, Provenance, ReceiptState, Usage, UsageDimension};
use sha2::{Digest, Sha256};

pub(super) fn eligible(value: &TenantCapabilities) -> Result<()> {
    value.validate()?;
    ensure!(
        value.dispatch_enabled && value.strict_receipts && value.revision_bound_dispatch,
        "real bro did not advertise strict dispatch"
    );
    Ok(())
}

pub(super) fn original_usage() -> Value {
    json!({"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,
        "prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},
        "completion_tokens_details":{"reasoning_tokens":4}})
}

pub(super) fn request(
    host: &Host,
    client: &reqwest::Client,
    binding: &DispatchBinding,
    token: &str,
    stream: bool,
) -> Result<reqwest::RequestBuilder> {
    binding.validate()?;
    let mut request = client
        .post(format!("http://{}/v1/chat/completions", host.inference))
        .bearer_auth(token);
    let headers = serde_json::to_value(binding)?;
    let headers = headers
        .as_object()
        .context("literal dispatch header object")?;
    ensure!(headers.len() == 7);
    for (name, value) in headers {
        request = request.header(
            name.as_str(),
            value.as_str().context("literal dispatch header value")?,
        );
    }
    let mut body = json!({"model":MODEL,"messages":[{"role":"user","content":"one actual provider invocation"}],"stream":stream});
    if stream {
        body["stream_options"] = json!({"include_usage":true});
    }
    Ok(request.json(&body))
}

pub(super) async fn body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        ensure!(
            length <= BODY_LIMIT as u64,
            "response exceeds fixture bound"
        );
    }
    let mut result = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= BODY_LIMIT.saturating_sub(result.len()),
            "response exceeds fixture bound"
        );
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

pub(super) fn execution_id(response: &reqwest::Response) -> Result<ExecutionId> {
    Ok(response
        .headers()
        .get("x-bitrouter-request-id")
        .context("actual admitted identity")?
        .to_str()?
        .try_into()?)
}

pub(super) async fn reject(response: reqwest::Response, code: ErrorCode) -> Result<()> {
    let status = match code {
        ErrorCode::CapabilityMismatch => reqwest::StatusCode::CONFLICT,
        ErrorCode::Unauthorized => reqwest::StatusCode::UNAUTHORIZED,
        _ => bail!("unsupported fixture rejection"),
    };
    ensure!(
        response.status() == status,
        "expected {code:?}, got {}",
        response.status()
    );
    let error: bitrouter_ai::tenancy::operation::Error =
        serde_json::from_slice(&body(response).await?)?;
    error.validate()?;
    ensure!(error.code == code && error.dispatch_state == DispatchState::NotStarted);
    Ok(())
}

pub(super) async fn sent_once(provider: &MockServer, stream: bool) -> Result<()> {
    let requests = provider
        .received_requests()
        .await
        .context("provider recording unavailable")?;
    ensure!(
        requests.len() == 1,
        "expected one actual send, observed {}",
        requests.len()
    );
    let observed = &requests[0];
    ensure!(observed.method == "POST" && observed.url.path() == "/chat/completions");
    ensure!(
        observed
            .headers
            .get("authorization")
            .context("provider authorization")?
            .to_str()?
            == format!("Bearer {PROVIDER_KEY}")
    );
    ensure!(observed.body.len() < BODY_LIMIT);
    let value: Value = serde_json::from_slice(&observed.body)?;
    ensure!(
        value["model"] == "model",
        "literal ingress route must resolve only to its retained target"
    );
    ensure!(value["stream"].as_bool().is_some_and(|value| value) == stream);
    Ok(())
}

pub(super) async fn receipt(
    owned: &fixture::Provisioned,
    correlation: &CorrelationId,
    id: &ExecutionId,
) -> Result<ExecutionReceipt> {
    let installation = &owned.installation;
    let value = owned
        .reader
        .execution(installation, &owned.tenant, id)
        .await?;
    value.validate()?;
    ensure!(
        value.installation_id == *installation
            && value.tenant_id == owned.tenant
            && value.principal_id == owned.principal
    );
    ensure!(
        value.execution_id == *id
            && value.correlation_id == *correlation
            && value.execution_revision == owned.revision.revision_id
    );
    ensure!(
        value.state == ReceiptState::Committed
            && value.outcome == Outcome::Completed
            && value.attempts.len() == 1
    );
    usage(&value.usage)?;
    let attempt = &value.attempts[0];
    ensure!(attempt.outcome == Outcome::Completed);
    usage(&attempt.usage)?;
    let target = attempt.target.as_ref().context("actual retained target")?;
    ensure!(
        target.provider_id.as_str() == "mock"
            && target.model_id == "model"
            && target.credential_version.as_str() == "v1"
    );
    // Pin the opaque projections to the actual configured account and credential
    // slot, not merely an ID-shaped string from some other retained target.
    ensure!(
        target.account_id.as_str()
            == opaque(
                "acct1",
                b"bitrouter.receipt.configured-account.v1",
                &[
                    installation.as_str(),
                    "tenant",
                    owned.tenant.as_str(),
                    "route",
                    MODEL,
                    "mock",
                    "default",
                ]
            )?
    );
    ensure!(
        target.credential_id.as_str()
            == opaque(
                "cred1",
                b"bitrouter.receipt.credential-slot.v1",
                &[
                    installation.as_str(),
                    "tenant",
                    owned.tenant.as_str(),
                    "credential",
                    "dispatch-slot",
                ]
            )?
    );
    let expected = [
        (UsageDimension::InputTokens, 11),
        (UsageDimension::OutputTokens, 7),
        (UsageDimension::CacheReadTokens, 3),
        (UsageDimension::CacheWriteTokens, 2),
        (UsageDimension::ReasoningTokens, 4),
    ];
    ensure!(attempt.native_usage.len() == expected.len());
    for (name, value) in expected {
        ensure!(
            attempt
                .native_usage
                .iter()
                .any(|counter| counter.name == name && counter.value == value),
            "original native counter {name:?} changed"
        );
    }
    let serialized = serde_json::to_string(&value)?;
    ensure!(
        serialized.len() < BODY_LIMIT
            && !serialized.contains(PROVIDER_KEY)
            && !serialized.contains(CONTROL_TOKEN)
    );
    ensure!(
        !serialized.contains(
            owned
                .issued
                .secret
                .as_deref()
                .context("tenant credential")?
        )
    );
    let page = owned
        .reader
        .executions(installation, &owned.tenant, correlation, None, 2)
        .await?;
    page.validate()?;
    ensure!(
        page.execution_ids.as_slice() == std::slice::from_ref(id)
            && !page.multiple_executions
            && page.next_after.is_none()
    );
    Ok(value)
}

fn usage(value: &Usage) -> Result<()> {
    ensure!(
        value.input_tokens == Some(11)
            && value.output_tokens == Some(7)
            && value.cache_read_tokens == Some(3)
            && value.cache_write_tokens == Some(2)
            && value.reasoning_tokens == Some(4)
            && value.provenance == Provenance::Provider
            && value.complete,
        "original provider usage changed"
    );
    Ok(())
}

fn opaque(prefix: &str, domain: &[u8], parts: &[&str]) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(domain);
    for part in parts {
        hash.update(u64::try_from(part.len())?.to_be_bytes());
        hash.update(part.as_bytes());
    }
    Ok(format!("{prefix}-{}", hex::encode(hash.finalize())))
}
