use super::super::execution::{RetainedExecutionRevision, RetainedRoute};
use super::*;
use crate::{
    gateway::{
        caller::CallerContext,
        pipeline::{
            routing::{RoutingPrefs, RoutingTable},
            types::RoutingTarget,
        },
        providers::auth::credentials::Credential,
    },
    tenancy::configuration_store::StoredConfiguration,
};
use bitrouter_ai::types::{ApiProtocol, Capability, ModelOperation};
use std::sync::Arc;

mod policy;
mod targets;

fn checked<T>(result: std::result::Result<T, ErrorCode>) -> Result<T> {
    result.map_err(|code| anyhow::anyhow!("unexpected tenancy error: {code:?}"))
}

async fn retained(
    resources: &Resources,
    db: &DatabaseConnection,
    tenant: &str,
    revision: &str,
    document: Configuration,
) -> Result<StoredConfiguration> {
    let tenant = tenant.to_owned().try_into()?;
    let revision = revision.to_owned().try_into()?;
    let compiled = checked(resources.compile(db, &tenant, &revision, &document).await)?;
    let mut row = validation::stored(document, compiled)?;
    row.revision.tenant_id = tenant;
    row.revision.revision_id = revision;
    Ok(row)
}

fn selected(
    view: &RetainedExecutionRevision,
    name: &str,
    inbound: ApiProtocol,
) -> Result<Arc<RetainedRoute>> {
    checked(view.select(name, &inbound))
}

async fn first(route: &RetainedRoute, model: &str, inbound: ApiProtocol) -> Result<RoutingTarget> {
    let prefs = RoutingPrefs {
        operation: inbound.operation(),
        inbound_protocol: Some(inbound),
        ..Default::default()
    };
    route
        .route_chain(model, &prefs, &CallerContext::local())
        .await?
        .into_iter()
        .next()
        .context("missing retained candidate")
}

#[tokio::test]
async fn literal_selection_is_qualified_by_tenant_and_selected_route_only() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(
        &resources,
        &db,
        vec![
            credential("tenant")?,
            credential("other")?,
            route("tenant", "literal:fast", "a")?,
            route("tenant", "second", "b")?,
            route("other", "literal:fast", "c")?,
            policy("tenant", "unused")?,
        ],
    )
    .await?;
    let row = retained(
        &resources,
        &db,
        "tenant",
        "revision-a",
        document(
            vec![
                reference("tenant", ResourceKind::Route, "literal:fast")?,
                reference("tenant", ResourceKind::Route, "second")?,
            ],
            Some(reference("tenant", ResourceKind::Policy, "unused")?),
        )?,
    )
    .await?;
    let view = checked(resources.open_execution(&row))?;
    assert_eq!(view.installation_id(), &resources.installation);
    assert_eq!(view.tenant_id(), &row.revision.tenant_id);
    assert_eq!(view.revision_id(), &row.revision.revision_id);
    assert_eq!(view.document(), &row.revision.document);
    let a = selected(&view, "literal:fast", ApiProtocol::ChatCompletions)?;
    let b = selected(&view, "second", ApiProtocol::ChatCompletions)?;
    let ta = first(&a, "literal:fast", ApiProtocol::ChatCompletions).await?;
    let tb = first(&b, "second", ApiProtocol::ChatCompletions).await?;
    assert_eq!(ta.service_id, "a");
    assert_eq!(tb.service_id, "b");
    assert_eq!(
        checked(a.match_candidate(&ta))?,
        reference("tenant", ResourceKind::Credential, "slot")?
    );
    assert!(matches!(a.match_candidate(&tb), Err(ErrorCode::Forbidden)));
    assert_eq!(
        a.dependencies().iter().cloned().collect::<HashSet<_>>(),
        HashSet::from([
            a.reference().clone(),
            reference("tenant", ResourceKind::Credential, "slot")?,
        ])
    );
    assert!(a.policy().is_none());
    for alias in [
        "literal",
        " literal:fast",
        "literal:fast ",
        "@literal:fast",
        "literal/fast",
        "Literal:fast",
        "fixture:a",
    ] {
        assert!(matches!(
            view.select(alias, &ApiProtocol::ChatCompletions),
            Err(ErrorCode::NotFound)
        ));
    }
    let other = retained(
        &resources,
        &db,
        "other",
        "revision-a",
        document(
            vec![reference("other", ResourceKind::Route, "literal:fast")?],
            None,
        )?,
    )
    .await?;
    let other = checked(resources.open_execution(&other))?;
    let other = selected(&other, "literal:fast", ApiProtocol::ChatCompletions)?;
    let tc = first(&other, "literal:fast", ApiProtocol::ChatCompletions).await?;
    assert_eq!(tc.service_id, "c");
    assert_eq!(
        checked(other.match_candidate(&tc))?,
        reference("other", ResourceKind::Credential, "slot")?
    );
    assert!(matches!(a.match_candidate(&tc), Err(ErrorCode::Forbidden)));
    assert!(a.reload().await.is_err());
    Ok(())
}

#[tokio::test]
async fn pinned_revision_survives_new_revision_and_resource_source_loss() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(
        &resources,
        &db,
        vec![credential("tenant")?, route("tenant", "visible", "old")?],
    )
    .await?;
    let doc = document(
        vec![reference("tenant", ResourceKind::Route, "visible")?],
        None,
    )?;
    let row1 = retained(&resources, &db, "tenant", "config-1", doc).await?;
    let view1 = checked(resources.open_execution(&row1))?;
    let route1 = selected(&view1, "visible", ApiProtocol::ChatCompletions)?;
    let mut next = route("tenant", "visible", "new")?;
    next["reference"]["revision_id"] = json!("v2");
    import(&resources, &db, vec![next]).await?;
    let mut root2 = reference("tenant", ResourceKind::Route, "visible")?;
    root2.revision_id = "v2".to_owned().try_into()?;
    let row2 = retained(
        &resources,
        &db,
        "tenant",
        "config-2",
        document(vec![root2], None)?,
    )
    .await?;
    let view2 = checked(resources.open_execution(&row2))?;
    let route2 = selected(&view2, "visible", ApiProtocol::ChatCompletions)?;
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DELETE FROM tenancy_resource_revisions".to_owned(),
    ))
    .await?;
    let reopened = Resources::with_key(resources.installation.clone(), [7; 32])?;
    let restored1 = checked(reopened.open_execution(&row1))?;
    let restored1 = selected(&restored1, "visible", ApiProtocol::ChatCompletions)?;
    for route in [&*route1, &*restored1] {
        assert_eq!(
            first(route, "visible", ApiProtocol::ChatCompletions)
                .await?
                .service_id,
            "old"
        );
    }
    assert_eq!(
        first(&route2, "visible", ApiProtocol::ChatCompletions)
            .await?
            .service_id,
        "new"
    );
    assert!(matches!(
        reopened
            .credential(&db, &reference("tenant", ResourceKind::Credential, "slot")?)
            .await,
        Err(ErrorCode::NotFound)
    ));
    Ok(())
}

#[tokio::test]
async fn generation_defaults_and_unknown_capabilities_preserve_routing_semantics() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut input = route("tenant", "mixed", "model")?;
    input["definition"]["selector"] = json!("bitrouter/mixed");
    input["definition"]["routing"]["routers"] = json!({"mixed":{
        "selection":{"kind":"model","model":"fixture:model"},
        "defaults":{"system_prompt":"retained prompt","params":{"temperature":0.9,"top_p":0.8}}
    }});
    input["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
        json!([{"*": ["chat_completions", "decisions"]}]);
    import(&resources, &db, vec![credential("tenant")?, input]).await?;
    let row = retained(
        &resources,
        &db,
        "tenant",
        "mixed-revision",
        document(
            vec![reference("tenant", ResourceKind::Route, "mixed")?],
            None,
        )?,
    )
    .await?;
    let view = checked(resources.open_execution(&row))?;
    let generation = selected(&view, "mixed", ApiProtocol::ChatCompletions)?;
    let resolution = generation.resolve_model("mixed").await?;
    assert_eq!(
        resolution.overrides.system_prompt.as_deref(),
        Some("retained prompt")
    );
    assert!(!resolution.overrides.params.contains_key("temperature"));
    assert_eq!(resolution.overrides.params["top_p"], 0.8);
    assert!(
        !resolution
            .overrides
            .params
            .contains_key("max_output_tokens")
    );
    let mut input = crate::gateway::http::parse_input(
        &ApiProtocol::ChatCompletions,
        json!({"model":"mixed","messages":[{"role":"user","content":"hello"}]}),
    )?;
    generation.apply_generation_defaults(&mut input);
    let prompt = input.generation_prompt().context("generation")?;
    assert_eq!(prompt.params.max_tokens, Some(128));
    assert_eq!(prompt.params.temperature, Some(0.5));
    let prefs = RoutingPrefs {
        require_capabilities: vec![Capability::Tools, Capability::ImageInput],
        ..Default::default()
    };
    let chain = generation
        .route_resolved(&resolution.clean_model, &prefs, &CallerContext::local())
        .await?;
    assert_eq!(chain.len(), 1);
    assert!(chain[0].model_constraints.capabilities.is_empty());
    let decisions = selected(&view, "mixed", ApiProtocol::Decisions)?;
    let resolution = decisions.resolve_model("mixed").await?;
    assert!(resolution.overrides.params.is_empty());
    assert!(resolution.overrides.system_prompt.is_none());
    let overrides = decisions.preset_overrides("mixed").await?;
    assert!(overrides.params.is_empty() && overrides.system_prompt.is_none());
    let target = first(&decisions, "mixed", ApiProtocol::Decisions).await?;
    assert_eq!(target.api_protocol, ApiProtocol::Decisions);
    assert!(matches!(
        generation.match_candidate(&target),
        Err(ErrorCode::Forbidden)
    ));
    assert!(matches!(
        decisions.match_candidate(&chain[0]),
        Err(ErrorCode::Forbidden)
    ));
    assert!(
        decisions
            .route_chain("mixed", &RoutingPrefs::default(), &CallerContext::local())
            .await
            .is_err()
    );
    let wrong_inbound = RoutingPrefs {
        operation: ModelOperation::Decisions,
        inbound_protocol: Some(ApiProtocol::ChatCompletions),
        ..Default::default()
    };
    assert!(
        decisions
            .route_chain("mixed", &wrong_inbound, &CallerContext::local())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn retained_policy_accessors_and_tier_exploration_chains_are_immutable() -> Result<()> {
    let (db, resources) = fixture().await?;
    let mut input = route("tenant", "choice", "base")?;
    let policy_ref = reference("tenant", ResourceKind::Policy, "policy")?;
    input["definition"]["selector"] = json!("bitrouter/choice");
    input["definition"]["policy"] = serde_json::to_value(&policy_ref)?;
    input["definition"]["routing"]["routers"] = json!({"choice": {
        "selection":{"kind":"policy", "policy":"selected", "base_model":"fixture:base"}
    }});
    input["definition"]["exploration"] = json!(["fixture:explore"]);
    let mut policy = policy("tenant", "policy")?;
    let extra_ref = reference("tenant", ResourceKind::Credential, "policy-slot")?;
    policy["definition"]["dependencies"] = json!([extra_ref]);
    let mut extra = credential("tenant")?;
    extra["reference"] = serde_json::to_value(&extra_ref)?;
    import(
        &resources,
        &db,
        vec![credential("tenant")?, extra, policy, input],
    )
    .await?;
    let row = retained(
        &resources,
        &db,
        "tenant",
        "policy-revision",
        document(
            vec![reference("tenant", ResourceKind::Route, "choice")?],
            Some(policy_ref.clone()),
        )?,
    )
    .await?;
    let view = checked(resources.open_execution(&row))?;
    let route = selected(&view, "choice", ApiProtocol::ChatCompletions)?;
    assert_eq!(
        route.dependencies().iter().cloned().collect::<HashSet<_>>(),
        HashSet::from([
            route.reference().clone(),
            policy_ref.clone(),
            extra_ref,
            reference("tenant", ResourceKind::Credential, "slot")?,
        ])
    );
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DELETE FROM tenancy_resource_revisions".to_owned(),
    ))
    .await?;
    let retained_policy = route.policy().context("missing retained policy")?;
    assert_eq!(retained_policy.reference(), &policy_ref);
    assert_eq!(retained_policy.name(), "selected");
    assert_eq!(
        retained_policy.document().policies["selected"]
            .default_tier
            .as_deref(),
        Some("economy")
    );
    assert!(
        checked(retained_policy.configuration())?
            .providers
            .contains_key("fixture")
    );
    for (selector, expected) in [
        ("choice", "base"),
        ("fixture:cheap", "cheap"),
        ("fixture:strong", "strong"),
        ("fixture:explore", "explore"),
    ] {
        let resolution = route.resolve_model(selector).await?;
        let targets = route
            .route_resolved(
                &resolution.clean_model,
                &RoutingPrefs::default(),
                &CallerContext::local(),
            )
            .await?;
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].service_id, expected);
        assert!(route.match_candidate(&targets[0]).is_ok());
    }
    assert!(route.resolve_model("fixture:unretained").await.is_err());
    Ok(())
}

#[tokio::test]
async fn credentials_are_exact_owned_static_slots_with_no_fallback() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(
        &resources,
        &db,
        vec![credential("tenant")?, credential("other")?],
    )
    .await?;
    let original = reference("tenant", ResourceKind::Credential, "slot")?;
    let value = checked(resources.credential(&db, &original).await)?;
    assert_eq!(
        value,
        Credential::ApiKey {
            value: "PRIVATE_STATIC_TOKEN".into()
        }
    );
    let mut missing = original.clone();
    missing.revision_id = "missing".to_owned().try_into()?;
    assert!(matches!(
        resources.credential(&db, &missing).await,
        Err(ErrorCode::NotFound)
    ));
    assert!(matches!(
        resources
            .credential(&db, &reference("tenant", ResourceKind::Route, "slot")?)
            .await,
        Err(ErrorCode::InvalidRequest)
    ));
    let wrong_key = Resources::with_key(resources.installation.clone(), [8; 32])?;
    assert!(matches!(
        wrong_key.credential(&db, &original).await,
        Err(ErrorCode::AuthorityUnavailable)
    ));
    // A retained OAuth record must still fail closed even if written with a
    // valid wrapping key by an older/unsupported importer.
    let mut oauth = credential("tenant")?;
    oauth["reference"]["resource_id"] = json!("oauth-slot");
    oauth["definition"]["credential"] = json!({"type":"oauth","data":{"access_token":"secret","refresh_token":"refresh","expires_at":0}});
    let oauth: manifest::Import = serde_json::from_value(oauth)?;
    storage::retain(
        &resources,
        &db,
        &manifest::Artifact {
            reference: oauth.reference.clone(),
            definition: oauth.definition,
            envelope: None,
        },
    )
    .await?;
    assert!(matches!(
        resources.credential(&db, &oauth.reference).await,
        Err(ErrorCode::AuthorityUnavailable)
    ));
    Ok(())
}
