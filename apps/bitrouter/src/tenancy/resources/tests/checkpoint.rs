use super::*;

// Retired wire input stays raw: there is no current typed checkpoint capability.
fn checkpoint(owner: &str) -> Result<Value> {
    let digest = format!("sha256:{}", "a".repeat(64));
    Ok(json!({
        "reference": {"owner_kind":"tenant", "owner_id":owner,
            "resource_kind":"checkpoint", "resource_id":"learned", "revision_id":"v1"},
        "definition": {"kind":"checkpoint", "evidence": {
            "profile":"learned_checkpoint_v1", "source_configuration":"source-r1",
            "source_policy":reference(owner, ResourceKind::Policy, "parent")?,
            "predecessor":null, "creator":"original-author",
            "experiment_digest":digest, "input_root":digest,
            "source_fence_digest":digest, "effective_head_digest":digest,
            "algorithm":"history_optimizer_v1", "units":0,
            "conclusive":0, "unavailable":0
        }}
    }))
}

#[test]
fn retired_learned_checkpoint_rejects_old_valid_evidence_and_mutations() -> Result<()> {
    let base = checkpoint("tenant")?;
    assert!(serde_json::from_value::<manifest::Import>(base.clone()).is_err());
    for (field, value) in [
        ("profile", json!("learned_checkpoint_v99")),
        ("algorithm", json!("history_optimizer_v99")),
        ("source_fence_digest", json!("unqualified-label")),
        ("units", json!(1025)),
        ("conclusive", json!(u32::MAX)),
        ("unavailable", json!(1)),
        (
            "source_policy",
            serde_json::to_value(reference("other", ResourceKind::Policy, "parent")?)?,
        ),
        ("predecessor", base["reference"].clone()),
    ] {
        let mut bad = base.clone();
        bad["definition"]["evidence"][field] = value;
        assert!(serde_json::from_value::<manifest::Import>(bad).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn retired_checkpoint_import_is_atomic_and_preserves_current_policy_closure() -> Result<()> {
    let (db, resources) = fixture().await?;
    let key = rand::random();
    let resources = Resources::with_key(resources.installation, key)?;
    import(
        &resources,
        &db,
        vec![policy("tenant", "parent")?, policy("tenant", "unselected")?],
    )
    .await?;
    let parent = reference("tenant", ResourceKind::Policy, "parent")?;
    let original = document(Vec::new(), Some(parent.clone()))?;
    let compiled = compile(&resources, &db, &original).await?;
    assert_eq!(compiled.compiler_version, MODEL_POLICY_COMPILER_VERSION);
    assert_eq!(compiled.dependencies, vec![parent]);
    let retained = decrypt(&resources, &original, &compiled)?;
    assert_eq!(retained.format, 7);
    retained.validate_format()?;
    assert_eq!(retained.artifacts.len(), 1);
    let stored = validation::stored(original.clone(), compiled)?;
    resources
        .validate_compiled(&stored)
        .map_err(|code| anyhow::anyhow!("current policy validation: {code:?}"))?;
    resources
        .open_execution(&stored)
        .map_err(|code| anyhow::anyhow!("current policy open: {code:?}"))?;
    let references = storage::references(&db).await?;
    let mut successor = policy("tenant", "successor")?;
    successor["definition"]["dependencies"] = json!([checkpoint("tenant")?["reference"]]);
    let mut extra = credential("tenant")?;
    extra["reference"]["resource_id"] = json!("must-not-be-created");
    let rejected = vec![extra, checkpoint("tenant")?, successor];
    for format in 1..=4 {
        let bytes = serde_json::to_vec(&json!({
            "format":format, "installation_id":resources.installation, "resources":rejected
        }))?;
        assert!(resources.provision_bytes(&db, &bytes).await.is_err());
        #[cfg(all(feature = "core", feature = "harness"))]
        {
            let bro = Resources::with_key(resources.installation.clone(), key)?.for_bro_capture();
            assert!(bro.provision_bytes(&db, &bytes).await.is_err());
        }
        assert_eq!(storage::references(&db).await?, references);
    }
    let recompiled = compile(&resources, &db, &original).await?;
    assert_eq!(
        serde_json::to_value(decrypt(&resources, &original, &recompiled)?)?,
        serde_json::to_value(retained)?
    );
    resources.open_execution(&stored).map_err(|code| {
        anyhow::anyhow!("retained policy changed after rejected import: {code:?}")
    })?;
    db.close().await?;
    Ok(())
}

#[test]
fn authenticated_retired_checkpoint_is_rejected_even_with_current_bro_markers() -> Result<()> {
    let key = rand::random();
    let resources = Resources::with_key("installation".to_owned().try_into()?, key)?;
    let parent = reference("tenant", ResourceKind::Policy, "parent")?;
    let document = document(Vec::new(), Some(parent.clone()))?;
    let current = policy("tenant", "parent")?;
    let retired = checkpoint("tenant")?;
    let mut envelope = json!({
        "format":3, "document":document, "routes":[],
        "artifacts":[
            {"reference":current["reference"], "definition":current["definition"], "envelope":null},
            {"reference":retired["reference"], "definition":retired["definition"], "envelope":null}
        ]
    });
    for (format, compiler_version) in [(3, COMPILER_VERSION), (8, BRO_OBLIGATION_COMPILER_VERSION)]
    {
        envelope["format"] = json!(format);
        // Neither old nor current numeric markers can restore the retired decoder.
        assert!(serde_json::from_value::<Envelope>(envelope.clone()).is_err());
        let mut stored = validation::stored(
            document.clone(),
            CompiledConfiguration {
                payload: Value::Null,
                dependencies: vec![parent.clone()],
                compiler_version: compiler_version.into(),
            },
        )?;
        validation::reseal(&resources, &mut stored, &envelope)?;
        assert_eq!(
            resources.validate_compiled(&stored),
            Err(ErrorCode::AuthorityUnavailable)
        );
        assert_eq!(
            resources.validate_executable(&stored),
            Err(ErrorCode::AuthorityUnavailable)
        );
        assert!(matches!(
            resources.open_execution(&stored),
            Err(ErrorCode::AuthorityUnavailable)
        ));
        #[cfg(all(feature = "core", feature = "harness"))]
        {
            let bro = Resources::with_key(resources.installation.clone(), key)?.for_bro_capture();
            assert_eq!(
                bro.validate_compiled(&stored),
                Err(ErrorCode::AuthorityUnavailable)
            );
            assert_eq!(
                bro.validate_executable(&stored),
                Err(ErrorCode::AuthorityUnavailable)
            );
            assert!(matches!(
                bro.open_execution(&stored),
                Err(ErrorCode::AuthorityUnavailable)
            ));
        }
    }
    Ok(())
}
