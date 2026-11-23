//! File defaults reach actual configured sends without replacing caller values.

use super::*;

const DEFAULTS_SELECTOR: &str = "bitrouter/file-defaults";
const ALTERNATE_ROUTE: &str = "model.acme.alternate-defaults";

async fn with_file_defaults(url: &str) -> Result<Harness> {
    let mut harness = Harness::new(url).await?;
    // Parse ordinary router YAML, then restart the real owning service with it.
    let mut configured = crate::config::parse(
        r#"
inherit_defaults: false
providers:
  mock:
    api_protocol: [{"*": [chat_completions, ordered_generate_content]}]
routers:
  file-defaults:
    selection: {kind: model, model: acme/shared}
    defaults:
      params: {max_tokens: 37, temperature: 0.25, top_p: 0.75}
  alternate-defaults:
    selection: {kind: model, model: acme/shared}
    defaults:
      params: {max_tokens: 53, temperature: 0.5, top_p: 0.625}
"#,
    )?;
    harness
        .config
        .providers
        .get_mut("mock")
        .context("configured provider")?
        .api_protocol = configured
        .providers
        .remove("mock")
        .context("native-capable provider")?
        .api_protocol;
    harness = harness.restart().await?;
    harness
        .fixture
        .service
        .catalog_candidates(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            ROUTE,
            GenerationProtocol::GenerateContent,
        )
        .await
        .map_err(failure)?;
    harness.config.routers = configured.routers;
    let catalog = harness
        .config
        .tenancy
        .catalog
        .as_mut()
        .context("configured catalog missing")?;
    catalog
        .routes
        .insert(ROUTE.into(), DEFAULTS_SELECTOR.into());
    catalog.routes.insert(
        ALTERNATE_ROUTE.into(),
        "bitrouter/alternate-defaults".into(),
    );
    ensure!(catalog.profiles.len() == 1 && catalog.profiles.contains_key(SOURCE));
    harness.restart().await
}

async fn dispatch_protocol(
    fixture: &Fixture,
    credential: &CredentialIssued,
    descriptor: &CatalogCandidatesV1,
    path: &str,
    body: Value,
) -> Result<()> {
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let mut request = Request::post(path).body(Body::from(serde_json::to_vec(&body)?))?;
    *request.headers_mut() = headers(fixture, credential, descriptor).await?;
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    let response = tokio::time::timeout(TIMEOUT, router.oneshot(request)).await??;
    let status = response.status();
    let execution = response
        .headers()
        .get("x-bitrouter-request-id")
        .map(|value| -> Result<ExecutionId> { Ok(value.to_str()?.try_into()?) })
        .transpose()?;
    let body = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await??;
    ensure!(
        status == StatusCode::OK,
        "configured defaults dispatch failed: {status} {}",
        String::from_utf8_lossy(&body)
    );
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    let receipt = receipt(fixture, execution.context("configured execution header")?).await?;
    ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
    Ok(())
}

async fn actual_defaults(protocol: GenerationProtocol, path: &str) -> Result<()> {
    let mut provider = Provider::start().await?;
    let mut harness = with_file_defaults(&provider.url).await?;
    ensure!(
        harness
            .fixture
            .service
            .catalog_candidates(
                &harness.fixture.operator,
                &harness.fixture.tenant,
                ROUTE,
                GenerationProtocol::GenerateContent,
            )
            .await
            .is_err(),
        "native configured exposure must refuse file defaults"
    );
    let descriptor = harness
        .fixture
        .service
        .catalog_candidates(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            ROUTE,
            protocol,
        )
        .await
        .map_err(failure)?;
    let credential = grant(
        &mut harness.fixture,
        scope(&descriptor),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "file-defaults",
    )
    .await?;
    let before = publication_counts(&harness.fixture).await?;
    for explicit in [false, true] {
        let mut body = match protocol {
            GenerationProtocol::ChatCompletions | GenerationProtocol::Messages => {
                json!({"model":ROUTE,"messages":[{"role":"user","content":"hello"}]})
            }
            GenerationProtocol::Responses => json!({"model":ROUTE,"input":"hello"}),
            GenerationProtocol::GenerateContent => anyhow::bail!("native defaults are separate"),
        };
        if explicit {
            let token_field = match protocol {
                GenerationProtocol::ChatCompletions => "max_completion_tokens",
                GenerationProtocol::Messages => "max_tokens",
                GenerationProtocol::Responses => "max_output_tokens",
                GenerationProtocol::GenerateContent => {
                    anyhow::bail!("native defaults are separate")
                }
            };
            body[token_field] = json!(11);
            body["temperature"] = json!(0.85);
            body["top_p"] = json!(0.95);
        }
        dispatch_protocol(&harness.fixture, &credential, &descriptor, path, body).await?;
        let (authorization, sent) = tokio::time::timeout(TIMEOUT, provider.received.recv())
            .await?
            .context("actual provider request missing")?;
        ensure!(authorization == format!("Bearer {SECRET}"));
        ensure!(sent["model"] == "native-shared");
        ensure!(
            sent.get(bitrouter_ai::tenancy::catalog::BODY_MEMBER)
                .is_none()
        );
        let token_field = if explicit && protocol == GenerationProtocol::ChatCompletions {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        let other_field = if token_field == "max_tokens" {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        ensure!(sent[token_field] == if explicit { json!(11) } else { json!(37) });
        ensure!(sent.get(other_field).is_none() && sent.get("max_output_tokens").is_none());
        ensure!(sent["temperature"] == if explicit { json!(0.85) } else { json!(0.25) });
        ensure!(sent["top_p"] == if explicit { json!(0.95) } else { json!(0.75) });
    }
    ensure!(provider.calls() == 2);
    ensure!(publication_counts(&harness.fixture).await? == before);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn configured_chat_defaults_fill_omissions_and_preserve_explicit_typed_values() -> Result<()>
{
    actual_defaults(GenerationProtocol::ChatCompletions, "/v1/chat/completions").await
}

#[tokio::test]
async fn configured_messages_defaults_fill_omissions_and_preserve_explicit_typed_values()
-> Result<()> {
    actual_defaults(GenerationProtocol::Messages, "/v1/messages").await
}

#[tokio::test]
async fn configured_responses_defaults_fill_omissions_and_preserve_explicit_typed_values()
-> Result<()> {
    actual_defaults(GenerationProtocol::Responses, "/v1/responses").await
}

#[tokio::test]
async fn defaults_routers_share_canonical_profile_but_keep_distinct_roots_and_sends() -> Result<()>
{
    let mut provider = Provider::start().await?;
    let mut harness = with_file_defaults(&provider.url).await?;
    let first = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let second = harness
        .fixture
        .service
        .catalog_candidates(
            &harness.fixture.operator,
            &harness.fixture.tenant,
            ALTERNATE_ROUTE,
            GenerationProtocol::ChatCompletions,
        )
        .await
        .map_err(failure)?;
    ensure!(first.manifest.candidates.len() == 1 && second.manifest.candidates.len() == 1);
    ensure!(first.templates.len() == 1 && second.templates.len() == 1);
    let first_template = &first.templates[0];
    let second_template = &second.templates[0];
    ensure!(first_template.source_id == SOURCE && second_template.source_id == SOURCE);
    ensure!(
        first_template.platform_profile == second_template.platform_profile
            && first_template.byok_profile == second_template.byok_profile
            && first_template.credential_ref == second_template.credential_ref
    );
    ensure!(
        first.manifest.candidates[0].admission_profile.is_some()
            && first.manifest.candidates[0].admission_profile
                == second.manifest.candidates[0].admission_profile
    );
    ensure!(
        first.manifest.execution_revision == second.manifest.execution_revision
            && first.manifest.route.key() != second.manifest.route.key()
            && first.manifest.route.revision_id != second.manifest.route.revision_id
            && first_template.template_id != second_template.template_id
    );
    let GrantScope::Resources { mut resources } = scope(&first) else {
        anyhow::bail!("first configured dependency scope");
    };
    let GrantScope::Resources { resources: other } = scope(&second) else {
        anyhow::bail!("second configured dependency scope");
    };
    for key in other {
        if !resources.contains(&key) {
            resources.push(key);
        }
    }
    let credential = grant(
        &mut harness.fixture,
        GrantScope::Resources { resources },
        TenantRole::TenantAdmin.permissions().to_vec(),
        "canonical-defaults-executor",
    )
    .await?;
    let before = publication_counts(&harness.fixture).await?;
    for (descriptor, max_tokens, temperature, top_p) in
        [(&first, 37, 0.25, 0.75), (&second, 53, 0.5, 0.625)]
    {
        dispatch_protocol(
            &harness.fixture,
            &credential,
            descriptor,
            "/v1/chat/completions",
            json!({"model":descriptor.manifest.route.resource_id,
                "messages":[{"role":"user","content":"hello"}]}),
        )
        .await?;
        let (authorization, sent) = tokio::time::timeout(TIMEOUT, provider.received.recv())
            .await?
            .context("router defaults provider send")?;
        ensure!(authorization == format!("Bearer {SECRET}") && sent["model"] == "native-shared");
        ensure!(
            sent["max_tokens"] == max_tokens
                && sent["temperature"] == temperature
                && sent["top_p"] == top_p
        );
        ensure!(
            sent.get(bitrouter_ai::tenancy::catalog::BODY_MEMBER)
                .is_none()
        );
    }
    ensure!(provider.calls() == 2 && publication_counts(&harness.fixture).await? == before);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn configured_router_policies_and_request_checks_are_refused_explicitly() -> Result<()> {
    let provider = Provider::start().await?;
    for (kind, router_yaml) in [
        (
            "policy",
            r#"
routers:
  guarded:
    selection: {kind: policy, policy: guarded-policy, base_model: acme/shared}
"#,
        ),
        (
            "checks",
            r#"
checkers:
  input-guard:
    native: {revision: guard-v1}
routers:
  guarded:
    selection: {kind: model, model: acme/shared}
    checks:
      request: [{checker: input-guard}]
"#,
        ),
    ] {
        let mut harness = Harness::new(&provider.url).await?;
        harness.config = crate::config::parse(&format!(
            "{}\n{router_yaml}",
            catalog_fixture_yaml(&provider.url, SECRET)
        ))?;
        harness
            .config
            .tenancy
            .catalog
            .as_mut()
            .context("catalog exposure")?
            .routes
            .insert(ROUTE.into(), "bitrouter/guarded".into());
        let resolved = harness.config.resolve_router("bitrouter/guarded")?;
        ensure!(resolved.clean_model == "acme/shared");
        if kind == "policy" {
            ensure!(
                resolved.policy.as_deref() == Some("guarded-policy")
                    && resolved.request_checks.is_empty()
            );
        } else {
            ensure!(resolved.policy.is_none() && resolved.request_checks.len() == 1);
            ensure!(resolved.request_checks[0].checker_id == "input-guard");
        }
        // This is the actual Service startup compiler, after ordinary YAML and
        // router resolution succeeded; missing host registrations cannot mask it.
        let error = match harness.restart().await {
            Err(error) => error,
            Ok(harness) => {
                harness.fixture.service.shutdown().await?;
                anyhow::bail!("configured {kind} router silently lost its behavior");
            }
        };
        ensure!(
            error
                .to_string()
                .contains("does not support router policies or request checks"),
            "wrong configured {kind} refusal: {error:#}"
        );
        ensure!(provider.calls() == 0);
    }
    Ok(())
}

#[test]
fn configured_health_group_requires_identical_rules_but_allows_distinct_member_rpm() -> Result<()> {
    use crate::config::tenancy::catalog::CatalogConfig;
    let config: CatalogConfig = serde_saphyr::from_str(
        r#"
pool_id: platform
routes: {model.acme.shared: acme/shared}
profiles:
  mock/primary/acme/shared:
    schema_version: 1
    rate: {rate_group_id: upstream, member_rpm: 100}
    health:
      rules: availability_v1
      failure_threshold: 3
      transient_cooldown_seconds: 30
      persistent_cooldown_seconds: 300
      probe_lease_seconds: 10
    health_group_id: mock.acme.shared
    fallback: ordered_terminal_rejections_v1
  mock/backup/acme/shared:
    schema_version: 1
    rate: {rate_group_id: upstream, member_rpm: 7}
    health:
      rules: availability_v1
      failure_threshold: 3
      transient_cooldown_seconds: 30
      persistent_cooldown_seconds: 300
      probe_lease_seconds: 10
    health_group_id: mock.acme.shared
    fallback: ordered_terminal_rejections_v1
"#,
    )?;
    config.validate()?;
    let first = config
        .profiles
        .get("mock/primary/acme/shared")
        .context("primary member")?;
    let second = config
        .profiles
        .get("mock/backup/acme/shared")
        .context("backup member")?;
    ensure!(first.rate.as_ref().context("primary rate")?.member_rpm == 100);
    ensure!(second.rate.as_ref().context("backup rate")?.member_rpm == 7);
    for field in [
        "failure_threshold",
        "transient_cooldown_seconds",
        "persistent_cooldown_seconds",
        "probe_lease_seconds",
    ] {
        let mut conflict = config.clone();
        let member = conflict
            .profiles
            .get_mut("mock/backup/acme/shared")
            .context("backup member")?;
        let health = member.health.as_mut().context("backup health")?;
        match field {
            "failure_threshold" => health.failure_threshold += 1,
            "transient_cooldown_seconds" => health.transient_cooldown_seconds += 1,
            "persistent_cooldown_seconds" => health.persistent_cooldown_seconds += 1,
            "probe_lease_seconds" => health.probe_lease_seconds += 1,
            _ => anyhow::bail!("unexpected health field"),
        }
        // Each individual member remains valid: refusal must come from sharing
        // one group with conflicting complete rules, not malformed input.
        member.validate()?;
        let error = conflict
            .validate()
            .err()
            .context("conflicting health group accepted")?;
        ensure!(
            error.to_string().contains("conflicting rules"),
            "wrong refusal for {field}"
        );
    }
    Ok(())
}
