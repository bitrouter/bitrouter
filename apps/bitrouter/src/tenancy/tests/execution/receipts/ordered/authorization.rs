//! Current resource grants and exact retained identities govern real v2 sends.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bitrouter_ai::tenancy::{
    authority::{BindingPut, GrantScope, Permission, TenantPut, TenantRole, TenantState},
    operation::{DispatchState, ErrorCode, Mutation, OperationStatus},
};
use bitrouter_sdk::{error::Error as ControlError, gateway::tenancy::Error as DispatchError};

fn selected_scope(harness: &OrderedHarness, credentials: &[usize]) -> Result<GrantScope> {
    let mut resources = vec![harness.route.key()];
    for index in credentials {
        let mut key = harness.route.key();
        key.resource_kind = ResourceKind::Credential;
        key.resource_id = format!("static-slot-{index}").try_into()?;
        resources.push(key);
    }
    Ok(GrantScope::Resources { resources })
}

async fn scoped_key(
    harness: &OrderedHarness,
    operation: &str,
    credentials: &[usize],
) -> Result<CredentialIssued> {
    let mut issuance = harness.fixture.issuance(operation).await?;
    issuance.permissions = vec![Permission::ExecutionSubmit, Permission::CredentialUse];
    issuance.scope = selected_scope(harness, credentials)?;
    Ok(harness
        .control(true, HttpVersion::Http1Only)?
        .issue_credential(&harness.fixture.tenant, &issuance)
        .await?)
}

async fn narrow_binding(harness: &OrderedHarness, credentials: &[usize]) -> Result<()> {
    let fixture = &harness.fixture;
    let binding = crate::tenancy::store::directory::binding_in(
        &fixture.db,
        &fixture.tenant,
        &fixture.binding,
        64 * 1024,
    )
    .await?
    .context("current recipient binding")?;
    let operation = harness
        .control(true, HttpVersion::Http1Only)?
        .put_binding(
            &fixture.tenant,
            &fixture.binding,
            &BindingPut {
                operation_id: "narrow-ordered-binding".try_into()?,
                expected_revision: Some(binding.revision.clone()),
                principal_id: fixture.principal.clone(),
                role: TenantRole::TenantAdmin,
                scope: selected_scope(harness, credentials)?,
                expires_at: binding.expires.wire.clone(),
                expected_management_owner: Some(match &fixture.operator {
                    crate::tenancy::model::Caller::Installation(id) => id.clone(),
                    _ => anyhow::bail!("fixture operator missing"),
                }),
                installation_id: fixture.installation.clone(),
            },
        )
        .await?;
    ensure!(operation.status == OperationStatus::Applied);
    Ok(())
}

async fn reject_dispatch(
    harness: &OrderedHarness,
    binding: &OrderedDispatchBindingV2,
    status: u16,
    code: ErrorCode,
) -> Result<()> {
    let client = GatewayClient::with_options(
        &harness.endpoint,
        None,
        TransportOptions {
            http_version: HttpVersion::Http1Only,
            request_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        },
    )?;
    let request: ChatRequest = serde_json::from_value(json!({
        "model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":false
    }))?;
    let error = client
        .ordered_tenant(harness.token()?, binding)?
        .chat_reply(&request)
        .await
        .err()
        .context("unauthorized ordered dispatch succeeded")?;
    ensure!(
        error.dispatch_state() == DispatchState::NotStarted,
        "{error:?}"
    );
    let DispatchError::Response(failure) = error else {
        anyhow::bail!("rejection did not come from the HTTP gateway");
    };
    ensure!(failure.problem.is_none(), "{failure:?}");
    ensure!(failure.metadata.status == status, "{failure:?}");
    ensure!(failure.metadata.execution_id.is_none());
    ensure!(failure.metadata.error_source.is_none());
    let body = failure.tenancy_error.context("typed tenancy rejection")?;
    ensure!(body.code == code, "unexpected rejection: {body:?}");
    ensure!(body.execution_id.is_none());
    ensure!(
        harness.observed()?.is_empty(),
        "rejected dispatch reached a provider"
    );
    Ok(())
}

#[tokio::test]
async fn selected_subset_survives_removal_of_unselected_credential_scope() -> Result<()> {
    let harness = OrderedHarness::new(3, 64, None).await?;
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    // The original token still names the whole tenant. Its live binding now
    // grants only the route and slot 0, excluding the other retained dependencies.
    narrow_binding(&harness, &[0]).await?;
    let id = harness
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await?;
    // Receipt inspection has its own whole-tenant read grant; the narrowed
    // inference credential proves only the selected execution authorization.
    let receipt = harness
        .control(true, HttpVersion::Http1Only)?
        .execution_v2(&harness.fixture.installation, &harness.fixture.tenant, &id)
        .await?;
    ensure!(receipt.state == ReceiptState::Committed && receipt.outcome == Outcome::Completed);
    ensure!(receipt.accepted_selection.candidate_indices == [0]);
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].candidate_index == 0);
    ensure!(harness.observed()? == ["model-0"]);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn selected_credential_requires_its_own_ingress_credential_grant() -> Result<()> {
    // Even when the first candidate is authorized and would succeed, the
    // complete selected set must be authorized before that first provider send.
    for (selected, granted) in [(vec![0], vec![1]), (vec![0, 1], vec![0])] {
        let mut harness = OrderedHarness::new(2, 64, None).await?;
        let binding = harness
            .binding(selected, GenerationProtocol::ChatCompletions)
            .await?;
        harness.credential = scoped_key(&harness, "selected-slot-scope", &granted).await?;
        reject_dispatch(&harness, &binding, 403, ErrorCode::Forbidden).await?;
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn selected_route_requires_exact_revision_and_owner_before_sending() -> Result<()> {
    let harness = OrderedHarness::new(1, 64, None).await?;
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    for field in ["revision", "tenant", "pool"] {
        let mut wrong = binding.clone();
        match field {
            "revision" => wrong.selection.route.revision_id = "unretained".try_into()?,
            "tenant" => wrong.selection.route.owner_id = "foreign-tenant".try_into()?,
            _ => wrong.selection.route.owner_kind = OwnerKind::OperatorPool,
        }
        reject_dispatch(&harness, &wrong, 409, ErrorCode::CapabilityMismatch).await?;
    }
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn candidate_manifest_denies_tenant_credentials_even_with_admin_permissions() -> Result<()> {
    let harness = OrderedHarness::new(2, 64, None).await?;
    let fixture = &harness.fixture;
    let operator = harness.control(true, HttpVersion::Http1Only)?;
    let manifest = operator
        .candidate_manifest(
            &fixture.installation,
            &fixture.tenant,
            &harness.revision.revision_id,
            &harness.route,
            GenerationProtocol::ChatCompletions,
        )
        .await?;
    ensure!(manifest.candidates.len() == 2);
    let error = harness
        .control(false, HttpVersion::Http1Only)?
        .candidate_manifest(
            &fixture.installation,
            &fixture.tenant,
            &harness.revision.revision_id,
            &harness.route,
            GenerationProtocol::ChatCompletions,
        )
        .await
        .err()
        .context("tenant credential read operator-only manifest")?;
    let ControlError::Tenancy(failure) = error else {
        anyhow::bail!("manifest denial did not come from HTTP");
    };
    ensure!(failure.status == 403 && failure.body_error.is_none());
    ensure!(!failure.headers.contains_key("x-bitrouter-request-id"));
    ensure!(!failure.headers.contains_key("x-bitrouter-error-source"));
    let body = failure.body.context("typed manifest denial")?;
    ensure!(body.code == ErrorCode::Forbidden && body.dispatch_state == DispatchState::NotStarted);
    ensure!(body.execution_id.is_none());
    ensure!(harness.observed()?.is_empty());
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn authority_revocation_after_binding_prevents_any_provider_send() -> Result<()> {
    for change in ["scope", "credential", "tenant"] {
        let harness = OrderedHarness::new(2, 64, None).await?;
        let binding = harness
            .binding(vec![0], GenerationProtocol::ChatCompletions)
            .await?;
        let fixture = &harness.fixture;
        let control = harness.control(true, HttpVersion::Http1Only)?;
        let (status, code) = match change {
            "scope" => {
                narrow_binding(&harness, &[1]).await?;
                (403, ErrorCode::Forbidden)
            }
            "credential" => {
                let operation = control
                    .revoke_credential(
                        &fixture.tenant,
                        &harness.credential.credential_id,
                        &Mutation {
                            operation_id: "revoke-ordered-credential".try_into()?,
                            expected_revision: harness.credential.credential_revision.clone(),
                            installation_id: fixture.installation.clone(),
                        },
                    )
                    .await?;
                ensure!(operation.status == OperationStatus::Applied);
                (401, ErrorCode::Unauthorized)
            }
            _ => {
                let current = fixture.service.current().map_err(failure)?;
                let tenant = current
                    .tenants
                    .get(&fixture.tenant)
                    .context("current tenant")?;
                let operation = control
                    .put_tenant(
                        &fixture.tenant,
                        &TenantPut {
                            operation_id: "suspend-ordered-tenant".try_into()?,
                            expected_revision: Some(tenant.revision.clone()),
                            state: TenantState::Suspended,
                            installation_id: fixture.installation.clone(),
                        },
                    )
                    .await?;
                ensure!(operation.status == OperationStatus::Applied);
                (403, ErrorCode::TenantInactive)
            }
        };
        reject_dispatch(&harness, &binding, status, code).await?;
        fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn selection_and_host_attempt_limits_reject_without_provider_sends() -> Result<()> {
    for (count, limit) in [(9, 8), (65, 64)] {
        let harness = OrderedHarness::new(count, limit, None).await?;
        let mut binding = harness
            .binding(vec![0], GenerationProtocol::ChatCompletions)
            .await?;
        binding.selection.candidate_indices = (0..u16::try_from(count)?).collect();
        if count == 9 {
            // A manually constructed binding cannot bypass the current host ceiling.
            reject_dispatch(&harness, &binding, 429, ErrorCode::CapacityExceeded).await?;
        } else {
            let client = GatewayClient::new(&harness.endpoint, None)?;
            ensure!(matches!(
                client.ordered_tenant(harness.token()?, &binding),
                Err(DispatchError::Request(_))
            ));
            // Exercise server validation as well as the SDK's local bound.
            // https://www.rfc-editor.org/rfc/rfc4648.html#section-5
            let selection = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&binding.selection)?);
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(Duration::from_secs(30))
                .build()?;
            let mut request = client
                .post(format!("{}/v1/chat/completions", harness.endpoint))
                .bearer_auth(harness.token()?)
                .header("Bitrouter-Candidate-Selection", selection);
            let encoded = serde_json::to_value(&binding)?;
            for (name, value) in encoded.as_object().context("binding fields")? {
                if name != "selection" {
                    request =
                        request.header(name.as_str(), value.as_str().context("binding header")?);
                }
            }
            let response = request
                .json(&json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":false}))
                .send()
                .await?;
            ensure!(response.status() == StatusCode::BAD_REQUEST);
            ensure!(!response.headers().contains_key("x-bitrouter-request-id"));
            let body: bitrouter_ai::tenancy::operation::Error = response.json().await?;
            body.validate()?;
            ensure!(
                body.code == ErrorCode::InvalidRequest
                    && body.dispatch_state == DispatchState::NotStarted
            );
            ensure!(body.execution_id.is_none());
            ensure!(harness.observed()?.is_empty());
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_scoped_callers_preserve_order_and_immutable_configuration() -> Result<()> {
    let harness = OrderedHarness::new(3, 64, None).await?;
    // Each principal has two live credential slots. Retire the fixture's unused
    // broad key so both scoped callers fit the unchanged authority capacity.
    let retired = harness
        .control(true, HttpVersion::Http1Only)?
        .revoke_credential(
            &harness.fixture.tenant,
            &harness.credential.credential_id,
            &Mutation {
                operation_id: "retire-concurrent-fixture-key".try_into()?,
                expected_revision: harness.credential.credential_revision.clone(),
                installation_id: harness.fixture.installation.clone(),
            },
        )
        .await?;
    ensure!(retired.status == OperationStatus::Applied);
    let key_a = scoped_key(&harness, "concurrent-a", &[2, 0]).await?;
    let key_b = scoped_key(&harness, "concurrent-b", &[1, 0]).await?;
    let token_a = key_a.secret.as_deref().context("caller A secret")?;
    let token_b = key_b.secret.as_deref().context("caller B secret")?;
    let protocol = GenerationProtocol::ChatCompletions;
    let mut binding_a = harness.binding(vec![2, 0], protocol).await?;
    binding_a.correlation_id = "caller-a".try_into()?;
    let mut binding_b = harness.binding(vec![1, 0], protocol).await?;
    binding_b.correlation_id = "caller-b".try_into()?;
    let fixture = &harness.fixture;
    let operator = harness.control(true, HttpVersion::Http1Only)?;
    let manifest = || {
        operator.candidate_manifest(
            &fixture.installation,
            &fixture.tenant,
            &harness.revision.revision_id,
            &harness.route,
            protocol,
        )
    };
    let configuration = || {
        operator.configuration(
            &fixture.installation,
            &fixture.tenant,
            &harness.revision.revision_id,
        )
    };
    let original_manifest = serde_json::to_value(manifest().await?)?;
    let original_configuration = serde_json::to_value(configuration().await?)?;

    let client = GatewayClient::with_options(
        &harness.endpoint,
        None,
        TransportOptions {
            http_version: HttpVersion::Http1Only,
            request_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        },
    )?;
    let request: ChatRequest = serde_json::from_value(json!({
        "model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":false
    }))?;
    let caller_a = client.ordered_tenant(token_a, &binding_a)?;
    let caller_b = client.ordered_tenant(token_b, &binding_b)?;
    let (reply_a, reply_b) =
        tokio::join!(caller_a.chat_reply(&request), caller_b.chat_reply(&request));
    let mut ids = Vec::new();
    for reply in [reply_a?, reply_b?] {
        ensure!(reply.metadata.status == 200);
        let Body::Json(body) = reply.body else {
            anyhow::bail!("nonstreaming concurrent call returned SSE");
        };
        ensure!(serde_json::to_string(&body)?.contains("receipt reply"));
        ids.push(
            reply
                .metadata
                .execution_id
                .context("concurrent execution identity")?,
        );
    }
    ensure!(ids[0] != ids[1]);
    for (id, binding) in ids.iter().zip([&binding_a, &binding_b]) {
        let receipt = operator
            .execution_v2(&fixture.installation, &fixture.tenant, id)
            .await?;
        ensure!(receipt.state == ReceiptState::Committed && receipt.outcome == Outcome::Completed);
        ensure!(receipt.correlation_id == binding.correlation_id);
        ensure!(receipt.execution_revision == harness.revision.revision_id);
        ensure!(receipt.accepted_selection == binding.selection.accept(protocol)?);
        ensure!(
            receipt
                .attempts
                .iter()
                .map(|attempt| attempt.candidate_index)
                .collect::<Vec<_>>()
                == binding.selection.candidate_indices
        );
        ensure!(
            receipt
                .attempts
                .iter()
                .map(|attempt| attempt.observed_http_status)
                .collect::<Vec<_>>()
                == [Some(503), Some(200)]
        );
        let evidence = operator.usage_evidence_v2(&receipt).await?;
        evidence.validate_against(&receipt)?;
        ensure!(evidence.attempts.len() == 2);
    }

    // Reusing the pool and retained route must not union the two callers' grants.
    for (token, foreign_binding) in [(token_a, &binding_b), (token_b, &binding_a)] {
        let error = client
            .ordered_tenant(token, foreign_binding)?
            .chat_reply(&request)
            .await
            .err()
            .context("concurrent caller acquired the other credential scope")?;
        ensure!(error.dispatch_state() == DispatchState::NotStarted);
        let DispatchError::Response(failure) = error else {
            anyhow::bail!("cross-scope rejection did not come from HTTP");
        };
        ensure!(failure.metadata.status == 403 && failure.problem.is_none());
        ensure!(failure.metadata.execution_id.is_none() && failure.metadata.error_source.is_none());
        ensure!(
            failure
                .tenancy_error
                .is_some_and(|error| error.code == ErrorCode::Forbidden)
        );
    }
    let mut observed = harness.observed()?;
    observed.sort_unstable();
    ensure!(observed == ["model-0", "model-0", "model-1", "model-2"]);
    ensure!(serde_json::to_value(manifest().await?)? == original_manifest);
    ensure!(serde_json::to_value(configuration().await?)? == original_configuration);
    fixture.service.shutdown().await?;
    Ok(())
}
