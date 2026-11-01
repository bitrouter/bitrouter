use super::*;

async fn fixture_view(count: usize) -> Result<(Resources, StoredConfiguration)> {
    let (db, resources) = fixture().await?;
    let mut input = route("tenant", "ordered", "model")?;
    input["definition"]["selector"] = json!("model");
    input["definition"]["routing"]["providers"] = json!({});
    let mut slots = Vec::new();
    let mut credentials = Vec::new();
    for index in 0..count {
        let name = format!("provider-{index:03}");
        let slot = reference("tenant", ResourceKind::Credential, &format!("slot-{index}"))?;
        let mut secret = credential("tenant")?;
        secret["reference"] = serde_json::to_value(&slot)?;
        slots.push(secret);
        credentials.push(json!({"provider":name,"account":"default","credential":slot}));
        input["definition"]["routing"]["providers"][&name] = json!({
            "api_base":"https://example.com/v1",
            "api_protocol":[{"*":["chat_completions","messages","responses"]}],
            "models":[{"id":"model"}]
        });
    }
    input["definition"]["credentials"] = json!(credentials);
    slots.push(input);
    import(&resources, &db, slots).await?;
    let row = retained(
        &resources,
        &db,
        "tenant",
        "ordered-config",
        document(
            vec![reference("tenant", ResourceKind::Route, "ordered")?],
            None,
        )?,
    )
    .await?;
    Ok((resources, row))
}

async fn targets(route: &RetainedRoute, inbound: ApiProtocol) -> Result<Vec<RoutingTarget>> {
    Ok(route
        .route_chain(
            "ordered",
            &RoutingPrefs {
                operation: inbound.operation(),
                inbound_protocol: Some(inbound),
                ..Default::default()
            },
            &CallerContext::local(),
        )
        .await?)
}

#[tokio::test]
async fn subsets_preserve_root_ordinals_and_do_not_mutate_other_views() -> Result<()> {
    let (resources, row) = fixture_view(4).await?;
    let view = checked(resources.open_execution(&row))?;
    let reference = reference("tenant", ResourceKind::Route, "ordered")?;
    for inbound in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::Responses,
    ] {
        let original = selected(&view, "ordered", inbound.clone())?;
        let all = targets(&original, inbound.clone()).await?;
        assert_eq!(all.len(), 4);
        let route = checked(view.select_ordered_for_test(&reference, &inbound, &[3, 1]))?;
        let actual = targets(&route, inbound.clone()).await?;
        assert_eq!(actual.len(), 2);
        for (target, ordinal) in actual.iter().zip([3_u16, 1]) {
            assert_eq!(
                target.provider_name,
                all[usize::from(ordinal)].provider_name
            );
            assert_eq!(checked(route.candidate_ordinal(target))?, Some(ordinal));
        }
        assert!(route.match_candidate(&all[0]).is_err());
        assert!(route.candidate_ordinal(&all[0]).is_err());
        assert_eq!(route.dependencies().len(), 3);
        assert!(
            !route
                .dependencies()
                .contains(&checked(original.match_candidate(&all[0]))?)
        );
        assert_eq!(targets(&original, inbound.clone()).await?.len(), 4);
        let sibling = checked(view.select_ordered_for_test(&reference, &inbound, &[0]))?;
        assert!(sibling.match_candidate(&actual[0]).is_err());
        let reopened = checked(resources.open_execution(&row))?;
        let restarted = checked(reopened.select_ordered_for_test(&reference, &inbound, &[3, 1]))?;
        let after = targets(&restarted, inbound).await?;
        assert_eq!(
            after.iter().map(|t| &t.provider_name).collect::<Vec<_>>(),
            actual.iter().map(|t| &t.provider_name).collect::<Vec<_>>()
        );
    }
    Ok(())
}

#[tokio::test]
async fn candidate_bounds_and_exact_resource_revision_are_enforced() -> Result<()> {
    let (resources, row) = fixture_view(65).await?;
    let view = checked(resources.open_execution(&row))?;
    let mut reference = reference("tenant", ResourceKind::Route, "ordered")?;
    for count in [1_u16, 8, 9, 64] {
        let ordinals: Vec<_> = (0..count).rev().collect();
        let route = checked(view.select_ordered_for_test(
            &reference,
            &ApiProtocol::ChatCompletions,
            &ordinals,
        ))?;
        assert_eq!(
            targets(&route, ApiProtocol::ChatCompletions).await?.len(),
            usize::from(count)
        );
    }
    for invalid in [Vec::new(), vec![0, 0], vec![65], (0..65).collect()] {
        assert!(
            view.select_ordered_for_test(&reference, &ApiProtocol::ChatCompletions, &invalid)
                .is_err()
        );
    }
    reference.revision_id = "other".try_into()?;
    assert!(
        view.select_ordered_for_test(&reference, &ApiProtocol::ChatCompletions, &[0])
            .is_err()
    );
    Ok(())
}
