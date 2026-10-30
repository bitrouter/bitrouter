use super::*;
use anyhow::ensure;

fn rows() -> Result<Vec<Value>> {
    let mut parent = route("tenant", "parent", "generation")?;
    parent["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
        json!([{"*":"chat_completions"}]);
    parent["definition"]["core_choice"] = json!({"profile":"choice_v1","route":reference("tenant",ResourceKind::Route,"child")?,"policy":{}});
    let mut child = route("tenant", "child", "judge")?;
    child["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
        json!([{"*":"decisions"}]);
    child["definition"]["routing"]["providers"]["fixture"]["headers"]["x-large"] =
        json!("x".repeat(16 * 1024));
    Ok(vec![parent, credential("tenant")?, child])
}

#[tokio::test]
async fn choice_import_rejects_unsupported_child_graph_atomically() -> Result<()> {
    for mutation in [
        "missing",
        "self",
        "nested",
        "assessment",
        "generation",
        "exploration",
        "credential",
        "cross-tenant",
        "policy-bound",
    ] {
        let (db, resources) = fixture().await?;
        let mut input = rows()?;
        match mutation {
            "missing" => {
                input.pop();
            }
            "self" => {
                input[0]["definition"]["core_choice"]["route"] = input[0]["reference"].clone()
            }
            "nested" => {
                input[2]["definition"]["core_choice"] =
                    input[0]["definition"]["core_choice"].clone()
            }
            "assessment" => {
                input[2]["definition"]["assessment"] = json!({
                    "profile": "assessment_v1", "route": input[0]["reference"].clone()
                });
            }
            "generation" => {
                input[2]["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
                    json!([{"*":"chat_completions"}])
            }
            "exploration" => input[2]["definition"]["exploration"] = json!(["fixture:other"]),
            "credential" => input[1]["definition"]["oauth_generation"] = json!(1),
            "cross-tenant" => {
                input[0]["definition"]["core_choice"]["route"]["owner_id"] = json!("other")
            }
            "policy-bound" => {
                input[0]["definition"]["core_choice"]["policy"]["max_candidates"] = json!(33)
            }
            _ => anyhow::bail!("unknown graph mutation"),
        }
        ensure!(
            import(&resources, &db, input).await.is_err(),
            "invalid {mutation} accepted"
        );
        ensure!(
            !storage::has_resources(&db).await?,
            "failed import published a partial closure"
        );
        import(&resources, &db, rows()?).await?;
        ensure!(storage::references(&db).await?.len() == 3);
    }
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
#[tokio::test]
async fn hidden_choice_survives_source_loss_and_keeps_exact_cache_root_pin() -> Result<()> {
    use crate::gateway::{
        caller::CallerContext,
        pipeline::routing::{RoutingPrefs, RoutingTable},
    };
    use crate::tenancy::cache::{Cache, CacheError};
    use bitrouter_ai::types::{ApiProtocol, ModelOperation};
    let (db, resources) = fixture().await?;
    import(&resources, &db, rows()?).await?;
    let parent_ref = reference("tenant", ResourceKind::Route, "parent")?;
    let child_ref = reference("tenant", ResourceKind::Route, "child")?;
    let doc = document(vec![parent_ref.clone()], None)?;
    let compiled = compile(&resources, &db, &doc).await?;
    let row = validation::stored(doc, compiled)?;
    let checked = |code| anyhow::anyhow!("retained Choice: {code:?}");
    let view = resources.open_execution(&row).map_err(checked)?;
    ensure!(matches!(
        view.select("child", &ApiProtocol::Decisions),
        Err(ErrorCode::NotFound)
    ));
    let weight = view.retained_heap_bytes().map_err(checked)?;
    ensure!(
        weight > 16 * 1024,
        "hidden header/source allocations were not charged"
    );
    let cache = Cache::new(1, 32 * 1024 * 1024)?;
    let root = cache.insert(1, view, weight, 0)?;
    let parent = root
        .select_native("parent", ModelOperation::Generation)
        .map_err(checked)?;
    let child = parent.choice_route().map_err(checked)?;
    ensure!(child.reference() == &child_ref && parent.reference() == &parent_ref);
    ensure!(parent.dependencies().contains(&child_ref));
    let prefs = RoutingPrefs {
        operation: ModelOperation::Decisions,
        inbound_protocol: None,
        ..Default::default()
    };
    let targets = child
        .route_chain("child", &prefs, &CallerContext::local())
        .await?;
    ensure!(targets.len() == 1 && targets[0].service_id == "judge");
    ensure!(
        child.match_candidate(&targets[0]).map_err(checked)?
            == reference("tenant", ResourceKind::Credential, "slot")?
    );
    ensure!(
        parent.match_candidate(&targets[0]).is_err(),
        "child proof was unioned into its parent"
    );
    let mut forged = targets[0].clone();
    forged.service_id = "forged".into();
    ensure!(child.match_candidate(&forged).is_err());
    drop(parent);
    drop(root);
    db.execute_unprepared("DELETE FROM tenancy_resource_revisions")
        .await?;
    let next = resources.open_execution(&row).map_err(checked)?;
    ensure!(
        matches!(cache.insert(2, next, weight, 0), Err(CacheError::Capacity)),
        "hidden child escaped cache accounting"
    );
    ensure!(
        child
            .route_chain("child", &prefs, &CallerContext::local())
            .await?[0]
            .service_id
            == "judge"
    );
    drop(child);
    drop(cache.insert(
        2,
        resources.open_execution(&row).map_err(checked)?,
        weight,
        0,
    )?);
    Ok(())
}

#[tokio::test]
async fn choice_child_cannot_be_exposed_and_minimal_profile_cannot_execute_it() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(&resources, &db, rows()?).await?;
    let parent = reference("tenant", ResourceKind::Route, "parent")?;
    let child = reference("tenant", ResourceKind::Route, "child")?;
    ensure!(
        compile(
            &resources,
            &db,
            &document(vec![parent.clone(), child], None)?
        )
        .await
        .is_err()
    );
    let qualified = compile(&resources, &db, &document(vec![parent], None)?).await;
    if cfg!(all(feature = "core", feature = "harness")) {
        ensure!(qualified.is_ok());
    } else {
        ensure!(
            qualified.is_err(),
            "unsupported profile qualified execution"
        );
    }
    Ok(())
}
