use super::*;

// Format fixtures do not stand in for accepted learner/assessment evidence.
fn checkpoint(owner: &str) -> Result<Value> {
    let digest = format!("sha256:{}", "a".repeat(64));
    Ok(json!({
        "reference": reference(owner, ResourceKind::Checkpoint, "learned")?,
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
fn learned_checkpoint_rejects_unknown_profile_foreign_edges_and_counter_overflow() -> Result<()> {
    let base = checkpoint("tenant")?;
    serde_json::from_value::<manifest::Import>(base.clone())?.validate()?;
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
        (
            "predecessor",
            serde_json::to_value(reference("tenant", ResourceKind::Checkpoint, "learned")?)?,
        ),
    ] {
        let mut bad = base.clone();
        bad["definition"]["evidence"][field] = value;
        assert!(
            serde_json::from_value::<manifest::Import>(bad)
                .and_then(|value| value.validate().map_err(serde::de::Error::custom))
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn learned_checkpoint_uses_v3_only_when_selected_and_keeps_exact_policy_closure() -> Result<()>
{
    let (db, resources) = fixture().await?;
    let checked = async {
        let checkpoint = checkpoint("tenant")?;
        let checkpoint_ref = reference("tenant", ResourceKind::Checkpoint, "learned")?;
        let mut successor = policy("tenant", "successor")?;
        successor["definition"]["dependencies"] = json!([checkpoint_ref]);
        import(
            &resources,
            &db,
            vec![policy("tenant", "parent")?, checkpoint, successor],
        )
        .await?;
        let original = document(
            Vec::new(),
            Some(reference("tenant", ResourceKind::Policy, "parent")?),
        )?;
        let original_compiled = compile(&resources, &db, &original).await?;
        let old = decrypt(&resources, &original, &original_compiled)?;
        assert_eq!(old.format, 1);
        assert!(serde_json::to_value(&old)?.get("format").is_none());
        let next = document(
            Vec::new(),
            Some(reference("tenant", ResourceKind::Policy, "successor")?),
        )?;
        let next_compiled = resources
            .compile(
                &db,
                &"tenant".to_owned().try_into()?,
                &"config-v1".to_owned().try_into()?,
                &next,
            )
            .await;
        if !cfg!(feature = "core") {
            assert!(matches!(next_compiled, Err(ErrorCode::CapabilityMismatch)));
            return Ok(());
        }
        let next_compiled =
            next_compiled.map_err(|code| anyhow::anyhow!("compile failed: {code:?}"))?;
        assert_eq!(next_compiled.dependencies.len(), 3);
        let mut learned = decrypt(&resources, &next, &next_compiled)?;
        assert_eq!(learned.format, 3);
        learned.validate_format()?;
        assert!(next_compiled.dependencies.contains(&checkpoint_ref));
        for wrong in [1, 2, 4] {
            learned.format = wrong;
            assert!(learned.validate_format().is_err());
        }
        let stored = validation::stored(next, next_compiled)?;
        resources
            .validate_compiled(&stored)
            .map_err(|code| anyhow::anyhow!("retained validation: {code:?}"))?;
        // An unrelated new capability in the catalog cannot change old bytes.
        let recompiled = compile(&resources, &db, &original).await?;
        assert_eq!(
            serde_json::to_value(decrypt(&resources, &original, &recompiled)?)?,
            serde_json::to_value(old)?
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let closed = db.close().await;
    checked?;
    closed?;
    Ok(())
}

#[test]
fn inactive_learned_data_authenticates_even_when_execution_capability_is_absent() -> Result<()> {
    let resources = Resources::with_key("installation".to_owned().try_into()?, [7; 32])?;
    let mut successor = policy("tenant", "successor")?;
    successor["definition"]["dependencies"] =
        json!([reference("tenant", ResourceKind::Checkpoint, "learned")?]);
    let mut artifacts = Vec::new();
    for value in [
        policy("tenant", "parent")?,
        checkpoint("tenant")?,
        successor,
    ] {
        let input: manifest::Import = serde_json::from_value(value)?;
        input.validate()?;
        artifacts.push(Artifact {
            reference: input.reference,
            definition: input.definition,
            envelope: None,
        });
    }
    let document = document(
        Vec::new(),
        Some(reference("tenant", ResourceKind::Policy, "successor")?),
    )?;
    let dependencies = artifacts
        .iter()
        .map(|value| value.reference.clone())
        .collect();
    let envelope = Envelope {
        format: 3,
        document: document.clone(),
        routes: Vec::new(),
        artifacts,
    };
    let mut stored = validation::stored(
        document,
        CompiledConfiguration {
            payload: Value::Null,
            dependencies,
            compiler_version: COMPILER_VERSION.into(),
        },
    )?;
    // Explicit retained-format fixture, not a learner publication shortcut.
    validation::reseal(&resources, &mut stored, &serde_json::to_value(envelope)?)?;
    resources
        .validate_compiled(&stored)
        .map_err(|code| anyhow::anyhow!("inactive authentication: {code:?}"))?;
    assert_eq!(
        resources.validate_executable(&stored).is_ok(),
        cfg!(feature = "core")
    );
    assert_eq!(
        resources.open_execution(&stored).is_ok(),
        cfg!(feature = "core")
    );
    Ok(())
}
