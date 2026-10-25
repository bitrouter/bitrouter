use super::*;

#[tokio::test]
async fn closure_byte_budget_stops_before_loading_later_roots() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(&resources, &db, vec![credential("tenant")?]).await?;
    let mut roots = Vec::new();
    // Each artifact fits its individual bound. The unused provider remains
    // encrypted source, while the selected envelope stays small. Nine such
    // valid roots exceed the aggregate 8 MiB closure limit.
    for index in 0..9 {
        let name = format!("large-{index}");
        let mut input = route("tenant", &name, "model")?;
        input["definition"]["routing"]["providers"]["unused"] = json!({
            "api_base":"https://example.com/v1",
            "headers":{"x-large":"x".repeat(950 * 1024)}
        });
        import(&resources, &db, vec![input]).await?;
        roots.push(reference("tenant", ResourceKind::Route, &name)?);
    }
    // A late missing root distinguishes incremental rejection from a final
    // envelope check: an unbounded traversal returns NotFound instead.
    let missing = reference("tenant", ResourceKind::Route, "missing")?;
    roots.push(missing.clone());
    let tenant = "tenant".to_owned().try_into()?;
    let revision = "config-v1".to_owned().try_into()?;
    assert!(matches!(
        resources
            .compile(&db, &tenant, &revision, &document(roots, None)?)
            .await,
        Err(ErrorCode::CapacityExceeded)
    ));
    assert!(matches!(
        resources
            .compile(&db, &tenant, &revision, &document(vec![missing], None)?)
            .await,
        Err(ErrorCode::NotFound)
    ));
    Ok(())
}

#[test]
fn route_byte_budget_stops_before_resolving_later_selectors() -> Result<()> {
    let mut input = route("tenant", "expanded", "model")?;
    input["definition"]["routing"]["providers"]["fixture"]["headers"]["x-large"] =
        json!("x".repeat(16 * 1024));
    let mut exploration: Vec<_> = (0..255).map(|n| format!("fixture:model-{n}")).collect();
    // Resolution of this tail must never be reached after the 1 MiB budget.
    exploration.push("bitrouter/missing-router".into());
    input["definition"]["exploration"] = json!(exploration);
    let input: manifest::Import = serde_json::from_value(input)?;
    input.validate()?;
    let error = compiler::prepare_route(&input, None)
        .err()
        .context("oversized route accepted")?;
    assert_eq!(error.to_string(), "route envelope byte capacity exceeded");
    Ok(())
}

#[tokio::test]
async fn corrupt_catalog_identity_is_detected_without_projecting_oversized_cells() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(&resources, &db, vec![credential("tenant")?]).await?;
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_resource_revisions SET resource_id=hex(zeroblob(1048576))".to_owned(),
    ))
    .await?;
    assert!(storage::has_resources(&db).await?);
    assert!(storage::references(&db).await.is_err());
    assert!(resources.initialize_catalog(&db, None).await.is_err());
    Ok(())
}

#[test]
fn resolver_expansion_is_bounded_before_account_and_endpoint_cloning() -> Result<()> {
    let mut endpoints = route("tenant", "expanded", "model")?;
    endpoints["definition"]["selector"] = json!("many");
    endpoints["definition"]["routing"]["models"] = json!({"many": {
        "endpoints": vec![json!({"provider":"fixture", "service_id":"model"}); 257]
    }});
    let mut accounts = route("tenant", "expanded", "model")?;
    accounts["definition"]["routing"]["providers"]["fixture"]["headers"]["x-large"] =
        json!("x".repeat(256 * 1024));
    accounts["definition"]["routing"]["providers"]["fixture"]["accounts"] = json!(
        (0..64)
            .map(|n| json!({"label":format!("account-{n}")}))
            .collect::<Vec<_>>()
    );
    for input in [endpoints, accounts] {
        let input: manifest::Import = serde_json::from_value(input)?;
        input.validate()?;
        let error = compiler::prepare_route(&input, None)
            .err()
            .context("oversized expansion accepted")?;
        assert_eq!(
            error.to_string(),
            "route resolver expansion capacity exceeded"
        );
    }
    Ok(())
}
