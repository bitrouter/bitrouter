use bitrouter_ai::tenancy::{
    authority::{GrantScope, MAX_GRANT_SCOPE_BYTES, OwnerKind, ResourceKey, ResourceKind},
    validation::Validate,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn keys(count: usize) -> Result<Vec<ResourceKey>, Box<dyn std::error::Error>> {
    (0..count)
        .map(|index| {
            Ok(ResourceKey {
                owner_kind: OwnerKind::OperatorPool,
                owner_id: "pool".try_into()?,
                resource_kind: ResourceKind::Policy,
                resource_id: format!("policy-{index:06}").try_into()?,
            })
        })
        .collect()
}

/// UT for scopes above the former count bound, preserving explicit ownership.
#[test]
fn grant_scope_uses_bytes_without_weakening_uniqueness_or_pool_ownership() -> TestResult {
    for scope in [
        GrantScope::Resources {
            resources: keys(343)?,
        },
        GrantScope::Tenant {
            operator_pools: keys(343)?,
        },
    ] {
        scope.validate()?;
        let roundtrip: GrantScope = serde_json::from_slice(&serde_json::to_vec(&scope)?)?;
        assert_eq!(roundtrip, scope);
        roundtrip.validate()?;
    }
    let mut resources = keys(343)?;
    resources.push(resources[0].clone());
    for scope in [
        GrantScope::Resources {
            resources: resources.clone(),
        },
        GrantScope::Tenant {
            operator_pools: resources,
        },
        GrantScope::Resources { resources: vec![] },
    ] {
        assert!(scope.validate().is_err());
    }
    GrantScope::Tenant {
        operator_pools: vec![],
    }
    .validate()?;
    let mut wrong_owner = keys(343)?;
    wrong_owner[342].owner_kind = OwnerKind::Tenant;
    assert!(
        GrantScope::Tenant {
            operator_pools: wrong_owner
        }
        .validate()
        .is_err()
    );
    Ok(())
}

/// UT for every previously allowed key width fitting the new encoded budget.
#[test]
fn grant_scope_preserves_the_old_maximum_width_256_key_scopes() -> TestResult {
    let mut resources = keys(256)?;
    for resource in &mut resources {
        resource.owner_id = "o".repeat(128).try_into()?;
        resource.resource_id = format!("{}{}", resource.resource_id, "r".repeat(115)).try_into()?;
        resource.resource_kind = ResourceKind::Configuration;
    }
    for scope in [
        GrantScope::Resources {
            resources: resources.clone(),
        },
        GrantScope::Tenant {
            operator_pools: resources,
        },
    ] {
        assert!(serde_json::to_vec(&scope)?.len() < MAX_GRANT_SCOPE_BYTES);
        scope.validate()?;
    }
    Ok(())
}

fn scope_at_bytes(target: usize, tenant: bool) -> Result<GrantScope, Box<dyn std::error::Error>> {
    let scope = |resources| {
        if tenant {
            GrantScope::Tenant {
                operator_pools: resources,
            }
        } else {
            GrantScope::Resources { resources }
        }
    };
    let sample = keys(1)?.pop().ok_or("key missing")?;
    let sample_bytes = serde_json::to_vec(&sample)?.len();
    // Tenant omits an empty pool array, so measure a present array envelope.
    let envelope = serde_json::to_vec(&scope(vec![sample]))?.len() - sample_bytes;
    let row = sample_bytes + 1;
    let mut resources = keys((target - envelope) / row)?;
    let mut remaining = target - serde_json::to_vec(&scope(resources.clone()))?.len();
    for resource in &mut resources {
        let padding = remaining.min(128 - resource.resource_id.as_str().len());
        resource.resource_id =
            format!("{}{}", resource.resource_id, "r".repeat(padding)).try_into()?;
        remaining -= padding;
    }
    assert_eq!(remaining, 0);
    let result = scope(resources);
    assert_eq!(serde_json::to_vec(&result)?.len(), target);
    Ok(result)
}

/// UT for the inclusive encoded limit and refusal before duplicate-set allocation.
#[test]
fn grant_scope_accepts_exact_byte_limit_and_rejects_one_more_byte_first() -> TestResult {
    for tenant in [false, true] {
        scope_at_bytes(MAX_GRANT_SCOPE_BYTES, tenant)?.validate()?;
        let oversized = scope_at_bytes(MAX_GRANT_SCOPE_BYTES + 1, tenant)?;
        assert_eq!(
            oversized
                .validate()
                .err()
                .ok_or("oversized scope accepted")?
                .to_string(),
            "invalid tenancy contract: serialized byte limit"
        );
    }
    let duplicate = keys(1)?.pop().ok_or("key missing")?;
    let oversized = GrantScope::Resources {
        resources: vec![duplicate; MAX_GRANT_SCOPE_BYTES / 32],
    };
    assert_eq!(
        oversized
            .validate()
            .err()
            .ok_or("oversized duplicate scope accepted")?
            .to_string(),
        "invalid tenancy contract: serialized byte limit"
    );
    Ok(())
}

/// UT for canonical and SDK-packaged schemas sharing the broadened shape.
#[cfg(feature = "schema")]
#[test]
fn grant_scope_schemas_preserve_shapes_without_the_old_cardinality_ceiling() -> TestResult {
    let schema = serde_json::to_value(schemars::schema_for!(GrantScope))?;
    for document in [
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../docs/contracts/resource-tenancy/schema.json"
        ))?,
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../docs/specs/service-api/directory.schema.json"
        ))?,
    ] {
        let scope = &document["$defs"]["GrantScope"];
        assert_eq!(schema["oneOf"].as_array().ok_or("scope variants")?.len(), 2);
        assert!(
            scope["oneOf"][0]["properties"]["operator_pools"]
                .get("maxItems")
                .is_none()
        );
        assert!(
            scope["oneOf"][1]["properties"]["resources"]
                .get("maxItems")
                .is_none()
        );
        assert_eq!(scope["oneOf"][1]["properties"]["resources"]["minItems"], 1);
        assert_eq!(
            scope["oneOf"][1]["properties"]["resources"]["uniqueItems"],
            true
        );
        assert_eq!(scope["description"], schema["description"]);
    }
    Ok(())
}
