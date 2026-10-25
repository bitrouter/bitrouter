use super::*;

#[tokio::test]
async fn repeated_stage0_defaults_exhaust_native_work_without_rejecting_wire_v1() -> TestResult<()>
{
    for legacy in [false, true] {
        let mut input = basic("inherited")?;
        if let Definition::Route {
            routing,
            selector,
            exploration,
            ..
        } = &mut input.definition
        {
            *selector = "@shared".into();
            let params = json!({"private_default":"x".repeat(128 * 1024)});
            if legacy {
                routing["presets"] = json!({"shared":{
                    "model":"fixture:model", "params":params
                }});
            } else {
                routing["routers"] = json!({"shared":{
                    "selection":{"kind":"model","model":"fixture:model"},
                    "defaults":{"params":params}
                }});
            }
            routing["variants"] = json!({});
            for index in 0..80 {
                let variant = format!("v{index}");
                routing["variants"][&variant] = json!({});
                exploration.push(format!("@shared:{variant}"));
            }
        }
        // The 128 KiB default appears once in source, but each distinct variant
        // resolves that same definition and copies it before discarding it.
        let envelope = envelope(&input, None)?;
        ensure!(
            serde_json::to_vec(&envelope)?.len() < super::super::super::super::MAX_ARTIFACT_BYTES
        );
        let resources =
            super::super::super::super::Resources::with_key("installation".try_into()?, [29; 32])?;
        let row = super::persistence::sealed(&resources, &envelope)?;
        checked(resources.validate_compiled(&row))?;
        let view = checked(resources.open_execution(&row))?;
        ensure!(matches!(
            view.select_native("inherited", ModelOperation::Generation),
            Err(ErrorCode::CapacityExceeded)
        ));
        let wire = checked(view.select("inherited", &ApiProtocol::Messages))?;
        assert_eq!(targets(&wire, "inherited").await?.len(), 1);
        assert_eq!(
            wire.route_resolved(
                "@shared:v79",
                &RoutingPrefs::default(),
                &CallerContext::local()
            )
            .await?
            .len(),
            1
        );
        // The same authenticated definition is supported when its cumulative
        // use fits; this must not become a ban on large defaults or variants.
        if let Definition::Route { exploration, .. } = &mut input.definition {
            exploration.truncate(1);
        }
        let small = super::view(super::envelope(&input, None)?)?;
        checked(small.select_native("inherited", ModelOperation::Generation))?;
    }
    Ok(())
}

#[test]
fn root_stage0_is_preflighted_and_merged_root_preferences_pay_for_every_tier() -> TestResult<()> {
    let mut input = basic("root-prefs")?;
    if let Definition::Route {
        routing,
        selector,
        exploration,
        ..
    } = &mut input.definition
    {
        *selector = "@shared".into();
        routing["presets"] = json!({"shared":{
            "model":"fixture:model",
            "routing":{"ignore":["x".repeat(128 * 1024)]}
        }});
        *exploration = (0..80).map(|i| format!("fixture:other-{i}")).collect();
    }
    let Definition::Route {
        routing, selector, ..
    } = &input.definition
    else {
        anyhow::bail!("route definition");
    };
    let (config, _) = compiler::routing_config_bounded(routing, None)?;
    let mut budget = Budget::new();
    budget.work = ByteBudget::new(128 * 1024 - 1);
    // Even the root resolver cannot allocate its large routing preference with
    // a smaller budget; no provider resolution or target expansion is involved.
    ensure!(matches!(
        resolve_stage0(&config, selector, &mut budget),
        Err(Unavailable::Capacity)
    ));
    let envelope = envelope(&input, None)?;
    let decoded = retained::decode(envelope)?;
    let data = decoded.routes.get("root-prefs").context("route")?;
    ensure!(matches!(
        data.native.chains(),
        Err(ErrorCode::CapacityExceeded)
    ));
    // Tiers are bare provider selectors: the only repeated large copy is the
    // root preference merged into each tier, not a named tier's Stage 0 result.
    ensure!(data.chains.iter().any(|c| c.selector == "fixture:other-79"));
    Ok(())
}

#[tokio::test]
async fn native_budget_exhaustion_keeps_large_valid_v1_wire_view_usable() -> TestResult<()> {
    let mut source = basic("large")?;
    if let Definition::Route {
        routing,
        credentials,
        exploration,
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
        *exploration = vec!["fixture:other".into(), "fixture:third".into()];
    }
    let envelope = envelope(&source, None)?;
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
    if let Definition::Route { exploration, .. } = &mut input.definition {
        exploration.push("fixture:other".into());
    }
    let envelope = envelope(&input, None)?;
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
