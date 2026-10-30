use super::*;
use crate::gateway::{model::target::AuthScheme, pipeline::types::OutboundHeaderRule};
use bitrouter_ai::types::{ChatTokenLimitField, ReasoningEffortConfig};

async fn fixture_row() -> Result<(DatabaseConnection, Resources, StoredConfiguration)> {
    let (db, resources) = fixture().await?;
    let mut input = route("tenant", "visible", "model")?;
    input["definition"]["routing"]["providers"]["fixture"]["headers"] =
        json!({"x-private":"PRIVATE_HEADER_VALUE", "x-second":"second"});
    import(&resources, &db, vec![credential("tenant")?, input]).await?;
    let row = retained(
        &resources,
        &db,
        "tenant",
        "config-v1",
        document(
            vec![reference("tenant", ResourceKind::Route, "visible")?],
            None,
        )?,
    )
    .await?;
    Ok((db, resources, row))
}

#[tokio::test]
async fn every_target_field_and_optional_or_header_distinction_is_bound() -> Result<()> {
    let (_db, resources, row) = fixture_row().await?;
    let view = checked(resources.open_execution(&row))?;
    let route = selected(&view, "visible", ApiProtocol::ChatCompletions)?;
    let target = first(&route, "visible", ApiProtocol::ChatCompletions).await?;
    assert!(target.registry.is_none());
    assert!(route.match_candidate(&target).is_ok());
    let mut registry_target = target.clone();
    registry_target.registry = Some(Arc::new(
        crate::gateway::catalog::execution::SelectedInterface {
            provider_id: target.provider_name.clone(),
            provider_model_id: target.service_id.clone(),
            tariff_endpoint: Some(target.api_base.clone()),
            verified_profile: None,
            billing: None,
            interface: registry_json::json!({}),
            auth: registry_json::json!({"kind": "none"}),
            canonical_id: None,
        },
    ));
    assert!(compiler::target_value(&registry_target).is_err());
    assert!(matches!(
        route.match_candidate(&registry_target),
        Err(ErrorCode::Forbidden)
    ));
    macro_rules! reject {
        ($field:ident, $value:expr) => {{
            let mut changed = target.clone();
            changed.$field = $value;
            assert!(
                matches!(route.match_candidate(&changed), Err(ErrorCode::Forbidden)),
                stringify!($field)
            );
        }};
    }
    reject!(provider_name, "different".into());
    reject!(service_id, "different".into());
    reject!(api_base, format!("{}/", target.api_base));
    reject!(api_key, "injected".into());
    reject!(api_protocol, ApiProtocol::Messages);
    reject!(chat_token_limit_field, Some(ChatTokenLimitField::MaxTokens));
    reject!(
        chat_token_limit_field,
        Some(ChatTokenLimitField::MaxCompletionTokens)
    );
    reject!(chat_supports_store, Some(true));
    reject!(chat_supports_store, Some(false));
    reject!(chat_supports_stream_options, Some(true));
    reject!(chat_supports_stream_options, Some(false));
    reject!(chat_google_extensions, !target.chat_google_extensions);
    let reasoning: ReasoningEffortConfig = serde_json::from_value(json!({"levels":["low"]}))?;
    reject!(reasoning_effort, Some(reasoning));
    let mut constraints = target.model_constraints.clone();
    constraints.capabilities.push(Capability::Tools);
    reject!(model_constraints, constraints);
    let mut constraints = target.model_constraints.clone();
    constraints.source = Some("other source".into());
    reject!(model_constraints, constraints);
    reject!(account_label, Some(String::new()));
    reject!(
        account_label,
        if target.account_label.is_none() {
            Some("default".into())
        } else {
            None
        }
    );
    reject!(api_key_override, Some(String::new()));
    reject!(api_base_override, Some(target.api_base.clone()));
    reject!(
        auth_scheme,
        match target.auth_scheme {
            AuthScheme::Bearer => AuthScheme::XApiKey,
            AuthScheme::XApiKey => AuthScheme::Bearer,
        }
    );
    assert_eq!(target.headers.len(), 2);
    let mut headers = target.headers.clone();
    headers.reverse();
    reject!(headers, headers);
    for default in [None, Some(""), Some("changed")] {
        let mut headers = target.headers.clone();
        headers[0] = OutboundHeaderRule::new(headers[0].name().as_str(), default, false)?;
        reject!(headers, headers);
    }
    let mut headers = target.headers.clone();
    headers[0] = OutboundHeaderRule::new("x-different", Some("PRIVATE_HEADER_VALUE"), false)?;
    reject!(headers, headers);
    let mut headers = target.headers.clone();
    headers[0] = OutboundHeaderRule::new(
        headers[0].name().as_str(),
        Some("PRIVATE_HEADER_VALUE"),
        true,
    )?;
    reject!(headers, headers);
    Ok(())
}

fn plaintext(resources: &Resources, row: &StoredConfiguration) -> Result<Value> {
    let sealed = serde_json::from_value(row.compiled["sealed"].clone())?;
    let bytes = resources
        .key
        .as_ref()
        .context("missing fixture key")?
        .open_bound(
            "configuration",
            &(
                &resources.installation,
                &row.revision.tenant_id,
                &row.revision.revision_id,
                COMPILER_VERSION,
                &row.revision.document,
                &row.dependencies,
            ),
            &sealed,
        )?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn mutate_targets(envelope: &mut Value, mutate: impl Fn(&mut Value)) -> Result<()> {
    fn chains(route: &mut Value, mutate: &impl Fn(&mut Value)) -> Result<()> {
        for chain in route["chains"].as_array_mut().context("missing chains")? {
            for target in chain["targets"].as_array_mut().context("missing targets")? {
                mutate(&mut target["target"]);
            }
        }
        Ok(())
    }
    for route in envelope["routes"]
        .as_array_mut()
        .context("missing routes")?
    {
        chains(route, &mutate)?;
    }
    for artifact in envelope["artifacts"]
        .as_array_mut()
        .context("missing artifacts")?
    {
        if !artifact["envelope"].is_null() {
            chains(&mut artifact["envelope"], &mutate)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn authenticated_but_corrupt_typed_target_fields_fail_before_execution() -> Result<()> {
    let (_db, resources, row) = fixture_row().await?;
    let original = plaintext(&resources, &row)?;
    assert!(
        original["routes"][0]["chains"][0]["targets"][0]["target"]
            .get("registry")
            .is_none()
    );
    let cases = [
        ("registry", json!(null)),
        ("registry", json!({"auth": {"kind": "none"}})),
        ("auth_scheme", json!("unknown")),
        ("chat_token_limit_field", json!("unknown")),
        ("chat_supports_store", json!("false")),
        ("chat_supports_stream_options", json!(0)),
        ("chat_google_extensions", json!(null)),
        ("account", json!({"label":"default"})),
        ("reasoning_effort", json!({"levels":[],"default":null})),
        (
            "headers",
            json!([{"name":"authorization","default":"injected","passthrough":false}]),
        ),
        (
            "headers",
            json!([{"name":"x-private","default":"secret","passthrough":true}]),
        ),
        (
            "headers",
            json!([{"name":"X-Private","default":"secret","passthrough":false}]),
        ),
    ];
    for (field, value) in cases {
        let mut envelope = original.clone();
        mutate_targets(&mut envelope, |target| {
            target[field] = value.clone();
        })?;
        let mut changed = row.clone();
        validation::reseal(&resources, &mut changed, &envelope)?;
        assert!(
            matches!(
                resources.validate_compiled(&changed),
                Err(ErrorCode::AuthorityUnavailable)
            ),
            "{field}"
        );
        assert!(
            matches!(
                resources.open_execution(&changed),
                Err(ErrorCode::AuthorityUnavailable)
            ),
            "{field}"
        );
    }
    for missing in ["account", "chat_supports_store", "reasoning_effort"] {
        let mut envelope = original.clone();
        mutate_targets(&mut envelope, |target| {
            if let Some(fields) = target.as_object_mut() {
                fields.remove(missing);
            }
        })?;
        let mut changed = row.clone();
        validation::reseal(&resources, &mut changed, &envelope)?;
        assert!(matches!(
            resources.open_execution(&changed),
            Err(ErrorCode::AuthorityUnavailable)
        ));
    }
    let mut envelope = original;
    mutate_targets(&mut envelope, |target| {
        target["model_constraints"]["unknown_authority"] = json!(true);
    })?;
    let mut changed = row;
    validation::reseal(&resources, &mut changed, &envelope)?;
    assert!(matches!(
        resources.open_execution(&changed),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    Ok(())
}

#[tokio::test]
async fn identical_targets_with_distinct_credential_refs_are_ambiguous_even_for_equal_secrets()
-> Result<()> {
    let (db, resources, mut row) = fixture_row().await?;
    let second_ref = reference("tenant", ResourceKind::Credential, "second-slot")?;
    let mut second = credential("tenant")?;
    second["reference"] = serde_json::to_value(&second_ref)?;
    import(&resources, &db, vec![second]).await?;
    assert_eq!(
        checked(resources.credential(&db, &second_ref).await)?,
        checked(
            resources
                .credential(&db, &reference("tenant", ResourceKind::Credential, "slot")?)
                .await
        )?
    );
    let mut envelope = plaintext(&resources, &row)?;
    row.dependencies.push(second_ref.clone());
    let binding = json!({"provider":"extra","account":"default","credential":second_ref});
    fn duplicate(route: &mut Value, reference: &ResourceRef) -> Result<()> {
        let targets = route["chains"][0]["targets"]
            .as_array_mut()
            .context("missing targets")?;
        let mut second = targets.first().context("missing target")?.clone();
        second["credential"] = serde_json::to_value(reference)?;
        targets.push(second);
        Ok(())
    }
    duplicate(&mut envelope["routes"][0], &second_ref)?;
    for artifact in envelope["artifacts"]
        .as_array_mut()
        .context("missing artifacts")?
    {
        if !artifact["envelope"].is_null() {
            artifact["definition"]["credentials"]
                .as_array_mut()
                .context("missing bindings")?
                .push(binding.clone());
            duplicate(&mut artifact["envelope"], &second_ref)?;
        }
    }
    validation::reseal(&resources, &mut row, &envelope)?;
    let error = crate::tenancy::resources::validation::compiled(&resources, &row)
        .err()
        .context("ambiguous credential accepted")?;
    assert_eq!(error.to_string(), "ambiguous retained target credential");
    assert!(matches!(
        resources.open_execution(&row),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    Ok(())
}

#[tokio::test]
async fn equal_concrete_targets_in_different_routes_keep_their_own_credential_ref() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut second_credential = credential("tenant")?;
    let second_ref = reference("tenant", ResourceKind::Credential, "second-slot")?;
    second_credential["reference"] = serde_json::to_value(&second_ref)?;
    let mut second_route = route("tenant", "b", "same")?;
    second_route["definition"]["credentials"][0]["credential"] = serde_json::to_value(&second_ref)?;
    import(
        &resources,
        &db,
        vec![
            credential("tenant")?,
            second_credential,
            route("tenant", "a", "same")?,
            second_route,
        ],
    )
    .await?;
    let row = retained(
        &resources,
        &db,
        "tenant",
        "equal-targets",
        document(
            vec![
                reference("tenant", ResourceKind::Route, "a")?,
                reference("tenant", ResourceKind::Route, "b")?,
            ],
            None,
        )?,
    )
    .await?;
    let view = checked(resources.open_execution(&row))?;
    let a = selected(&view, "a", ApiProtocol::ChatCompletions)?;
    let b = selected(&view, "b", ApiProtocol::ChatCompletions)?;
    let target = first(&a, "a", ApiProtocol::ChatCompletions).await?;
    assert_eq!(
        checked(a.match_candidate(&target))?,
        reference("tenant", ResourceKind::Credential, "slot")?
    );
    assert_eq!(checked(b.match_candidate(&target))?, second_ref);
    assert!(!a.dependencies().contains(&second_ref));
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
#[tokio::test]
async fn canonical_defaults_allow_token_tariffs_but_unknown_overrides_do_not() -> Result<()> {
    use crate::gateway::model::protocol::{
        InboundAdapter, chat_completions::ChatCompletionsAdapter,
    };

    for extra_default in [false, true] {
        let (db, resources) = fixture().await?;
        let mut input = route("tenant", "visible", "model")?;
        if extra_default {
            input["definition"]["selector"] = json!("bitrouter/with-extra");
            input["definition"]["routing"]["routers"] = json!({"with-extra": {
                "selection":{"kind":"model","model":"fixture:model"},
                "defaults":{"params":{"vendor_billable_option":true}}
            }});
        }
        import(&resources, &db, vec![credential("tenant")?, input]).await?;
        let row = retained(
            &resources,
            &db,
            "tenant",
            "config-v1",
            document(
                vec![reference("tenant", ResourceKind::Route, "visible")?],
                None,
            )?,
        )
        .await?;
        let view = checked(resources.open_execution(&row))?;
        let selected = selected(&view, "visible", ApiProtocol::ChatCompletions)?;
        let mut prompt = ChatCompletionsAdapter.parse_request(json!({
            "model":"visible", "messages":[{"role":"user","content":"fixture turn"}], "stream":false
        }))?;
        assert_eq!(selected.block_token_pricing_input(&prompt), !extra_default);
        prompt
            .params
            .extra
            .insert("vendor_billable_option".into(), json!(true));
        assert!(!selected.block_token_pricing_input(&prompt));
    }
    Ok(())
}
