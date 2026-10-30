use super::*;

#[test]
fn owned_compilation_disables_registry_and_rejects_host_catalog_settings() -> Result<()> {
    let mut source = json!({"providers": {"fixture": {
        "api_base": "https://example.com/v1", "models": [{"id": "model"}]
    }}});
    let (config, _) = compiler::routing_config(&source)?;
    assert!(!config.registry.enabled);
    assert!(
        config
            .providers
            .values()
            .all(|provider| provider.registry.is_none())
    );
    let targets = crate::config::routing_table::resolve_clean_route_chain(
        &config,
        "fixture:model",
        &crate::gateway::pipeline::routing::RoutingPrefs::default(),
    )?;
    assert!(!targets.is_empty());
    assert!(targets.iter().all(|target| target.registry.is_none()));
    source["registry"] = json!({"enabled": true});
    assert!(compiler::routing_config(&source).is_err());
    Ok(())
}

#[tokio::test]
async fn fixed_router_base_does_not_reinterpret_its_model_as_a_variant() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut input = route("tenant", "fixed", "fast")?;
    input["definition"]["selector"] = json!("bitrouter/fixed");
    input["definition"]["routing"]["routers"] = json!({"fixed": {
        "selection":{"kind":"model", "model":"fixture:fast"}
    }});
    input["definition"]["routing"]["variants"] = json!({"fast":{}});
    input["definition"]["routing"]["providers"]["fixture"]["models"] =
        json!([{"id":"fast"}, {"id":"fixture"}]);
    import(&resources, &db, vec![credential("tenant")?, input]).await?;
    let doc = document(
        vec![reference("tenant", ResourceKind::Route, "fixed")?],
        None,
    )?;
    let compiled = compile(&resources, &db, &doc).await?;
    let envelope = decrypt(&resources, &doc, &compiled)?;
    assert!(!envelope.routes[0].chains.is_empty());
    for chain in &envelope.routes[0].chains {
        assert_eq!(chain.selector, "bitrouter/fixed");
        assert!(!chain.targets.is_empty());
        for target in &chain.targets {
            assert_eq!(target.target["provider"], "fixture");
            assert_eq!(target.target["model"], "fast");
        }
    }
    Ok(())
}

#[tokio::test]
async fn tier_and_exploration_selectors_resolve_once_and_keep_their_preferences() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut input = route("tenant", "choice", "base")?;
    let policy_ref = reference("tenant", ResourceKind::Policy, "policy")?;
    input["definition"]["selector"] = json!("bitrouter/choice");
    input["definition"]["policy"] = serde_json::to_value(&policy_ref)?;
    input["definition"]["routing"]["routers"] = json!({"choice": {
        "selection":{"kind":"policy", "policy":"selected", "base_model":"fixture:base"}
    }});
    input["definition"]["routing"]["presets"] = json!({"experiment":{"model":"shared"}});
    input["definition"]["routing"]["variants"] = json!({"fast":{"routing":{"ignore":["other"]}}});
    input["definition"]["routing"]["providers"]["fixture"]["models"] =
        json!([{"id":"base"}, {"id":"fast"}, {"id":"fixture"}, {"id":"shared"}]);
    input["definition"]["routing"]["providers"]["other"] = json!({
        "api_base":"https://example.com/v1", "models":[{"id":"shared"}]
    });
    input["definition"]["exploration"] = json!(["@experiment:fast"]);
    let mut policy = policy("tenant", "policy")?;
    // One suffix is a variant; the remaining ':fast' is a literal provider
    // model. A second Stage-0 pass would incorrectly retain model fixture.
    policy["definition"]["document"]["policies"]["selected"]["tiers"]["economy"]["model"] =
        json!("fixture:fast:fast");
    import(&resources, &db, vec![credential("tenant")?, policy, input]).await?;
    let doc = document(
        vec![reference("tenant", ResourceKind::Route, "choice")?],
        Some(policy_ref),
    )?;
    let compiled = compile(&resources, &db, &doc).await?;
    let envelope = decrypt(&resources, &doc, &compiled)?;
    let chains = &envelope.routes[0].chains;
    for (selector, model) in [
        ("bitrouter/choice", "base"),
        ("fixture:fast:fast", "fast"),
        ("fixture:strong", "strong"),
        ("@experiment:fast", "shared"),
    ] {
        let selected: Vec<_> = chains
            .iter()
            .filter(|chain| chain.selector == selector)
            .collect();
        assert!(!selected.is_empty());
        for chain in selected {
            assert_eq!(chain.targets.len(), 1);
            assert_eq!(chain.targets[0].target["provider"], "fixture");
            assert_eq!(chain.targets[0].target["model"], model);
        }
    }
    Ok(())
}

fn protocol_endpoint(protocol: &str, endpoint: &str) -> Result<Value> {
    let mut input = route("tenant", "endpoint", "model")?;
    let provider = &mut input["definition"]["routing"]["providers"]["fixture"];
    provider["api_protocol"] = json!([{ "*": protocol }]);
    provider["protocol_endpoints"] = json!({});
    provider["protocol_endpoints"][protocol] = json!(endpoint);
    Ok(input)
}

#[tokio::test]
async fn protocol_endpoint_rejection_rolls_back_without_retaining_the_url() -> Result<()> {
    let (db, resources) = fixture().await?;
    for protocol in ["chat_completions", "messages", "responses", "decisions"] {
        for endpoint in [
            "https://metadata.google.internal/v1",
            "https://instance-data.ec2.internal/v1",
            "http://169.254.169.254/latest/meta-data",
            "https://10.0.0.1/v1",
            "https://192.168.1.1/v1",
            "https://[fd00::1]/v1",
            "file:///etc/passwd",
            "gopher://example.com/v1",
            "http://example.com/v1",
        ] {
            let input = protocol_endpoint(protocol, endpoint)?;
            let parsed: manifest::Import = serde_json::from_value(input.clone())?;
            let error = compiler::prepare_route(&parsed, None)
                .err()
                .context("forbidden protocol endpoint accepted")?;
            assert_eq!(error.to_string(), "invalid retained target URL");
            assert!(
                import(&resources, &db, vec![credential("tenant")?, input])
                    .await
                    .is_err()
            );
            assert!(!storage::has_resources(&db).await?);
        }
    }
    Ok(())
}

#[test]
fn protocol_endpoints_preserve_permitted_loopback_and_public_https() -> Result<()> {
    for protocol in ["chat_completions", "messages", "responses", "decisions"] {
        for endpoint in [
            "https://example.com/native",
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:8080/v1",
        ] {
            let input: manifest::Import =
                serde_json::from_value(protocol_endpoint(protocol, endpoint)?)?;
            let envelope = compiler::prepare_route(&input, None)?;
            assert!(!envelope.chains.is_empty());
            for chain in envelope.chains {
                assert!(!chain.targets.is_empty());
                for target in chain.targets {
                    assert_eq!(target.target["api_base"], endpoint);
                    assert_eq!(target.target["protocol"], protocol);
                }
            }
        }
    }
    Ok(())
}
