use super::*;
use crate::tenancy::configuration_store::StoredConfiguration;
use bitrouter_ai::tenancy::configuration::ConfigurationRevision;
use sha2::{Digest, Sha256};

pub(super) fn stored(
    document: Configuration,
    compiled: CompiledConfiguration,
) -> Result<StoredConfiguration> {
    let mut canonical = serde_json::to_value(&document)?;
    crate::tenancy::command::sort(&mut canonical);
    let content_digest = hex::encode(Sha256::digest(serde_json::to_vec(&canonical)?)).try_into()?;
    Ok(StoredConfiguration {
        revision: ConfigurationRevision {
            tenant_id: "tenant".to_owned().try_into()?,
            revision_id: "config-v1".to_owned().try_into()?,
            schema_version: 1,
            compiler_version: compiled.compiler_version,
            content_digest,
            document,
        },
        compiled: compiled.payload,
        dependencies: compiled.dependencies,
        creator: "principal".to_owned().try_into()?,
        authority_revision: 1_u64.try_into()?,
        created_at: "2026-01-01T00:00:00Z".to_owned().try_into()?,
    })
}

pub(super) fn reseal(
    resources: &Resources,
    row: &mut StoredConfiguration,
    envelope: &Value,
) -> Result<()> {
    let revision = &row.revision;
    let sealed = resources
        .key
        .as_ref()
        .context("missing fixture key")?
        .seal_bound(
            "configuration",
            &(
                &resources.installation,
                &revision.tenant_id,
                &revision.revision_id,
                COMPILER_VERSION,
                &revision.document,
                &row.dependencies,
            ),
            &serde_json::to_vec(envelope)?,
        )?;
    row.compiled = json!({"kind":"sealed", "sealed":sealed});
    Ok(())
}

#[tokio::test]
async fn retained_validation_authenticates_all_binding_fields_and_ciphertext() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(
        &resources,
        &db,
        vec![credential("tenant")?, route("tenant", "visible", "model")?],
    )
    .await?;
    let doc = document(
        vec![reference("tenant", ResourceKind::Route, "visible")?],
        None,
    )?;
    let compiled = compile(&resources, &db, &doc).await?;
    let original = stored(doc, compiled)?;
    resources
        .validate_compiled(&original)
        .map_err(|c| anyhow::anyhow!("valid payload rejected: {c:?}"))?;
    let mut changes = Vec::new();
    let mut changed = original.clone();
    changed.revision.content_digest = "0".repeat(64).try_into()?;
    changes.push(changed);
    let mut changed = original.clone();
    changed.revision.schema_version = 2;
    changes.push(changed);
    let mut changed = original.clone();
    changed.revision.compiler_version = "unknown-compiler".into();
    changes.push(changed);
    let mut changed = original.clone();
    changed.revision.revision_id = "another-revision".to_owned().try_into()?;
    changes.push(changed);
    let mut changed = original.clone();
    changed.revision.tenant_id = "another-tenant".to_owned().try_into()?;
    changes.push(changed);
    let mut changed = original.clone();
    changed.revision.document.generation_defaults.temperature = 1.0;
    changes.push(changed);
    let mut changed = original.clone();
    changed.dependencies.pop();
    changes.push(changed);
    let mut changed = original.clone();
    changed.compiled["sealed"]["ciphertext"] = json!("AAAA");
    changes.push(changed);
    for changed in changes {
        assert!(matches!(
            resources.validate_compiled(&changed),
            Err(ErrorCode::AuthorityUnavailable)
        ));
    }
    let other_installation =
        Resources::with_key("other-installation".to_owned().try_into()?, [7; 32])?;
    assert!(matches!(
        other_installation.validate_compiled(&original),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    let wrong_key = Resources::with_key(resources.installation.clone(), [8; 32])?;
    assert!(matches!(
        wrong_key.validate_compiled(&original),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    // Retained validation remains independent of the mutable resource catalog.
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DELETE FROM tenancy_resource_revisions".to_owned(),
    ))
    .await?;
    assert!(resources.validate_compiled(&original).is_ok());
    Ok(())
}

#[tokio::test]
async fn retained_validation_rejects_authenticated_invalid_envelope_shapes() -> Result<()> {
    let (db, resources) = fixture().await?;
    import(
        &resources,
        &db,
        vec![credential("tenant")?, route("tenant", "visible", "model")?],
    )
    .await?;
    let doc = document(
        vec![reference("tenant", ResourceKind::Route, "visible")?],
        None,
    )?;
    let compiled = compile(&resources, &db, &doc).await?;
    let envelope = serde_json::to_value(decrypt(&resources, &doc, &compiled)?)?;
    let original = stored(doc, compiled)?;
    let mut changes = Vec::new();
    let mut changed = envelope.clone();
    changed["document"]["generation_defaults"]["temperature"] = json!(1.5);
    changes.push(changed);
    let mut changed = envelope.clone();
    changed["routes"][0]["reference"]["owner_id"] = json!("foreign");
    changes.push(changed);
    let mut changed = envelope.clone();
    changed["routes"][0]["chains"][0]["targets"][0]["credential"]["revision_id"] =
        json!("unapproved");
    changes.push(changed);
    let mut changed = envelope.clone();
    changed["routes"][0]["chains"][0]["targets"][0]["target"]["headers"] = json!(false);
    changes.push(changed);
    let mut changed = envelope.clone();
    // Authenticate the same forbidden URL in both retained copies, so this
    // fails URL validation rather than merely the composition equality check.
    for path in ["routes", "artifacts"] {
        let route = if path == "routes" {
            &mut changed["routes"][0]
        } else {
            &mut changed["artifacts"][0]["envelope"]
        };
        route["chains"][0]["targets"][0]["target"]["api_base"] =
            json!("https://169.254.169.254/latest/meta-data");
    }
    changes.push(changed);
    let mut changed = envelope.clone();
    changed["artifacts"] = json!([]);
    changes.push(changed);
    let mut changed = envelope;
    changed["routes"] = json!([]);
    changes.push(changed);
    for envelope in changes {
        let mut changed = original.clone();
        reseal(&resources, &mut changed, &envelope)?;
        assert!(matches!(
            resources.validate_compiled(&changed),
            Err(ErrorCode::AuthorityUnavailable)
        ));
    }
    Ok(())
}

#[tokio::test]
async fn empty_retained_marker_requires_exact_binding_without_a_key() -> Result<()> {
    let (db, keyed) = fixture().await?;
    let resources = Resources {
        installation: keyed.installation,
        key: None,
        approved_origins: Vec::new(),
    };
    let doc = document(Vec::new(), None)?;
    let original = stored(doc.clone(), compile(&resources, &db, &doc).await?)?;
    assert!(resources.validate_compiled(&original).is_ok());
    let mut changed = original.clone();
    changed.revision.revision_id = "different".to_owned().try_into()?;
    assert!(matches!(
        resources.validate_compiled(&changed),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    let mut changed = original.clone();
    changed.compiled["unexpected"] = json!(true);
    assert!(matches!(
        resources.validate_compiled(&changed),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    let mut changed = original;
    changed
        .dependencies
        .push(reference("tenant", ResourceKind::Credential, "extra")?);
    assert!(matches!(
        resources.validate_compiled(&changed),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    Ok(())
}
