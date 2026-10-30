use std::collections::HashMap;

use anyhow::{Context, Result as TestResult, ensure};
use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKind};
use serde_json::json;

use super::*;
use crate::{
    gateway::{
        caller::CallerContext,
        pipeline::{
            routing::{RoutingPrefs, RoutingTable},
            types::OutboundHeaderRule,
        },
    },
    tenancy::cache::Cache,
};

mod registry;

fn weight(value: &RetainedExecutionRevision) -> TestResult<usize> {
    retained_heap_bytes(value).map_err(|code| anyhow::anyhow!("weight failed: {code:?}"))
}

#[test]
fn optional_assessment_reference_is_charged_without_selecting_the_route() -> TestResult<()> {
    let mut value = revision()?;
    insert(&mut value, "subject", route("subject", "private")?);
    let before = weight(&value)?;
    let assessment = reference(
        ResourceKind::Route,
        "judge-with-distinct-immutable-revision",
    )?;
    let expected = assessment.owner_id.as_str().len()
        + assessment.resource_id.as_str().len()
        + assessment.revision_id.as_str().len();
    Arc::get_mut(value.routes.get_mut("subject").context("subject")?)
        .context("unique route")?
        .assessment = Some(assessment);
    ensure!(weight(&value)? >= before + expected);
    Ok(())
}

fn reference(kind: ResourceKind, name: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "tenant".try_into()?,
        resource_kind: kind,
        resource_id: name.try_into()?,
        revision_id: "r1".try_into()?,
    })
}

fn revision() -> TestResult<RetainedExecutionRevision> {
    Ok(RetainedExecutionRevision {
        installation: "installation".try_into()?,
        tenant: "tenant".try_into()?,
        revision: "r1".try_into()?,
        authority_revision: 1.try_into()?,
        document: serde_json::from_value(json!({
            "routes": [], "policy": null,
            "generation_defaults": {"max_output_tokens": 32, "temperature": 0.0},
            "learning": {"mode": "off", "max_evaluations": 0},
            "limits": {"active_executions": 1, "queued_work": 1, "learning_jobs": 0}
        }))?,
        routes: HashMap::new(),
    })
}

fn route(name: &str, header: &str) -> TestResult<RouteData> {
    let (target, projection) = super::super::target::decode(json!({
        "provider": "fixture", "model": "model", "api_base": "https://example.invalid/v1",
        "protocol": "chat_completions", "account": null, "auth_scheme": "bearer",
        "chat_token_limit_field": null, "chat_supports_store": null,
        "chat_supports_stream_options": null, "chat_google_extensions": false,
        "reasoning_effort": null, "model_constraints": NativeRouteConstraints::default(),
        "headers": [{"name": "x-private", "default": header, "passthrough": false}]
    }))?;
    let reference = reference(ResourceKind::Route, name)?;
    Ok(RouteData {
        assessment: None,
        block: None,
        block_pricing: Vec::new(),
        dependencies: vec![reference.clone()],
        reference,
        selector: "fixture:model".into(),
        overrides: PromptOverrides::default(),
        policy: None,
        choice: None,
        chains: vec![Chain {
            selector: "fixture:model".into(),
            inbound: ApiProtocol::ChatCompletions,
            candidates: vec![0],
        }],
        native: NativeChains::Ready(vec![NativeChain {
            selector: "fixture:model".into(),
            operation: bitrouter_ai::types::ModelOperation::Generation,
            candidates: vec![0],
        }]),
        candidates: vec![Candidate {
            target,
            projection,
            credential: self::reference(ResourceKind::Credential, "slot")?,
        }],
    })
}

fn insert(value: &mut RetainedExecutionRevision, name: &str, route: RouteData) {
    value.document.routes.push(route.reference.clone());
    value.routes.insert(name.into(), Arc::new(route));
}

#[test]
fn selection_does_not_hide_unselected_routes_or_their_long_fixed_headers() -> TestResult<()> {
    let mut value = revision()?;
    insert(&mut value, "chosen", route("chosen", "short")?);
    insert(&mut value, "other", route("other", "short")?);
    let selected = value
        .select("chosen", &ApiProtocol::ChatCompletions)
        .map_err(|code| anyhow::anyhow!("selection failed: {code:?}"))?;
    let before = weight(&value)?;
    let private = "p".repeat(96 * 1024);
    value
        .routes
        .insert("other".into(), Arc::new(route("other", &private)?));
    let after = weight(&value)?;
    ensure!(selected.reference().resource_id.as_str() == "chosen");
    // Both the decoded header and the exact candidate projection retain data.
    // Counting only the chosen route, or only its projection, misses this delta.
    ensure!(after > before + private.len());
    Ok(())
}

#[tokio::test]
async fn exported_target_detaches_header_buffers_and_survives_root_eviction() -> TestResult<()> {
    assert_exported_target_detaches(false).await
}

#[tokio::test]
async fn native_target_detaches_headers_and_keeps_the_entire_revision_charged() -> TestResult<()> {
    assert_exported_target_detaches(true).await
}

async fn assert_exported_target_detaches(native: bool) -> TestResult<()> {
    let long_name = format!("x-{}", "retained-header-".repeat(96));
    let long_value = "private-retained-value-".repeat(4096);
    let mut data = route("chosen", &long_value)?;
    let candidate = data.candidates.first_mut().context("candidate")?;
    candidate.target.headers = vec![OutboundHeaderRule::new(
        &long_name,
        Some(&long_value),
        false,
    )?];
    candidate.projection = super::super::target::project(&candidate.target)?;
    let mut first = revision()?;
    insert(&mut first, "chosen", data);
    let first = RetainedExecutionRevision::new(
        first.installation,
        first.tenant,
        first.revision,
        first.authority_revision,
        super::super::ValidatedEnvelope {
            learned_checkpoint: false,
            document: first.document,
            routes: first.routes,
        },
    )
    .map_err(|code| anyhow::anyhow!("constructor failed: {code:?}"))?;
    let first_weight = weight(&first)?;
    let cache = Cache::<u8, RetainedExecutionRevision>::new(1, 128 * 1024 * 1024)?;
    let root = cache.insert(1, first, first_weight, 0)?;
    // Select through the actual cache Pin, not the uncached decoder-test method.
    let selected = if native {
        root.select_native("chosen", bitrouter_ai::types::ModelOperation::Generation)
    } else {
        root.select("chosen", &ApiProtocol::ChatCompletions)
    }
    .map_err(|code| anyhow::anyhow!("selection failed: {code:?}"))?;
    let target = selected
        .route_chain(
            "chosen",
            &RoutingPrefs::default(),
            &CallerContext::new("credential", "principal"),
        )
        .await?
        .into_iter()
        .next()
        .context("exported target")?;
    let (sensitive, passthrough) = {
        let original = root
            .routes
            .get("chosen")
            .context("cached route")?
            .candidates
            .first()
            .context("cached candidate")?
            .target
            .headers
            .first()
            .context("cached header")?;
        let exported = target.headers.first().context("exported header")?;
        let original_value = original.default().context("cached header value")?;
        let exported_value = exported.default().context("exported header value")?;
        ensure!(original.name().as_str() == long_name);
        ensure!(original_value.as_bytes() == long_value.as_bytes());
        ensure!(exported.name() == original.name());
        ensure!(exported_value.as_bytes() == original_value.as_bytes());
        // Both owners are live: allocator reuse cannot make these assertions
        // pass accidentally after the cached backing allocation has been freed.
        ensure!(
            !std::ptr::eq(
                original.name().as_str().as_ptr(),
                exported.name().as_str().as_ptr()
            ),
            "exported name still shares the cached backing allocation"
        );
        ensure!(
            !std::ptr::eq(
                original_value.as_bytes().as_ptr(),
                exported_value.as_bytes().as_ptr()
            ),
            "exported value still shares the cached backing allocation"
        );
        ensure!(exported_value.is_sensitive() == original_value.is_sensitive());
        ensure!(exported.passthrough() == original.passthrough());
        (original_value.is_sensitive(), original.passthrough())
    };
    drop(root);
    // The selected route alone must prevent replacement of the charged root.
    let blocked = revision()?;
    let blocked_weight = weight(&blocked)?;
    ensure!(
        cache.insert(2, blocked, blocked_weight, 0).err()
            == Some(crate::tenancy::cache::CacheError::Capacity)
    );
    ensure!(cache.get(&1)?.is_some());
    drop(selected);

    let mut second = revision()?;
    second.revision = "r2".try_into()?;
    insert(&mut second, "replacement", route("replacement", "second")?);
    let second = RetainedExecutionRevision::new(
        second.installation,
        second.tenant,
        second.revision,
        second.authority_revision,
        super::super::ValidatedEnvelope {
            learned_checkpoint: false,
            document: second.document,
            routes: second.routes,
        },
    )
    .map_err(|code| anyhow::anyhow!("replacement constructor failed: {code:?}"))?;
    let second_weight = weight(&second)?;
    let replacement = cache.insert(2, second, second_weight, 0)?;
    ensure!(replacement.revision_id().as_str() == "r2");
    ensure!(cache.get(&1)?.is_none(), "R1 was not evicted");
    ensure!(cache.get(&2)?.is_some(), "replacement is not resident");
    // Keep the public target beyond both root eviction and the cache lifetime.
    drop(replacement);
    drop(cache);
    let exported = target.headers.first().context("surviving header")?;
    let value = exported.default().context("surviving header value")?;
    ensure!(exported.name().as_str() == long_name);
    ensure!(value.as_bytes() == long_value.as_bytes());
    ensure!(value.is_sensitive() == sensitive);
    ensure!(exported.passthrough() == passthrough);
    ensure!(target.provider_name == "fixture" && target.service_id == "model");

    // Hosted candidates forbid passthrough, but the private copying primitive
    // must preserve it for other callers, including a rule without a default.
    let passthrough = OutboundHeaderRule::new(&long_name, None, true)?;
    let detached = passthrough.detached()?;
    ensure!(detached.name() == passthrough.name());
    ensure!(detached.passthrough() && detached.default().is_none());
    ensure!(!std::ptr::eq(
        detached.name().as_str().as_ptr(),
        passthrough.name().as_str().as_ptr(),
    ));
    Ok(())
}

#[test]
fn shared_policy_and_source_allocations_are_counted_once_per_revision() -> TestResult<()> {
    let mut value = revision()?;
    let mut definition = PolicyDefinition::default();
    definition
        .routes
        .insert("evidence".into(), "p".repeat(64 * 1024));
    let mut document = PolicyLock::default();
    document.policies.insert("policy".into(), definition);
    let document = Arc::new(document);
    let source = Arc::new(json!({"private": "s".repeat(32 * 1024)}));
    for name in ["first", "second"] {
        let mut route = route(name, "short")?;
        route.policy = Some(RetainedPolicy {
            reference: reference(ResourceKind::Policy, "policy")?,
            name: "policy".into(),
            document: document.clone(),
            routing_source: source.clone(),
        });
        insert(&mut value, name, route);
    }
    let shared = weight(&value)?;
    let second = value.routes.get_mut("second").context("second route")?;
    let policy = Arc::get_mut(second)
        .context("unshared route")?
        .policy
        .as_mut()
        .context("retained policy")?;
    policy.document = Arc::new((*document).clone());
    policy.routing_source = Arc::new((*source).clone());
    let separate = weight(&value)?;
    ensure!(separate >= shared + 96 * 1024);
    Ok(())
}

#[test]
fn spare_vec_and_hash_capacity_is_charged() -> TestResult<()> {
    let mut value = revision()?;
    let empty = weight(&value)?;
    value.document.routes.reserve_exact(64);
    let vector = weight(&value)?;
    ensure!(vector >= empty + 64 * size_of::<ResourceRef>());
    value.routes.reserve(64);
    let map = weight(&value)?;
    ensure!(map >= vector + value.routes.capacity() * size_of::<(String, Arc<RouteData>)>());
    Ok(())
}

fn over_reserved_id<T: TryFrom<String>>(text: &str) -> TestResult<T> {
    let mut storage = String::with_capacity(8192);
    storage.push_str(text);
    ensure!(storage.capacity() > storage.len());
    storage
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid fixture ID"))
}

fn over_reserved_reference(kind: ResourceKind, name: &str) -> TestResult<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: over_reserved_id("tenant")?,
        resource_kind: kind,
        resource_id: over_reserved_id(name)?,
        revision_id: over_reserved_id("r1")?,
    })
}

fn assert_compact_id(value: impl Into<String>, expected: &str) -> TestResult<()> {
    // Consume the actual returned allocation. Cloning the opaque ID first
    // would trim String capacity and could hide a missing normalization step.
    let storage = value.into();
    ensure!(storage == expected, "normalization changed identifier text");
    ensure!(
        storage.capacity() == storage.len(),
        "identifier retained spare capacity"
    );
    Ok(())
}

fn assert_compact_reference(
    value: ResourceRef,
    expected_kind: ResourceKind,
    expected_name: &str,
) -> TestResult<()> {
    let ResourceRef {
        owner_kind,
        owner_id,
        resource_kind,
        resource_id,
        revision_id,
    } = value;
    ensure!(owner_kind == OwnerKind::Tenant && resource_kind == expected_kind);
    assert_compact_id(owner_id, "tenant")?;
    assert_compact_id(resource_id, expected_name)?;
    assert_compact_id(revision_id, "r1")
}

#[test]
fn constructor_compacts_actual_root_and_every_retained_reference_allocation() -> TestResult<()> {
    // Establish that the public opaque-ID constructor really retains excess
    // capacity, so this fixture fails if the private normalization is removed.
    let probe: bitrouter_ai::tenancy::identity::InstallationId = over_reserved_id("installation")?;
    let probe: String = probe.into();
    ensure!(probe.capacity() > probe.len());

    let mut document = revision()?.document;
    document.routes = vec![over_reserved_reference(ResourceKind::Route, "route")?];
    document.policy = Some(over_reserved_reference(ResourceKind::Policy, "policy")?);
    let mut data = route("route", "short")?;
    data.assessment = Some(over_reserved_reference(ResourceKind::Route, "judge")?);
    data.reference = over_reserved_reference(ResourceKind::Route, "route")?;
    data.dependencies = vec![
        over_reserved_reference(ResourceKind::Route, "route")?,
        over_reserved_reference(ResourceKind::Policy, "policy")?,
        over_reserved_reference(ResourceKind::Credential, "slot")?,
    ];
    data.policy = Some(RetainedPolicy {
        reference: over_reserved_reference(ResourceKind::Policy, "policy")?,
        name: "policy".into(),
        document: Arc::new(PolicyLock::default()),
        routing_source: Arc::new(json!({})),
    });
    for candidate in &mut data.candidates {
        candidate.credential = over_reserved_reference(ResourceKind::Credential, "slot")?;
    }
    let value = RetainedExecutionRevision::new(
        over_reserved_id("installation")?,
        over_reserved_id("tenant")?,
        over_reserved_id("r1")?,
        1.try_into()?,
        super::super::ValidatedEnvelope {
            learned_checkpoint: false,
            document,
            routes: HashMap::from([("route".into(), Arc::new(data))]),
        },
    )
    .map_err(|code| anyhow::anyhow!("constructor failed: {code:?}"))?;
    let RetainedExecutionRevision {
        installation,
        tenant,
        revision,
        authority_revision: _,
        document,
        mut routes,
    } = value;
    assert_compact_id(installation, "installation")?;
    assert_compact_id(tenant, "tenant")?;
    assert_compact_id(revision, "r1")?;
    ensure!(document.routes.len() == 1);
    for root in document.routes {
        assert_compact_reference(root, ResourceKind::Route, "route")?;
    }
    assert_compact_reference(
        document.policy.context("document policy")?,
        ResourceKind::Policy,
        "policy",
    )?;
    let data = Arc::try_unwrap(routes.remove("route").context("route")?)
        .map_err(|_| anyhow::anyhow!("unexpected shared route"))?;
    let RouteData {
        assessment,
        block_pricing: _,
        block: _,
        reference,
        dependencies,
        selector: _,
        overrides: _,
        policy,
        chains: _,
        native: _,
        candidates,
        choice: _,
    } = data;
    assert_compact_reference(reference, ResourceKind::Route, "route")?;
    assert_compact_reference(
        assessment.context("assessment ref")?,
        ResourceKind::Route,
        "judge",
    )?;
    ensure!(dependencies.len() == 3);
    for (dependency, (kind, name)) in dependencies.into_iter().zip([
        (ResourceKind::Route, "route"),
        (ResourceKind::Policy, "policy"),
        (ResourceKind::Credential, "slot"),
    ]) {
        assert_compact_reference(dependency, kind, name)?;
    }
    let RetainedPolicy {
        reference,
        name: _,
        document: _,
        routing_source: _,
    } = policy.context("retained policy")?;
    assert_compact_reference(reference, ResourceKind::Policy, "policy")?;
    ensure!(candidates.len() == 1);
    for Candidate {
        target: _,
        projection: _,
        credential,
    } in candidates
    {
        assert_compact_reference(credential, ResourceKind::Credential, "slot")?;
    }
    Ok(())
}

#[test]
fn json_arrays_and_strings_charge_spare_capacity_without_serializing() -> TestResult<()> {
    let mut text = String::with_capacity(4096);
    text.push('x');
    let text_capacity = text.capacity();
    let mut array = Vec::with_capacity(128);
    array.push(Value::String(text));
    let expected_minimum = array.capacity() * size_of::<Value>() + text_capacity;
    let mut measured = Weight::new();
    measured
        .json(&Value::Array(array), 0)
        .map_err(|code| anyhow::anyhow!("JSON weight failed: {code:?}"))?;
    ensure!(measured.bytes >= expected_minimum);
    Ok(())
}

#[test]
fn accounting_overflow_and_excessive_traversal_fail_closed() -> TestResult<()> {
    let mut weight = Weight::new();
    weight.bytes = usize::MAX;
    ensure!(matches!(weight.add(1), Err(ErrorCode::CapacityExceeded)));
    ensure!(matches!(
        product(usize::MAX, 2),
        Err(ErrorCode::CapacityExceeded)
    ));
    let mut weight = Weight::new();
    ensure!(matches!(
        weight.array::<u64>(usize::MAX),
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!(matches!(
        weight.visits(MAX_VISITS + 1),
        Err(ErrorCode::CapacityExceeded)
    ));
    let mut nested = Value::Null;
    for _ in 0..MAX_JSON_DEPTH + 2 {
        nested = Value::Array(vec![nested]);
    }
    ensure!(matches!(
        Weight::new().json(&nested, 0),
        Err(ErrorCode::CapacityExceeded)
    ));
    Ok(())
}

#[test]
fn unselected_native_selectors_and_index_spare_capacity_are_charged() -> TestResult<()> {
    let mut value = revision()?;
    insert(&mut value, "chosen", route("chosen", "short")?);
    insert(&mut value, "other", route("other", "short")?);
    let chosen = value
        .select("chosen", &ApiProtocol::ChatCompletions)
        .map_err(|code| anyhow::anyhow!("selection failed: {code:?}"))?;
    let before = weight(&value)?;
    let other =
        Arc::get_mut(value.routes.get_mut("other").context("other")?).context("unique route")?;
    let NativeChains::Ready(chains) = &mut other.native else {
        anyhow::bail!("ready native chain");
    };
    let mut selector = String::with_capacity(4096);
    selector.push_str("tier");
    let mut indices = Vec::with_capacity(2048);
    indices.push(0);
    let additional = selector.capacity() + indices.capacity() * size_of::<usize>();
    chains.push(NativeChain {
        selector,
        operation: bitrouter_ai::types::ModelOperation::Generation,
        candidates: indices,
    });
    ensure!(weight(&value)? >= before + additional);
    ensure!(chosen.reference().resource_id.as_str() == "chosen");
    Ok(())
}
