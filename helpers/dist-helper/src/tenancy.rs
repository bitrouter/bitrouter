//! Validate frozen inputs and public contracts without claiming runtime authorization.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

mod contracts;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    name: String,
    schema: String,
    valid: bool,
    /// Separate shape validity from local, cross-field semantic validity.
    semantic_valid: Option<bool>,
    value: Value,
}

fn read(root: &Path, name: &str) -> Result<Value> {
    let path = root.join("docs/contracts/resource-tenancy").join(name);
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn array(value: &Value) -> Result<&[Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .context("expected array")
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("missing nonempty string {field}"))
}

fn strings(value: &Value) -> Result<Vec<&str>> {
    array(value)?
        .iter()
        .map(|value| value.as_str().context("expected string"))
        .collect()
}

fn number(value: &Value, field: &str) -> Result<u64> {
    value[field]
        .as_u64()
        .with_context(|| format!("missing unsigned field {field}"))
}

fn size(value: &Value) -> Result<u64> {
    u64::try_from(serde_json::to_vec(value)?.len()).context("serialized size overflow")
}

fn shape_valid(schema: &Value, shape: &str, value: &Value) -> Result<bool> {
    ensure!(
        schema["$defs"].get(shape).is_some(),
        "unknown schema {shape}"
    );
    let mut selected = schema.clone();
    selected["$ref"] = format!("#/$defs/{shape}").into();
    // JSON Schema 2020-12, local references only. No remote schema fetches.
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&selected)
        .map_err(|error| anyhow::anyhow!("compiling {shape}: {error}"))?;
    Ok(validator.is_valid(value))
}

fn validate_profile(profile: &Value, schema: &Value) -> Result<()> {
    let tenants = number(profile, "tenants")?;
    let limits = &schema["$defs"]["Configuration"]["properties"]["limits"]["properties"];
    for (tenant, instance, limit) in [
        ("tenant_active", "instance_active", "active_executions"),
        ("tenant_queue", "instance_queue", "queued_work"),
        ("tenant_learning", "instance_learning", "learning_jobs"),
    ] {
        let per_tenant = number(profile, tenant)?;
        let per_instance = number(profile, instance)?;
        ensure!(per_tenant <= per_instance, "{tenant} exceeds {instance}");
        ensure!(
            per_instance <= number(&limits[limit], "maximum")?,
            "{instance} exceeds the wire ceiling"
        );
        let configured = tenants
            .checked_mul(per_tenant)
            .context("configured concurrency overflow")?;
        ensure!(
            per_instance <= configured,
            "unreachable {instance} capacity"
        );
    }
    ensure!(
        number(profile, "max_attempts")?
            == number(
                &schema["$defs"]["ExecutionReceipt"]["properties"]["attempts"],
                "maxItems"
            )?,
        "attempt bound differs from schema"
    );
    ensure!(
        number(profile, "max_references")?
            == number(
                &schema["$defs"]["Configuration"]["properties"]["routes"],
                "maxItems"
            )?,
        "reference bound differs from schema"
    );
    let receipt = number(profile, "max_receipt_bytes")?;
    let per_execution = number(profile, "completion_reserve_bytes_per_execution")?;
    ensure!(
        per_execution
            >= receipt
                .checked_mul(4)
                .context("receipt allowance overflow")?,
        "terminal reservation omits the WAL/index allowance"
    );
    let reserve = number(profile, "instance_active")?
        .checked_mul(per_execution)
        .and_then(|value| value.checked_add(profile["maintenance_floor_bytes"].as_u64()?))
        .context("completion headroom overflow")?;
    ensure!(
        number(profile, "strict_admission_free_floor_bytes")? >= reserve,
        "completion storage is under-reserved"
    );
    ensure!(
        number(profile, "receipt_high_water_bytes")? >= reserve,
        "receipt high-water below headroom"
    );
    let unknown_per_tenant = number(profile, "unknown_queue_per_tenant")?;
    let unknown_per_instance = number(profile, "unknown_queue_per_instance")?;
    ensure!(
        unknown_per_tenant <= unknown_per_instance,
        "unknown tenant queue exceeds instance"
    );
    ensure!(
        unknown_per_instance
            <= tenants
                .checked_mul(unknown_per_tenant)
                .context("unknown queue overflow")?,
        "unknown queue exceeds configured tenant capacity"
    );
    ensure!(
        number(profile, "queue_wait_ms")? <= number(profile, "request_timeout_ms")?,
        "queue wait exceeds request deadline"
    );
    for concurrency in array(&profile["concurrency"])? {
        let concurrency = concurrency.as_u64().context("invalid concurrency")?;
        ensure!(
            concurrency > 0 && concurrency <= number(profile, "instance_active")?,
            "measurement concurrency exceeds admission bound"
        );
    }
    let credentials = tenants
        .checked_mul(number(profile, "principals_per_tenant")?)
        .and_then(|value| value.checked_mul(profile["credentials_per_principal"].as_u64()?))
        .context("credential capacity overflow")?;
    let safety_minimum = tenants
        .checked_mul(2)
        .and_then(|value| value.checked_add(credentials))
        .context("safety operation capacity overflow")?;
    let safety = number(profile, "reserved_safety_operation_ids")?;
    ensure!(
        safety >= safety_minimum,
        "safety reserve cannot cover credential revokes and tenant stops"
    );
    ensure!(
        safety < number(profile, "max_operation_tombstones_per_instance")?,
        "no ordinary operation capacity"
    );
    Ok(())
}

fn validate_manifest(manifest: &Value) -> Result<()> {
    let installation = string(manifest, "source_installation_id")?;
    let mut keys = HashSet::new();
    let mut tenant_owners: HashMap<&str, HashSet<(&str, &str)>> = HashMap::new();
    let mut approvals = HashSet::new();
    for approval in array(&manifest["tenant_merge_approvals"])? {
        ensure!(
            approvals.insert(string(approval, "tenant_id")?),
            "duplicate tenant merge approval"
        );
    }
    for mapping in array(&manifest["mappings"])? {
        ensure!(
            string(mapping, "source_installation_id")? == installation,
            "mapping installation differs from manifest"
        );
        let kind = string(mapping, "source_kind")?;
        let owner = mapping["old_owner"].as_str();
        let resource = string(mapping, "resource_kind")?;
        ensure!(
            keys.insert((kind, owner, resource)),
            "duplicate or conflicting source owner mapping"
        );
        if mapping["disposition"] == "mapped" {
            ensure!(
                kind != "unresolved",
                "unresolved ownership cannot grant access"
            );
            tenant_owners
                .entry(string(mapping, "tenant_id")?)
                .or_default()
                .insert((kind, owner.context("mapped owner missing")?));
        }
        let examples = array(&mapping["resource_examples"])?;
        ensure!(
            u64::try_from(examples.len())? <= number(mapping, "source_count")?,
            "more examples than source rows"
        );
        let mut legacy_ids = HashSet::new();
        for example in examples {
            ensure!(
                legacy_ids.insert(string(example, "legacy_resource_id")?),
                "duplicate legacy resource example"
            );
        }
    }
    for (tenant, owners) in &tenant_owners {
        ensure!(
            owners.len() <= 1 || approvals.contains(tenant),
            "tenant merge lacks explicit operator evidence"
        );
    }
    for tenant in approvals {
        ensure!(
            tenant_owners
                .get(tenant)
                .is_some_and(|owners| owners.len() > 1),
            "merge approval has no corresponding owner merge"
        );
    }
    Ok(())
}

fn validate_configuration(document: &Value, profile: &Value) -> Result<()> {
    let routes = array(&document["routes"])?;
    let references = u64::try_from(routes.len())?
        .checked_add(u64::from(!document["policy"].is_null()))
        .context("reference count overflow")?;
    ensure!(
        references <= number(profile, "max_references")?,
        "aggregate reference bound exceeded"
    );
    ensure!(
        size(document)? <= number(profile, "max_config_bytes")?,
        "configuration byte bound exceeded"
    );
    let mut identities = HashSet::new();
    for reference in routes
        .iter()
        .chain(document.get("policy").filter(|value| !value.is_null()))
    {
        let key = (
            string(reference, "owner_kind")?,
            string(reference, "owner_id")?,
            string(reference, "resource_kind")?,
            string(reference, "resource_id")?,
        );
        ensure!(
            identities.insert(key),
            "one configuration names multiple revisions of a resource"
        );
    }
    Ok(())
}

fn validate_page(page: &Value) -> Result<()> {
    let ids = strings(&page["execution_ids"])?;
    ensure!(
        ids.iter()
            .zip(ids.iter().skip(1))
            .all(|(left, right)| left < right),
        "execution IDs must be strictly ascending ASCII"
    );
    if let Some(after) = page["next_after"].as_str() {
        ensure!(
            ids.last().copied() == Some(after),
            "page cursor must equal its last execution ID"
        );
    }
    Ok(())
}

fn validate_receipt(receipt: &Value, profile: &Value) -> Result<()> {
    ensure!(
        size(receipt)? <= number(profile, "max_receipt_bytes")?,
        "receipt byte bound exceeded"
    );
    let mut ids = HashSet::new();
    let attempts = array(&receipt["attempts"])?;
    let provenance = if attempts.is_empty()
        || attempts
            .iter()
            .any(|attempt| attempt["usage"]["provenance"] == "unknown")
    {
        "unknown"
    } else if attempts
        .iter()
        .any(|attempt| attempt["usage"]["provenance"] == "estimated")
    {
        "estimated"
    } else {
        "provider"
    };
    ensure!(
        receipt["usage"]["provenance"] == provenance,
        "aggregate provenance must conservatively include every attempt"
    );
    ensure!(
        receipt["usage"]["complete"] != true
            || (!attempts.is_empty()
                && attempts
                    .iter()
                    .all(|attempt| attempt["usage"]["complete"] == true)),
        "incomplete attempt cannot produce complete aggregate usage"
    );
    for attempt in attempts {
        ensure!(
            ids.insert(string(attempt, "attempt_id")?),
            "duplicate attempt identity"
        );
        let native = &attempt["native_usage"];
        ensure!(
            size(native)? <= number(profile, "max_native_usage_bytes_per_attempt")?,
            "native usage byte bound exceeded"
        );
        let mut names = HashSet::new();
        for quantity in array(native)? {
            ensure!(
                names.insert(string(quantity, "name")?),
                "duplicate native usage dimension"
            );
        }
    }
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
    ] {
        if let Some(total) = receipt["usage"][field].as_u64() {
            ensure!(
                !attempts.is_empty(),
                "observed receipt total has no attempts"
            );
            let sum = attempts.iter().try_fold(0_u64, |sum, attempt| {
                sum.checked_add(attempt["usage"][field].as_u64()?)
            });
            ensure!(
                sum == Some(total),
                "receipt total must include every attempt: {field}"
            );
        }
    }
    Ok(())
}

fn validate_eligibility(value: &Value) -> Result<()> {
    let capability = &value["capabilities"];
    ensure!(capability.is_object(), "tenant capabilities are absent");
    ensure!(
        capability["contract_version"] == 1,
        "unsupported tenant contract version"
    );
    for field in [
        "revision_bound_dispatch",
        "strict_receipts",
        "dispatch_enabled",
    ] {
        ensure!(capability[field] == true, "backend is ineligible: {field}");
    }
    for (field, header) in [
        ("installation_id", "Bitrouter-Installation-Id"),
        ("instance_id", "Bitrouter-Instance-Id"),
        ("capability_revision", "Bitrouter-Capability-Revision"),
    ] {
        ensure!(
            string(capability, field)? == string(&value["dispatch"], header)?,
            "stale or wrong dispatch binding: {field}"
        );
    }
    Ok(())
}

fn validate_semantics(shape: &str, value: &Value, schema: &Value, profile: &Value) -> Result<()> {
    match shape {
        "CapacityProfile" => validate_profile(value, schema),
        "MigrationManifest" => validate_manifest(value),
        "Configuration" => validate_configuration(value, profile),
        "ConfigurationPut" | "ConfigurationRevision" => {
            validate_configuration(&value["document"], profile)
        }
        "ExecutionPage" => validate_page(value),
        "ExecutionReceipt" => validate_receipt(value, profile),
        "DispatchEligibility" => validate_eligibility(value),
        _ => Ok(()),
    }
}

fn validate_operations(operations: &Value, covered: &HashSet<&str>) -> Result<HashSet<String>> {
    let operations = array(operations)?;
    ensure!(!operations.is_empty(), "missing operations");
    let mut names = HashSet::new();
    let mut routes = HashSet::new();
    for operation in operations {
        let name = string(operation, "name")?;
        let method = string(operation, "method")?;
        let path = string(operation, "path")?;
        ensure!(names.insert(name.to_owned()), "duplicate operation {name}");
        ensure!(
            ["GET", "POST", "PUT", "DELETE"].contains(&method),
            "invalid method for {name}"
        );
        ensure!(
            path.starts_with("/control/v1/"),
            "unprotected proposed operation {name}"
        );
        ensure!(
            routes.insert((method, path)),
            "duplicate method/path for {name}"
        );
        let response = string(operation, "response")?;
        ensure!(
            covered.contains(response),
            "operation {name} lacks a positive response fixture"
        );
        if method == "GET" {
            ensure!(
                operation.get("request") == Some(&Value::Null),
                "GET request must be explicitly null"
            );
        } else {
            let request = string(operation, "request")?;
            ensure!(
                covered.contains(request),
                "operation {name} lacks a positive request fixture"
            );
        }
    }
    Ok(names)
}

pub(crate) fn validate(root: &Path) -> Result<()> {
    let schema = read(root, "schema.json")?;
    contracts::check_packaged(root, &schema)?;
    let profile = read(root, "benchmark-profile.json")?;
    ensure!(
        shape_valid(&schema, "CapacityProfile", &profile)?,
        "invalid capacity profile shape"
    );
    validate_profile(&profile, &schema)?;
    let manifest = read(root, "migration-example.json")?;
    ensure!(
        shape_valid(&schema, "MigrationManifest", &manifest)?,
        "invalid migration example shape"
    );
    validate_manifest(&manifest)?;
    let fixtures: Vec<Fixture> = serde_json::from_value(read(root, "fixtures.json")?)?;
    ensure!(!fixtures.is_empty(), "missing contract fixtures");
    let mut names = HashSet::new();
    let mut covered = HashSet::new();
    let mut semantic_negatives = 0;
    for fixture in &fixtures {
        ensure!(
            names.insert(&fixture.name),
            "duplicate fixture {}",
            fixture.name
        );
        let actual = shape_valid(&schema, &fixture.schema, &fixture.value)?;
        ensure!(
            actual == fixture.valid,
            "fixture {} expected shape valid={}, got {actual}",
            fixture.name,
            fixture.valid
        );
        contracts::check(fixture, &schema)?;
        if fixture.valid {
            let semantic = validate_semantics(&fixture.schema, &fixture.value, &schema, &profile);
            let expected = fixture.semantic_valid.unwrap_or(true);
            ensure!(
                semantic.is_ok() == expected,
                "fixture {} expected semantic valid={expected}: {semantic:?}",
                fixture.name
            );
            if expected {
                covered.insert(fixture.schema.as_str());
            } else {
                semantic_negatives += 1;
            }
        } else {
            ensure!(
                fixture.semantic_valid.is_none(),
                "semantic expectation requires a shape-valid fixture"
            );
        }
    }
    ensure!(
        semantic_negatives > 0,
        "missing semantic rejection fixtures"
    );
    let operations = read(root, "operations.json")?;
    let operation_names = validate_operations(&operations, &covered)?;
    let scenarios = read(root, "scenarios.json")?;
    let mut scenario_names = HashSet::new();
    ensure!(
        !array(&scenarios)?.is_empty(),
        "missing deferred runtime scenarios"
    );
    for scenario in array(&scenarios)? {
        ensure!(
            scenario_names.insert(string(scenario, "name")?),
            "duplicate scenario name"
        );
        ensure!(
            operation_names.contains(string(scenario, "operation")?),
            "scenario has unknown operation"
        );
        string(scenario, "given")?;
        string(scenario, "expected")?;
        ensure!(
            scenario["verification"] == "required-in-M1-M3-runtime-tests",
            "M0 cannot claim runtime scenario verification"
        );
    }
    let inventory = read(root, "inventory.json")?;
    let mut families = HashSet::new();
    ensure!(!array(&inventory)?.is_empty(), "missing inventory");
    for family in array(&inventory)? {
        ensure!(
            families.insert(string(family, "name")?),
            "duplicate inventory family"
        );
        for field in ["existing", "target", "obligations", "exclusions"] {
            string(family, field)?;
        }
        let paths = strings(&family["paths"])?;
        ensure!(!paths.is_empty(), "inventory family has no source paths");
        for path in paths {
            ensure!(
                Path::new(path)
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
                "inventory path must be repository relative"
            );
            ensure!(root.join(path).exists(), "inventory source missing: {path}");
        }
    }
    println!(
        "validated {} shape fixtures ({} semantic rejections), {} operations, actual migration example and capacity inputs; runtime scenarios remain unimplemented",
        fixtures.len(),
        semantic_negatives,
        operation_names.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// UT for committed wire fixtures and local semantic invariants.
    #[test]
    fn ut_committed_tenancy_inputs_validate() -> Result<()> {
        validate(&crate::workspace_root()?)
    }

    /// UT for lookup coverage that must not disappear through a missing shape.
    #[test]
    fn ut_operation_without_response_is_rejected() {
        let operations = json!([{"name":"lookup", "method":"GET", "path":"/control/v1/tenancy/operations/{operation_id}", "request":null}]);
        assert!(validate_operations(&operations, &HashSet::new()).is_err());
    }

    #[test]
    fn ut_integral_json_number_spellings_match_frozen_and_public_schemas() -> Result<()> {
        let root = crate::workspace_root()?;
        let schema = read(&root, "schema.json")?;
        let fixtures: Vec<Fixture> = serde_json::from_value(read(&root, "fixtures.json")?)?;
        let original = fixtures
            .into_iter()
            .find(|f| f.name == "scoped-credential")
            .context("missing issuance fixture")?;
        for spelling in ["1.0", "1e0", "125e-2", "9007199254740992"] {
            let value: Value = serde_json::from_str(spelling)?;
            let mut fixture = Fixture {
                name: format!("integral-number-{spelling}"),
                schema: original.schema.clone(),
                valid: matches!(spelling, "1.0" | "1e0"),
                semantic_valid: None,
                value: original.value.clone(),
            };
            fixture.value["expected_authority_revision"] = value;
            assert_eq!(
                shape_valid(&schema, &fixture.schema, &fixture.value)?,
                fixture.valid
            );
            contracts::check(&fixture, &schema)?;
        }
        Ok(())
    }

    /// UT for conflict detection even when both mappings are individually valid.
    #[test]
    fn ut_conflicting_owner_mapping_is_rejected() -> Result<()> {
        let mut manifest = read(&crate::workspace_root()?, "migration-example.json")?;
        let mut duplicate = array(&manifest["mappings"])?
            .first()
            .context("missing mapping")?
            .clone();
        duplicate["tenant_id"] = "different-tenant".into();
        manifest["mappings"]
            .as_array_mut()
            .context("missing mappings")?
            .push(duplicate);
        assert!(validate_manifest(&manifest).is_err());
        Ok(())
    }
}
