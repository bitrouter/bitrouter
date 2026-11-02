use super::*;
use crate::tenancy::resources::tests::validation::{reseal, stored};
use crate::tenancy::resources::{Envelope, SHARED_HEALTH_COMPILER_VERSION};

pub(super) fn shared(name: &str, rpm: u32) -> Result<Value> {
    Ok(
        json!({"reference":reference(ResourceKind::Policy,name,"v1")?,
        "definition":{"kind":"admission_profile_v2","profile":{"schema_version":2,
            "rate":{"rate_group_id":"shared","member_rpm":rpm},
            "health":{"rules":"availability_v1","failure_threshold":3,
                "transient_cooldown_seconds":10,"persistent_cooldown_seconds":30,"probe_lease_seconds":5},
            "health_group_id":"circuit","fallback":"stop"}}}),
    )
}

pub(super) async fn import_v2(
    resources: &Resources,
    db: &DatabaseConnection,
    rows: Vec<Value>,
) -> Result<()> {
    resources
        .import_bytes(
            db,
            &serde_json::to_vec(
                &json!({"format":2,"installation_id":resources.installation,"resources":rows}),
            )?,
        )
        .await
}

#[tokio::test]
async fn group_rule_conflict_is_order_independent_and_import_is_atomic() -> Result<()> {
    for reverse in [false, true] {
        let key = rand::random();
        let (db, resources) = fixture_with_key(key).await?;
        for field in [
            "failure_threshold",
            "transient_cooldown_seconds",
            "persistent_cooldown_seconds",
            "probe_lease_seconds",
        ] {
            let first = shared("first", 1)?;
            let mut second = shared("second", 3)?;
            let old = second["definition"]["profile"]["health"][field]
                .as_u64()
                .context("rule")?;
            second["definition"]["profile"]["health"][field] = json!(old + 1);
            let mut rows = vec![credential()?, first, second];
            if reverse {
                rows.reverse();
            }
            ensure!(
                import_v2(&resources, &db, rows).await.is_err(),
                "{field} reverse={reverse}"
            );
            ensure!(storage::references(&db).await?.is_empty());
            let authority = crate::tenancy::store::load_existing(&db, chrono::Utc::now()).await?;
            ensure!(authority.resource_lifecycle.len() == 0);
        }
        let mut rows = vec![shared("first", 1)?, shared("second", 3)?];
        if reverse {
            rows.reverse();
        }
        import_v2(&resources, &db, rows.clone()).await?;
        let restarted = Resources::with_key(resources.installation.clone(), key)?;
        restarted.authenticate_retained(&db).await?;
        let observed = state(&restarted, &db).await?;
        ensure!(observed.profiles.len() == 2 && observed.group_rules.len() == 1);
        for row in rows {
            let reference: ResourceRef = serde_json::from_value(row["reference"].clone())?;
            let retained = observed.profile(&reference).context("retained")?;
            ensure!(
                u64::from(View::from(retained).rate().context("rate")?.member_rpm)
                    == row["definition"]["profile"]["rate"]["member_rpm"]
                        .as_u64()
                        .context("rpm")?
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn format_one_never_accepts_shared_profile_and_profile_families_cannot_reinterpret_history()
-> Result<()> {
    let (db, resources) = fixture().await?;
    ensure!(
        import(&resources, &db, vec![shared("profile", 1)?])
            .await
            .is_err()
    );
    ensure!(storage::references(&db).await?.is_empty());
    import_v2(&resources, &db, vec![shared("profile", 1)?]).await?;
    ensure!(
        import(&resources, &db, vec![profile("legacy")?])
            .await
            .is_err()
    );
    ensure!(storage::references(&db).await?.len() == 1);
    let (db, resources) = fixture().await?;
    import(&resources, &db, vec![profile("v1")?]).await?;
    let mut replacement = shared("profile", 1)?;
    replacement["reference"]["revision_id"] = json!("shared");
    ensure!(import_v2(&resources, &db, vec![replacement]).await.is_err());
    ensure!(storage::references(&db).await?.len() == 1);
    Ok(())
}

#[tokio::test]
async fn mixed_profile_closure_uses_authenticated_format_five_and_rejects_downgrade() -> Result<()>
{
    let (db, resources) = fixture().await?;
    let mut grouped_route = route("shared", true)?;
    grouped_route["reference"]["resource_id"] = json!("shared-route");
    for attachment in grouped_route["definition"]["attachments"]
        .as_array_mut()
        .context("attachments")?
    {
        attachment["profile"]["resource_id"] = json!("shared-profile");
    }
    let mut sibling_route = grouped_route.clone();
    sibling_route["reference"]["resource_id"] = json!("sibling-route");
    for attachment in sibling_route["definition"]["attachments"]
        .as_array_mut()
        .context("attachments")?
    {
        attachment["profile"]["resource_id"] = json!("sibling-profile");
    }
    import_v2(
        &resources,
        &db,
        vec![
            credential()?,
            profile("v1")?,
            shared("shared-profile", 1)?,
            shared("sibling-profile", 3)?,
            route("local", true)?,
            grouped_route,
            sibling_route,
        ],
    )
    .await?;
    let document = serde_json::from_value(
        json!({"routes":[reference(ResourceKind::Route,"route","local")?,reference(ResourceKind::Route,"shared-route","shared")?,reference(ResourceKind::Route,"sibling-route","shared")?],"policy":null,
        "generation_defaults":{"max_output_tokens":128,"temperature":0.5},
        "learning":{"mode":"off","max_evaluations":0},
        "limits":{"active_executions":1,"queued_work":1,"learning_jobs":0}}),
    )?;
    let tenant = "tenant".try_into()?;
    let revision = "config-v1".try_into()?;
    let compiled = resources
        .compile(&db, &tenant, &revision, &document)
        .await
        .map_err(|e| anyhow::anyhow!("compile: {e:?}"))?;
    ensure!(
        compiled.compiler_version == SHARED_HEALTH_COMPILER_VERSION
            && compiled.dependencies.len() == 7
    );
    let binding = (
        &resources.installation,
        &tenant,
        &revision,
        compiled.compiler_version.as_str(),
        &document,
        &compiled.dependencies,
    );
    let sealed = serde_json::from_value(compiled.payload["sealed"].clone())?;
    let bytes =
        resources
            .key
            .as_ref()
            .context("key")?
            .open_bound("configuration", &binding, &sealed)?;
    let envelope: Envelope = serde_json::from_slice(&bytes)?;
    ensure!(envelope.format == 5 && envelope.routes.is_empty());
    let row = stored(document, compiled)?;
    resources
        .open_execution(&row)
        .map_err(|e| anyhow::anyhow!("readback: {e:?}"))?;
    let baseline = serde_json::to_value(&envelope)?;
    for case in ["format", "family", "health_rules", "binding"] {
        let mut changed = baseline.clone();
        let index = changed["artifacts"]
            .as_array()
            .context("artifacts")?
            .iter()
            .position(|a| a["reference"]["resource_id"] == "sibling-profile")
            .context("profile")?;
        match case {
            "format" => changed["format"] = json!(4),
            "family" => {
                changed["artifacts"][index]["definition"]["kind"] = json!("admission_profile");
                let fields = changed["artifacts"][index]["definition"]["profile"]
                    .as_object_mut()
                    .context("profile")?;
                fields.insert("schema_version".into(), json!(1));
                fields.remove("health_group_id");
            }
            "health_rules" => {
                changed["artifacts"][index]["definition"]["profile"]["health"]["probe_lease_seconds"] =
                    json!(6)
            }
            "binding" => {
                changed["artifacts"][index]["definition"]["profile"]["health_group_id"] =
                    Value::Null
            }
            _ => anyhow::bail!("case"),
        }
        let mut corrupt = row.clone();
        reseal(&resources, &mut corrupt, &changed)?;
        // Re-sealing removes the crypto failure so this checks closed-shape and
        // cross-member validation, rather than merely corrupt ciphertext.
        if case == "family" {
            // Both families can coexist. A valid new local profile is separately
            // representable; immutable catalog history forbids this replacement.
            let original = state(&resources, &db).await?;
            let artifact: Artifact = serde_json::from_value(changed["artifacts"][index].clone())?;
            let mut history = original.clone();
            ensure!(history.insert(&artifact).is_err());
        } else {
            ensure!(
                resources.open_execution(&corrupt).is_err(),
                "accepted {case}"
            );
        }
    }
    let mut wrong_compiler = row.clone();
    wrong_compiler.revision.compiler_version =
        super::super::super::PROFILED_COMPILER_VERSION.into();
    ensure!(resources.open_execution(&wrong_compiler).is_err());
    Ok(())
}

#[tokio::test]
async fn shared_retained_loader_rejects_missing_heads_and_authenticated_conflicting_history()
-> Result<()> {
    for case in ["missing_head", "conflicting_rules", "changed_family"] {
        let key = rand::random();
        let (db, resources) = fixture_with_key(key).await?;
        let original = shared("profile", 1)?;
        import_v2(&resources, &db, vec![original.clone()]).await?;
        if case == "missing_head" {
            db.execute_unprepared("DELETE FROM tenancy_resource_lifecycle")
                .await?;
        } else {
            let mut invalid = if case == "changed_family" {
                profile("local-replacement")?
            } else {
                shared("conflicting", 3)?
            };
            if case == "conflicting_rules" {
                invalid["definition"]["profile"]["health"]["probe_lease_seconds"] = json!(6);
            }
            let imported: Import = serde_json::from_value(invalid)?;
            let artifact = Artifact {
                reference: imported.reference.clone(),
                definition: imported.definition,
                envelope: None,
            };
            // Seal a well-formed immutable row directly to isolate loader checks
            // from the public writer/import preflight that already refuses it.
            storage::retain(&resources, &db, &artifact).await?;
            let authority = crate::tenancy::store::load_existing(&db, chrono::Utc::now()).await?;
            lifecycle::write(
                &db,
                &resources.installation,
                &lifecycle::Record {
                    reference: artifact.reference,
                    is_head: case == "conflicting_rules",
                    revoked: true,
                    authority_revision: authority.revision,
                },
            )
            .await?;
            db.execute_unprepared("UPDATE tenancy_resource_lifecycle SET revoked=1")
                .await?;
        }
        let restarted = Resources::with_key(resources.installation.clone(), key)?;
        restarted.authenticate_retained(&db).await?;
        ensure!(state(&restarted, &db).await.is_err(), "accepted {case}");
        let references = storage::references(&db).await?;
        ensure!(
            import_v2(&restarted, &db, vec![original]).await.is_err(),
            "repaired {case}"
        );
        ensure!(storage::references(&db).await? == references);
    }
    Ok(())
}
