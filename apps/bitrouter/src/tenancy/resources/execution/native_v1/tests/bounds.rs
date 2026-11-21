use super::*;

#[test]
fn current_named_router_defaults_are_charged_before_resolution_copies() -> TestResult<()> {
    let source = json!({"providers":{"fixture":{"api_base":"https://example.com/v1","models":[{"id":"model"}]}},
        "routers":{"shared":{"selection":{"kind":"model","model":"fixture:model"},
            "defaults":{"params":{"private_default":"x".repeat(128*1024)}}}}});
    let (config, _) = compiler::routing_config_bounded(&source, None)?;
    let mut budget = Budget::new();
    budget.work = ByteBudget::new(128 * 1024 - 1);
    ensure!(matches!(
        resolve_stage0(&config, "bitrouter/shared", &mut budget),
        Err(Unavailable::Capacity)
    ));
    let mut budget = Budget::new();
    resolve_stage0(&config, "bitrouter/shared", &mut budget)
        .map_err(|_| anyhow::anyhow!("current router rejected"))?;
    ensure!(budget.work.remaining() < MAX_WORK_BYTES - 128 * 1024);
    for old in ["presets", "variants"] {
        let mut invalid = source.clone();
        invalid[old] = json!({});
        ensure!(compiler::routing_config_bounded(&invalid, None).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn native_budget_exhaustion_keeps_large_valid_v1_wire_view_usable() -> TestResult<()> {
    let mut source = basic("large")?;
    if let Definition::Route {
        routing,
        credentials,
        ..
    } = &mut source.definition
    {
        // Every target expansion conservatively charges this explicit model
        // catalog. Each v1 resolution fits its old cap; their cumulative native
        // work exceeds the new independent quota without growing the wire data.
        routing["providers"]["fixture"]["models"] = Value::Array(
            (0..128)
                .map(|index| json!({"id":format!("model-{index}-{}", "x".repeat(400))}))
                .collect(),
        );
        routing["providers"]["fixture"]["accounts"] = Value::Array(
            (0..64)
                .map(|index| json!({"label":format!("account-{index}")}))
                .collect(),
        );
        credentials.clear();
        for index in 0..64 {
            credentials.push(CredentialBinding {
                provider: "fixture".into(),
                account: format!("account-{index}"),
                credential: reference(ResourceKind::Credential, &format!("slot-{index}"))?,
            });
        }
    }
    let policy = bind_tiers(
        &mut source,
        vec!["fixture:other".into(), "fixture:third".into()],
    )?;
    let envelope = envelope(&source, Some(policy))?;
    let bytes = serde_json::to_vec(&envelope)?;
    ensure!(bytes.len() < super::super::super::super::MAX_ARTIFACT_BYTES);
    let wire_occurrences: usize = envelope
        .routes
        .iter()
        .flat_map(|r| &r.chains)
        .map(|c| c.targets.len())
        .sum();
    assert_eq!(wire_occurrences, 576);
    let resources =
        super::super::super::super::Resources::with_key("installation".try_into()?, [23; 32])?;
    let row = super::persistence::sealed(&resources, &envelope)?;
    checked(resources.validate_compiled(&row))?;
    let view = checked(resources.open_execution(&row))?;

    ensure!(matches!(
        view.select_native("large", ModelOperation::Generation),
        Err(ErrorCode::CapacityExceeded)
    ));
    let wire = checked(view.select("large", &ApiProtocol::Messages))?;
    ensure!(targets(&wire, "large").await?.len() == 64);
    ensure!(
        wire.route_resolved(
            "fixture:third",
            &RoutingPrefs::default(),
            &CallerContext::local()
        )
        .await?
        .len()
            == 64
    );
    // GET's production typed validation still accepts this exact v1 envelope.
    retained::decode(serde_json::from_slice(&bytes)?)?;
    Ok(())
}

#[test]
fn native_count_limits_and_failed_partial_work_never_publish_a_partial_route() -> TestResult<()> {
    let mut input = basic("bounded")?;
    let policy = bind_tiers(&mut input, vec!["fixture:other".into()])?;
    let envelope = envelope(&input, Some(policy))?;
    let decoded = retained::decode(envelope)?;
    let data = decoded.routes.get("bounded").context("route")?;
    let Definition::Route {
        routing,
        selector,
        credentials,
        ..
    } = &input.definition
    else {
        anyhow::bail!("route definition");
    };
    let mut budget = Budget::new();
    budget.chains = 1;
    let result = derive(
        selector,
        routing,
        credentials,
        &data.chains,
        &data.candidates,
        &mut budget,
    );
    ensure!(matches!(
        result,
        NativeChains::Unavailable(Unavailable::Capacity)
    ));
    // Chain preflight fails before spending source/target work.
    assert_eq!(
        budget.work.remaining(),
        MAX_WORK_BYTES - selector.len() - std::mem::size_of::<NativeChain>()
    );
    ensure!(budget.exhausted);
    let remaining = budget.work.remaining();
    let result = derive(
        selector,
        routing,
        credentials,
        &data.chains,
        &data.candidates,
        &mut budget,
    );
    ensure!(matches!(
        result,
        NativeChains::Unavailable(Unavailable::Capacity)
    ));
    assert_eq!(budget.work.remaining(), remaining);
    let mut budget = Budget::new();
    budget.indices = 1;
    let result = derive(
        selector,
        routing,
        credentials,
        &data.chains,
        &data.candidates,
        &mut budget,
    );
    ensure!(matches!(
        result,
        NativeChains::Unavailable(Unavailable::Capacity)
    ));
    assert_eq!(budget.indices, 0); // base was completed; it did not escape.
    ensure!(budget.bytes(usize::MAX).is_err());
    Ok(())
}

#[test]
fn resolver_preflight_rejects_native_account_expansion_before_target_allocation() -> TestResult<()>
{
    let input = basic("bounded")?;
    let decoded = retained::decode(envelope(&input, None)?)?;
    let data = decoded.routes.get("bounded").context("route")?;
    let Definition::Route {
        mut routing,
        selector,
        credentials,
        ..
    } = input.definition
    else {
        anyhow::bail!("route definition");
    };
    // This source is used only to exercise the newly introduced native error
    // boundary; the wire envelope above remains valid and independently usable.
    routing["providers"]["fixture"]["accounts"] = Value::Array(
        (0..256)
            .map(|index| json!({"label":format!("account-{index}")}))
            .collect(),
    );
    let mut budget = Budget::new();
    budget.indices = 1;
    let result = derive(
        &selector,
        &routing,
        &credentials,
        &data.chains,
        &data.candidates,
        &mut budget,
    );
    ensure!(matches!(
        result,
        NativeChains::Unavailable(Unavailable::Capacity)
    ));
    assert_eq!(budget.indices, 1); // no resolved target or index was accepted.
    Ok(())
}
