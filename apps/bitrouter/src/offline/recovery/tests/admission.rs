//! Normal Service/SDK producers, then the original stopped offline readers.
use super::*;
use sea_orm::sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

async fn catalogs(
    db: &mut SqliteConnection,
    target: &plan::Target,
    digest_key: &[u8; 32],
) -> Result<()> {
    let measured = inventory::inspect_in(db, digest_key).await?;
    catalog::fresh(db, &measured, target).await?;
    crate::tenancy::audit::recovery::fresh_catalog_v2_for_test(
        db,
        &measured,
        &target.installation_id,
    )
    .await
}

fn policy(installation: &bitrouter_ai::tenancy::identity::InstallationId) -> Result<Value> {
    let document = json!({"spec_version":bitrouter_ai::policy::SPEC_VERSION,"policies":{
        "selected":{"tiers":{"economy":{"model":"fixture:model-a"}},"default_tier":"economy"}
    }});
    let mut manifest = fixture::manifest(installation);
    manifest["resources"].as_array_mut().context("resources")?.push(json!({
        "reference":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"policy","resource_id":"selected","revision_id":"v1"},
        "definition":{"kind":"policy","name":"selected","document":document,"dependencies":[]}
    }));
    Ok(manifest)
}

#[tokio::test]
#[ignore = "requires explicit BITROUTER_TEST_RECOVERY_KEY; real Service/SDK and v1/v2 readers, no bro binary"]
async fn admission_metadata_preserves_fresh_service_sdk_policy_and_target() -> Result<()> {
    crate::runtime_crypto::DeploymentKey::from_environment(fixture::KEY_ENV)?;
    let digest_key = rand::random();
    let fixture = Fixture::new().await?;
    let manifest = policy(&fixture.target_provision.installation)?;
    let import = fixture.directory.path().join("admission-policy.json");
    let approval = fixture::private_json(&import, &manifest)?;
    let policy_ref: bitrouter_ai::tenancy::configuration::ResourceRef =
        serde_json::from_value(manifest["resources"][2]["reference"].clone())?;
    let live = fixture::Live::open(&fixture.target, Some(&import), Some(fixture::KEY_ENV)).await?;
    let client =
        bitrouter_sdk::tenancy::TenancyClient::new(&live.base, &fixture.target_provision.secret)?;
    let tenant = "tenant".try_into()?;
    let previous = client
        .active_revision(&fixture.target_provision.installation, &tenant)
        .await?;
    let mut document: bitrouter_ai::tenancy::configuration::Configuration =
        serde_json::from_value(fixture.provisioning["configurations"][0]["document"].clone())?;
    document.policy = Some(policy_ref.clone());
    let draft = client
        .put_configuration(
            &tenant,
            &bitrouter_ai::tenancy::configuration::ConfigurationPut {
                installation_id: fixture.target_provision.installation.clone(),
                operation_id: "policy-draft".try_into()?,
                expected_authority_revision: previous.authority_revision,
                document,
            },
        )
        .await?;
    let draft_result = client
        .operation(
            &fixture.target_provision.installation,
            &"policy-draft".try_into()?,
        )
        .await?;
    let active = client
        .activate(
            &tenant,
            &bitrouter_ai::tenancy::configuration::Activation {
                installation_id: fixture.target_provision.installation.clone(),
                operation_id: "policy-activate".try_into()?,
                expected_authority_revision: draft_result
                    .authority_revision
                    .context("draft authority")?,
                expected_active_revision: previous.revision_id,
                revision_id: draft.revision_id.clone(),
            },
        )
        .await?;
    let active_result = client
        .operation(
            &fixture.target_provision.installation,
            &"policy-activate".try_into()?,
        )
        .await?;
    let current = client
        .active_revision(&fixture.target_provision.installation, &tenant)
        .await?;
    ensure!(
        current.revision_id.as_ref() == Some(&draft.revision_id),
        "active configuration differs from draft"
    );
    live.close(&fixture.target).await?;
    let mut provisioning = fixture.provisioning.clone();
    provisioning["resources"]
        .as_array_mut()
        .context("resources")?
        .push(serde_json::to_value(&policy_ref)?);
    provisioning["configurations"]
        .as_array_mut()
        .context("configurations")?
        .push(serde_json::to_value(draft)?);
    provisioning["active"] = json!([{"tenant":tenant,"revision":current.revision_id,"authority_revision":active.authority_revision}]);
    provisioning["operations"]
        .as_array_mut()
        .context("operations")?
        .extend([
            json!({"principal_id":"owner","result":draft_result}),
            json!({"principal_id":"owner","result":active_result}),
        ]);
    let mut plan = fixture.plan.clone();
    plan["target"]["static_manifest"] = approval;
    plan["target"]["provisioning"] = fixture::private_json(
        &fixture.directory.path().join("admission-provisioning.json"),
        &provisioning,
    )?;
    let plan_path = fixture.directory.path().join("admission-plan.json");
    fixture::private_json(&plan_path, &plan)?;
    let target: plan::Target = serde_json::from_value(plan["target"].clone())?;
    {
        let before = stopped_bytes(&fixture.target)?;
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&fixture.target)
                .read_only(true)
                .immutable(true),
        )
        .await?;
        let witnesses: i64 =
            sea_orm::sqlx::query_scalar("SELECT COUNT(*) FROM tenancy_admission_index")
                .fetch_one(&mut db)
                .await?;
        ensure!(witnesses == 1);
        catalogs(&mut db, &target, &digest_key).await?;
        db.close().await?;
        let output = fixture.directory.path().join("admission-current");
        let report = crate::offline::recover_with_budget(
            &plan_path,
            &output,
            crate::offline::ValidationBudget::default(),
        )
        .await?;
        ensure!(
            serde_json::to_value(&report as &dyn crate::output::CliReport)?["fresh_provisioning_verified"]
                == true
        );
        ensure!(
            stopped_bytes(&fixture.target)? == before,
            "fresh verifier changed target bytes"
        );
    }
    let mut db =
        SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&fixture.target))
            .await?;
    for mutation in [
        "DELETE FROM tenancy_admission_index",
        "UPDATE tenancy_admission_index SET index_kind='health'",
        "UPDATE tenancy_admission_index SET reference_json='{}'",
        "extra operational table",
    ] {
        sea_orm::sqlx::query("BEGIN").execute(&mut db).await?;
        if mutation == "extra operational table" {
            sea_orm::sqlx::query("CREATE TABLE unexpected_admission_work(work INTEGER NOT NULL)")
                .execute(&mut db)
                .await?;
            sea_orm::sqlx::query("INSERT INTO unexpected_admission_work VALUES(1)")
                .execute(&mut db)
                .await?;
        } else {
            sea_orm::sqlx::query(mutation).execute(&mut db).await?;
        }
        let measured = inventory::inspect_in(&mut db, &digest_key).await?;
        ensure!(
            catalog::fresh(&mut db, &measured, &target).await.is_err(),
            "v1 accepted {mutation}"
        );
        ensure!(
            crate::tenancy::audit::recovery::fresh_catalog_v2_for_test(
                &mut db,
                &measured,
                &target.installation_id,
            )
            .await
            .is_err(),
            "v2 accepted {mutation}"
        );
        sea_orm::sqlx::query("ROLLBACK").execute(&mut db).await?;
    }
    db.close().await?;
    // Unkeyed authority provisioning has no resource witnesses.
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&fixture.source)
            .read_only(true)
            .immutable(true),
    )
    .await?;
    crate::tenancy::audit::recovery::fresh_admission_metadata(
        &mut db,
        &fixture.source_provision.installation,
        false,
    )
    .await?;
    db.close().await?;
    Ok(())
}
