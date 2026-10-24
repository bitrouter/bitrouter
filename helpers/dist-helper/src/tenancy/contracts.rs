//! Compare real public value types with the independent frozen wire fixtures.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{authority, configuration, execution, operation, validation::Validate};
use serde::{Serialize, de::DeserializeOwned};

use super::{Fixture, shape_valid};

fn typed<T: DeserializeOwned + Serialize + Validate + schemars::JsonSchema>(
    fixture: &Fixture,
    schema: &serde_json::Value,
) -> Result<()> {
    let generated = schemars::schema_for!(T);
    let generated = serde_json::to_value(generated)?;
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&generated)?;
    ensure!(
        validator.is_valid(&fixture.value) == fixture.valid,
        "generated schema disagrees with frozen fixture {}",
        fixture.name
    );
    let decoded = serde_json::from_value::<T>(fixture.value.clone());
    let valid = decoded.as_ref().is_ok_and(|value| value.validate().is_ok());
    let expected = fixture.valid && fixture.semantic_valid.unwrap_or(true);
    ensure!(
        valid == expected,
        "public contract disagrees with fixture {} (expected {expected})",
        fixture.name
    );
    if valid {
        let wire = serde_json::to_value(decoded.context("decoding validated fixture")?)?;
        ensure!(
            shape_valid(schema, &fixture.schema, &wire)?,
            "public round trip violates frozen schema: {}",
            fixture.name
        );
        super::validate_semantics(
            &fixture.schema,
            &wire,
            schema,
            &super::read(&crate::workspace_root()?, "benchmark-profile.json")?,
        )?;
    }
    Ok(())
}

pub(super) fn check_packaged(root: &std::path::Path, frozen: &serde_json::Value) -> Result<()> {
    let wire: serde_json::Value = serde_json::from_slice(&std::fs::read(
        root.join("crates/bitrouter-ai/src/tenancy/wire-schema.json"),
    )?)?;
    let definitions = wire["$defs"]
        .as_object()
        .context("packaged schema definitions")?;
    ensure!(!definitions.is_empty(), "packaged schema is empty");
    for (name, definition) in definitions {
        ensure!(
            frozen["$defs"].get(name) == Some(definition),
            "packaged wire definition drift: {name}"
        );
    }
    Ok(())
}

pub(super) fn check(fixture: &Fixture, schema: &serde_json::Value) -> Result<()> {
    match fixture.schema.as_str() {
        "TenantPut" => typed::<authority::TenantPut>(fixture, schema),
        "PrincipalPut" => typed::<authority::PrincipalPut>(fixture, schema),
        "BindingPut" => typed::<authority::BindingPut>(fixture, schema),
        "CredentialCreate" => typed::<authority::CredentialCreate>(fixture, schema),
        "CredentialIssued" => typed::<authority::CredentialIssued>(fixture, schema),
        "CredentialStatus" => typed::<authority::CredentialStatus>(fixture, schema),
        "AuditReaderCreate" => typed::<authority::AuditReaderCreate>(fixture, schema),
        "GrantScope" => typed::<authority::GrantScope>(fixture, schema),
        "CredentialRevoke" | "Mutation" => typed::<operation::Mutation>(fixture, schema),
        "OperationResult" => typed::<operation::OperationResult>(fixture, schema),
        "Error" => typed::<operation::Error>(fixture, schema),
        "TenantCapabilities" => typed::<operation::TenantCapabilities>(fixture, schema),
        "DispatchBinding" => typed::<operation::DispatchBinding>(fixture, schema),
        "ConfigurationPut" => typed::<configuration::ConfigurationPut>(fixture, schema),
        "ConfigurationRevision" => typed::<configuration::ConfigurationRevision>(fixture, schema),
        "Activation" => typed::<configuration::Activation>(fixture, schema),
        "ActiveRevision" => typed::<configuration::ActiveRevision>(fixture, schema),
        "ExecutionReceipt" => typed::<execution::ExecutionReceipt>(fixture, schema),
        "ExecutionPage" => typed::<execution::ExecutionPage>(fixture, schema),
        // Offline migration/capacity inputs are not runtime HTTP DTOs.
        "MigrationManifest" | "CapacityProfile" | "DispatchEligibility" => Ok(()),
        _ => anyhow::bail!("missing public contract coverage for {}", fixture.schema),
    }
}
