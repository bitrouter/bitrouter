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

/// The packaged wire, directory, and Rust schemas accept the same scope shapes.
#[cfg(feature = "schema")]
#[test]
fn grant_scope_schemas_preserve_shapes_without_the_old_cardinality_ceiling() -> TestResult {
    use serde_json::{Value, json, to_value};

    let scoped = |document: &str| -> Result<Value, serde_json::Error> {
        let document: Value = serde_json::from_str(document)?;
        Ok(json!({
            "$schema": document["$schema"],
            "$defs": document["$defs"],
            "$ref": "#/$defs/GrantScope",
        }))
    };
    let schemas = [
        (
            "tenancy wire schema",
            scoped(include_str!("../src/tenancy/wire-schema.json"))?,
        ),
        (
            "service directory schema",
            scoped(include_str!(
                "../../../docs/specs/service-api/directory.schema.json"
            ))?,
        ),
        (
            "GrantScope JsonSchema",
            to_value(schemars::schema_for!(GrantScope))?,
        ),
    ];
    let pool_key = to_value(keys(1)?.pop().ok_or("key missing")?)?;
    let mut tenant_key = pool_key.clone();
    tenant_key["owner_kind"] = json!("tenant");
    let mut invalid_key = pool_key.clone();
    invalid_key["resource_id"] = json!("invalid id");
    let cases = [
        (
            "resource scope above the former count limit",
            to_value(GrantScope::Resources {
                resources: keys(343)?,
            })?,
            true,
        ),
        (
            "tenant scope above the former count limit",
            to_value(GrantScope::Tenant {
                operator_pools: keys(343)?,
            })?,
            true,
        ),
        ("omitted tenant pools", json!({"kind": "tenant"}), true),
        (
            "empty tenant pools",
            json!({"kind": "tenant", "operator_pools": []}),
            true,
        ),
        ("missing resource list", json!({"kind": "resources"}), false),
        (
            "empty resource list",
            json!({"kind": "resources", "resources": []}),
            false,
        ),
        (
            "duplicate resources",
            json!({"kind": "resources", "resources": [pool_key, pool_key]}),
            false,
        ),
        (
            "duplicate operator pools",
            json!({"kind": "tenant", "operator_pools": [pool_key, pool_key]}),
            false,
        ),
        (
            "tenant key in resource scope",
            json!({"kind": "resources", "resources": [tenant_key]}),
            true,
        ),
        (
            "tenant key in operator pool list",
            json!({"kind": "tenant", "operator_pools": [tenant_key]}),
            false,
        ),
        (
            "resource key with an invalid identifier",
            json!({"kind": "resources", "resources": [invalid_key]}),
            false,
        ),
        (
            "valid shape above the runtime byte limit",
            to_value(scope_at_bytes(MAX_GRANT_SCOPE_BYTES + 1, false)?)?,
            true,
        ),
    ];
    for (source, schema) in schemas {
        let validator = jsonschema::validator_for(&schema)?;
        for (case, value, expected) in &cases {
            assert_eq!(validator.is_valid(value), *expected, "{source}: {case}");
        }
    }
    Ok(())
}
