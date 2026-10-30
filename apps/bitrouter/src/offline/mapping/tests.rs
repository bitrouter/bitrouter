use super::schema::Manifest;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

fn example() -> Result<Value> {
    Ok(serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/contracts/resource-tenancy/migration-example.json"
    )))?)
}
fn valid(value: &Value) -> Result<()> {
    Manifest::decode(&serde_json::to_vec(value)?)?;
    Ok(())
}
fn invalid(value: &Value) -> Result<()> {
    ensure!(Manifest::decode(&serde_json::to_vec(value)?).is_err());
    Ok(())
}
fn approval() -> Value {
    json!({"tenant_id":"tenant-a", "operator_principal_id":"operator.migration", "evidence_digest":"a".repeat(64), "rationale":"Exact namespace merge approved by installation operator"})
}

#[test]
fn frozen_example_preserves_duplicate_names_and_hashes_in_distinct_owners() -> Result<()> {
    let (_, counts) = Manifest::decode(&serde_json::to_vec(&example()?)?)?;
    ensure!(counts.mapping_keys == 3 && counts.mapped_keys == 2 && counts.quarantined_keys == 1);
    ensure!(counts.declared_mapped_rows == 4 && counts.declared_quarantined_rows == 2);
    Ok(())
}
#[test]
fn identical_duplicate_key_is_rejected_and_owner_namespace_is_significant() -> Result<()> {
    let mut value = example()?;
    value["mappings"][1] = value["mappings"][0].clone();
    invalid(&value)?;
    value["mappings"][1]["source_kind"] = json!("shared_user");
    // Same spelling in a second namespace is a real merge, never deduplicated.
    invalid(&value)?;
    value["tenant_merge_approvals"] = json!([approval()]);
    valid(&value)?;
    value["tenant_merge_approvals"] = json!([approval(), approval()]);
    invalid(&value)
}
#[test]
fn foreign_installation_and_unmatched_merge_approval_are_rejected() -> Result<()> {
    let mut value = example()?;
    value["mappings"][0]["source_installation_id"] = json!("elsewhere");
    invalid(&value)?;
    let mut value = example()?;
    value["tenant_merge_approvals"] = json!([approval()]);
    invalid(&value)
}
#[test]
fn unresolved_and_known_quarantine_cannot_grant_access_or_omit_null_fields() -> Result<()> {
    let mut value = example()?;
    value["mappings"][2]["tenant_id"] = json!("tenant-a");
    invalid(&value)?;
    value["mappings"][2]["tenant_id"] = Value::Null;
    value["mappings"][2]["old_owner"] = json!("invented");
    invalid(&value)?;
    value["mappings"][2]["source_kind"] = json!("native_owner");
    valid(&value)?;
    let object = value["mappings"][2]
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("fixture object"))?;
    object.remove("principal_id");
    invalid(&value)
}
#[test]
fn example_duplicates_ranges_and_unsupported_fields_are_not_silently_discarded() -> Result<()> {
    let mut value = example()?;
    let one = value["mappings"][0]["resource_examples"][0].clone();
    value["mappings"][0]["resource_examples"] = json!([one.clone(), one]);
    invalid(&value)?;
    let mut value = example()?;
    value["mappings"][0]["source_count"] = json!(0);
    invalid(&value)?;
    value["mappings"][0]["source_count"] = json!(9_007_199_254_740_992_u64);
    invalid(&value)?;
    let mut value = example()?;
    value["cutover_ready"] = json!(true);
    invalid(&value)
}
#[test]
fn bounds_are_applied_to_original_bytes_sequences_and_declared_aggregate() -> Result<()> {
    ensure!(Manifest::decode(&vec![b' '; 8 * 1024 * 1024 + 1]).is_err());
    let mut value = example()?;
    value["mappings"] = Value::Array((0..4097).map(|_| value["mappings"][0].clone()).collect());
    invalid(&value)?;
    let mut value = example()?;
    value["mappings"][0]["source_count"] = json!(9_007_199_254_740_991_u64);
    invalid(&value)?;
    let bytes = serde_json::to_string(&example()?)?;
    let duplicate = bytes.replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    ensure!(duplicate != bytes);
    ensure!(Manifest::decode(duplicate.as_bytes()).is_err());
    Ok(())
}
#[test]
fn legacy_owner_strings_are_not_forced_into_new_identifier_alphabet() -> Result<()> {
    let mut value = example()?;
    value["mappings"][0]["old_owner"] = json!("legacy owner / ünicode@example.test");
    value["mappings"][0]["resource_examples"][0]["legacy_resource_id"] =
        json!("/historical/path?old=value");
    valid(&value)?;
    value["mappings"][0]["tenant_id"] = json!("/invalid/new/identity");
    invalid(&value)
}

#[cfg(unix)]
#[tokio::test]
async fn real_bundle_mapping_records_claims_without_mutating_source_or_granting_authority()
-> Result<()> {
    use sea_orm::sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir()?;
    let source = root.path().join("legacy.sqlite");
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&source)
            .create_if_missing(true),
    )
    .await?;
    sea_orm::sqlx::query("CREATE TABLE old_records(id TEXT PRIMARY KEY, body TEXT)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO old_records VALUES('old-a','private-payload')")
        .execute(&mut db)
        .await?;
    db.close().await?;
    let original = std::fs::read(&source)?;
    let bundle = root.path().join("bundle");
    let snapshot = super::super::snapshot_inner(&source, &bundle).await?;
    let mut manifest = example()?;
    manifest["source_snapshot_digest"] = json!(snapshot.evidence.snapshot_digest);
    let input = root.path().join("mapping.json");
    std::fs::write(&input, serde_json::to_vec(&manifest)?)?;
    std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o600))?;
    let output = root.path().join("mapping-review");
    let report = super::run(&bundle, &snapshot.evidence_digest, &input, &output).await?;
    ensure!(!report.source_installation_observed && !report.backfill_applied);
    ensure!(!report.mapping_counts_verified && !report.ownership_evidence_verified);
    ensure!(!report.complete && !report.cutover_ready && !report.producer_verified);
    ensure!(std::fs::read(&source)? == original);
    ensure!(std::fs::read(output.join("manifest.json"))? == std::fs::read(&input)?);
    ensure!(!output.join("READY").exists() && output.join("RECORDED").is_file());
    // Even a legal manifest cannot smuggle a different source snapshot.
    manifest["source_snapshot_digest"] = json!("f".repeat(64));
    std::fs::write(&input, serde_json::to_vec(&manifest)?)?;
    let rejected = root.path().join("rejected");
    ensure!(
        super::run(&bundle, &snapshot.evidence_digest, &input, &rejected)
            .await
            .is_err()
    );
    ensure!(!rejected.exists());
    super::super::read_bundle(&bundle, Some(&snapshot.evidence_digest), "test").await?;
    Ok(())
}
