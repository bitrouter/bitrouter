use super::*;
use crate::gateway::providers::auth::credentials::OAuthToken;
use crate::tenancy::oauth::{crypto::Crypto, storage as oauth};
use sea_orm::TransactionTrait;

async fn rotate_last_slot(
    path: &Path,
    installation: &InstallationId,
    wrong_key: bool,
) -> Result<()> {
    let owned = crate::db::connect_owned_tenancy(&format!("sqlite://{}", path.display())).await?;
    let db = owned.database().clone();
    let result = async {
        let tx = db.begin().await?;
        let row = tx
            .query_one(Statement::from_string(
                tx.get_database_backend(),
                "SELECT metadata_json FROM tenancy_oauth_slots ORDER BY slot_key DESC LIMIT 1",
            ))
            .await?
            .context("last OAuth slot")?;
        let metadata: oauth::Metadata =
            serde_json::from_str(&row.try_get::<String>("", "metadata_json")?)?;
        let old = oauth::load(&tx, installation, &metadata.reference)
            .await?
            .context("retained OAuth")?;
        let actual = crate::runtime_crypto::DeploymentKey::from_environment(producer::KEY_ENV)?;
        let key = if wrong_key {
            let first = crate::runtime_crypto::DeploymentKey::from_bytes([19; 32])?;
            if first.id() != actual.id() {
                first
            } else {
                crate::runtime_crypto::DeploymentKey::from_bytes([20; 32])?
            }
        } else {
            actual
        };
        let mut metadata = old.metadata.clone();
        if !wrong_key {
            metadata.generation += 1;
        }
        let replacement = oauth::Row::new(
            &Crypto::new(installation.clone(), key),
            metadata,
            &OAuthToken {
                access_token: "retained-rotated-access".into(),
                refresh_token: Some("retained-rotated-refresh".into()),
                expires_at: 9999999999,
            },
        )?;
        // Use the original encrypted row constructor/CAS writer; the positive
        // fixture is a storage rotation, not a claim that a refresh HTTP ran.
        oauth::replace(&tx, &old, &replacement).await?;
        tx.commit().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = db.close().await;
    drop(owned);
    result?;
    closed?;
    Ok(())
}

async fn retained(path: &Path, installation: &InstallationId) -> Result<()> {
    let cancel = CancellationToken::new();
    let workspace = Workspace::create(
        &options(path.parent().context("fixture parent")?),
        cancel.clone(),
    )?;
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let mut connections = crate::offline::engine::Connections::new(cancel, Some(owner));
    connections.validation_budget().await?;
    connections
        .open_immutable_source_budgeted(path, &workspace)
        .await?;
    let result = async {
        let reader =
            ReadConnection::new(connections.source.as_mut().context("fixture connection")?)
                .with_workspace(&workspace);
        let row = reader
            .query_one(Statement::from_string(
                reader.get_database_backend(),
                "SELECT COUNT(*) AS count FROM tenancy_resource_revisions WHERE resource_id='slot'",
            ))
            .await?
            .context("resource history count")?;
        assert_eq!(row.try_get::<i64>("", "count")?, 2);
        let row = reader
            .query_one(Statement::from_string(
                reader.get_database_backend(),
                "SELECT COUNT(*) AS count FROM tenancy_oauth_slots",
            ))
            .await?
            .context("OAuth history count")?;
        assert_eq!(row.try_get::<i64>("", "count")?, 33);
        let row = reader
            .query_one(Statement::from_string(
                reader.get_database_backend(),
                "SELECT metadata_json FROM tenancy_oauth_slots ORDER BY slot_key DESC LIMIT 1",
            ))
            .await?
            .context("last OAuth slot")?;
        let metadata: oauth::Metadata =
            serde_json::from_str(&row.try_get::<String>("", "metadata_json")?)?;
        assert_eq!(metadata.generation, 1);
        assert!(matches!(metadata.state, oauth::State::Ready));
        let row = oauth::load(&reader, installation, &metadata.reference)
            .await?
            .context("retained OAuth row")?;
        let token = row.token(&Crypto::new(
            installation.clone(),
            crate::runtime_crypto::DeploymentKey::from_environment(producer::KEY_ENV)?,
        ))?;
        assert_eq!(token.access_token, "retained-rotated-access");
        assert_eq!(
            token.refresh_token.as_deref(),
            Some("retained-rotated-refresh")
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = connections.close().await;
    workspace.finish()?;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit BITROUTER_TEST_RECOVERY_KEY; real retained resources/OAuth v2 restore"]
async fn real_restore_keeps_old_resource_revisions_and_rotated_oauth_and_refuses_corruption()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(180),async {
        crate::runtime_crypto::DeploymentKey::from_environment(producer::KEY_ENV)?;
        let directory = home()?;
        let home = directory.path().canonicalize()?;
        let source = home.join("historical.sqlite");
        let live = producer::Live::open(&source,None,Some(producer::KEY_ENV)).await?;
        let result = producer::provision(&live).await;
        let closed = live.close(&source).await;
        let provision = result?; closed?;
        let mut manifest = producer::manifest(&provision.installation);
        let resources = manifest["resources"].as_array_mut().context("resources")?;
        let mut newer = resources[0].clone();
        newer["reference"]["revision_id"] = json!("v2");
        newer["definition"]["credential"]["data"]["value"] = json!("newer-static-secret");
        resources.push(newer);
        let registration = crate::gateway::providers::login::find("anthropic").context("OAuth registration")?;
        // Cross the real 32-reference/slot page boundary. All rows are imported
        // by Service with the existing authenticated immutable-resource writer.
        for index in 0..33 {
            resources.push(json!({"reference":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential",
                "resource_id":format!("oauth-{index:02}"),"revision_id":"v1"},
                "definition":{"kind":"credential","credential":{"type":"oauth","data":{"access_token":format!("seed-{index}"),
                    "refresh_token":format!("refresh-{index}"),"expires_at":9999999999_i64}},"oauth_generation":0,
                    "oauth":{"format":1,"mechanism":"claude_code_refresh_v1","provider":"claude-code","account":format!("account-{index}"),
                        "registration":"anthropic","token_endpoint":registration.auth.token_endpoint,"client_id":registration.auth.client_id}}}));
        }
        let import = home.join("historical-import.json");
        producer::private_json(&import,&manifest)?;
        let live = producer::Live::open(&source,Some(&import),Some(producer::KEY_ENV)).await?;
        live.close(&source).await?;
        rotate_last_slot(&source,&provision.installation,false).await?;
        retained(&source,&provision.installation).await?;
        let original = std::fs::read(&source)?;
        let bundle = home.join("historical-bundle");
        let evidence = snapshot(&source,&bundle,&home).await?;
        let anchor = evidence["evidence_digest"].as_str().context("evidence anchor")?;
        let restored = home.join("restored.sqlite");
        let report = crate::offline::restore_with_options(&bundle,anchor,&restored,provision.installation.as_str(),
            producer::KEY_ENV,32,options(&home)).await?;
        assert_eq!(serde_json::to_value(&report as &dyn crate::output::CliReport)?["action"],"restore");
        unchanged(&source,&original)?; unchanged(&restored,&original)?;
        retained(&restored,&provision.installation).await?;
        unchanged(&restored,&original)?;

        for (name, oauth) in [("bad-old-resource",false),("bad-last-oauth",true)] {
            let damaged = home.join(format!("{name}.sqlite"));
            std::fs::copy(&source,&damaged)?;
            if oauth { rotate_last_slot(&damaged,&provision.installation,true).await?; }
            else {
                stopped_update(&damaged,"UPDATE tenancy_resource_revisions SET payload_json=? WHERE resource_id='slot' AND revision_id='v1'","{}").await?;
            }
            let before = std::fs::read(&damaged)?;
            let bundle = home.join(format!("{name}-bundle"));
            let evidence = snapshot(&damaged,&bundle,&home).await?;
            let destination = home.join(format!("{name}-restored.sqlite"));
            assert!(crate::offline::restore_with_options(&bundle,evidence["evidence_digest"].as_str().context("bad evidence anchor")?,
                &destination,provision.installation.as_str(),producer::KEY_ENV,32,options(&home)).await.is_err());
            assert!(!destination.exists(),"failed retained proof published a destination");
            unchanged(&damaged,&before)?;
            assert!(bundle.join("READY").exists());
        }
        unchanged(&source,&original)?;
        Ok::<_,anyhow::Error>(())
    }).await.context("historical restore regression watchdog")?
}
