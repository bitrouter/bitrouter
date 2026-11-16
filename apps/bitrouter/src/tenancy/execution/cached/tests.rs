use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::configuration::ConfigurationRevision;
use serde_json::json;

use super::{
    ExecutionKey,
    configuration_store::{StoredConfiguration, execution_cache_binding},
};

fn stored() -> Result<StoredConfiguration> {
    Ok(StoredConfiguration {
        revision: serde_json::from_value::<ConfigurationRevision>(json!({
            "tenant_id":"tenant", "revision_id":"revision", "schema_version":1,
            "compiler_version":"compiler", "content_digest":"a".repeat(64),
            "document": {"routes":[], "policy":null,
                "generation_defaults":{"max_output_tokens":25,"temperature":0.5},
                "learning":{"mode":"off","max_evaluations":0},
                "limits":{"active_executions":1,"queued_work":0,"learning_jobs":0}}
        }))?,
        compiled: json!({"kind":"sealed", "sealed":{
            "format":1, "key_id":"key", "nonce":"nonce", "ciphertext":"ciphertext"}}),
        dependencies: Vec::new(),
        creator: "creator".try_into()?,
        authority_revision: 1_u64.try_into()?,
        created_at: "2026-11-16T00:00:00Z".try_into()?,
    })
}

#[test]
fn control_keys_compare_all_stored_facts_and_separate_dispatch_and_authority() -> Result<()> {
    let original = stored()?;
    let binding = execution_cache_binding(&original)?;
    ensure!(binding == execution_cache_binding(&original.clone())?);
    let changes: &[fn(&mut StoredConfiguration)] = &[
        |row| row.compiled["sealed"]["ciphertext"] = json!("changed"),
        |row| row.compiled["sealed"]["nonce"] = json!("changed"),
        |row| row.compiled["sealed"]["key_id"] = json!("changed"),
        |row| row.compiled["sealed"]["format"] = json!(2),
        |row| row.revision.compiler_version.push('x'),
        |row| row.revision.schema_version += 1,
        |row| row.revision.document.generation_defaults.max_output_tokens += 1,
    ];
    for (index, change) in changes.iter().enumerate() {
        let mut changed = original.clone();
        change(&mut changed);
        ensure!(
            binding != execution_cache_binding(&changed)?,
            "unbound stored field {index}"
        );
    }
    let mut dependency = original.clone();
    dependency.dependencies.push(serde_json::from_value(json!({
        "owner_kind":"tenant", "owner_id":"tenant", "resource_kind":"route",
        "resource_id":"route", "revision_id":"revision"}))?);
    ensure!(binding != execution_cache_binding(&dependency)?);
    let mut metadata = original.clone();
    metadata.creator = "other-creator".try_into()?;
    ensure!(binding != execution_cache_binding(&metadata)?);
    metadata = original.clone();
    metadata.created_at = "2026-11-16T00:00:01Z".try_into()?;
    ensure!(binding != execution_cache_binding(&metadata)?);
    metadata = original.clone();
    metadata.authority_revision = 2_u64.try_into()?;
    ensure!(binding != execution_cache_binding(&metadata)?);
    let control = ExecutionKey::Control {
        authority_revision: 1,
        binding: binding.clone(),
    };
    ensure!(
        control
            != ExecutionKey::Control {
                authority_revision: 2,
                binding
            }
    );
    ensure!(
        control
            != ExecutionKey::Dispatch {
                authority_revision: 1,
                tenant: "tenant".into(),
                revision: "revision".into()
            }
    );
    Ok(())
}
