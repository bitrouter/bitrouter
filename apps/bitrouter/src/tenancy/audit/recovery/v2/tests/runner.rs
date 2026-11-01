use super::*;

struct Fixture {
    _home: tempfile::TempDir,
    home: PathBuf,
    target: PathBuf,
    source: PathBuf,
    bundle: PathBuf,
    plan: Value,
    approval: Vec<Value>,
    old_secret: String,
}

impl Fixture {
    async fn new() -> Result<Self> {
        crate::runtime_crypto::DeploymentKey::from_environment(producer::KEY_ENV)?;
        let directory = home()?;
        let home = directory.path().canonicalize()?;
        let source = home.join("source.sqlite");
        let source_live = producer::Live::open(&source, None, None).await?;
        let source_result = async {
            let issued = producer::provision(&source_live).await?;
            let operator = source_live.operator()?;
            let original = &issued.operations[3]["result"];
            let credential = original["resource_id"]
                .as_str()
                .context("issued credential ID")?
                .try_into()?;
            let revision = original["resource_revision"]
                .as_str()
                .context("issued credential revision")?
                .try_into()?;
            operator
                .revoke_credential(
                    &"tenant".try_into()?,
                    &credential,
                    &bitrouter_ai::tenancy::operation::Mutation {
                        operation_id: "retire-old-key".try_into()?,
                        expected_revision: revision,
                        installation_id: issued.installation.clone(),
                    },
                )
                .await?;
            let revoked = operator
                .credential(&issued.installation, &"tenant".try_into()?, &credential)
                .await?;
            assert_eq!(serde_json::to_value(revoked)?["state"], "revoked");
            Ok::<_, anyhow::Error>(issued)
        }
        .await;
        let closed = source_live.close(&source).await;
        let source_provision = source_result?;
        closed?;

        let target = home.join("target.sqlite");
        let live = producer::Live::open(&target, None, Some(producer::KEY_ENV)).await?;
        let result = producer::provision(&live).await;
        let closed = live.close(&target).await;
        let mut target_provision = result?;
        closed?;
        let manifest = producer::manifest(&target_provision.installation);
        let import = home.join("import.json");
        producer::private_json(&import, &manifest)?;
        let live = producer::Live::open(&target, Some(&import), Some(producer::KEY_ENV)).await?;
        let configured = producer::configure(&live, &mut target_provision, &source_provision).await;
        let closed = live.close(&target).await;
        let observed = configured?;
        closed?;
        let mut records = vec![
            json!({"format":format::PROVISIONING,"installation_id":target_provision.installation,
            "operation_count":target_provision.operations.len(),"configuration_count":1,"active_count":1,"resource_count":2}),
        ];
        for operation in &target_provision.operations {
            records.push(json!({"kind":"operation","principal_id":operation["principal_id"],"result":operation["result"]}));
        }
        records.push(json!({"kind":"configuration","revision":observed["configurations"][0]}));
        records.push(json!({"kind":"active","tenant_id":observed["active"][0]["tenant"],
            "revision_id":observed["active"][0]["revision"],"authority_revision":observed["active"][0]["authority_revision"]}));
        let mut approval = vec![
            json!({"format":format::APPROVAL,"installation_id":target_provision.installation,"resource_count":2}),
        ];
        for resource in manifest["resources"]
            .as_array()
            .context("manifest resources")?
        {
            records.push(json!({"kind":"resource","reference":resource["reference"]}));
            approval.push(json!({"kind":"resource","reference":resource["reference"],"definition":resource["definition"]}));
        }
        let provisioning = json_lines(&home.join("provisioning.jsonl"), &records)?;
        let static_manifest = json_lines(&home.join("approval.jsonl"), &approval)?;
        let bundle = home.join("bundle");
        let evidence = snapshot(&source, &bundle, &home).await?;
        let commit = crate::artifact::current()
            .source_revision
            .context("test producer revision")?;
        let artifact_digest = {
            use sha2::{Digest, Sha256};
            use std::io::Read;
            let mut executable = std::fs::File::open(std::env::current_exe()?)?;
            let mut digest = Sha256::new();
            let mut buffer = [0; 65536];
            loop {
                let count = executable.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            hex::encode(digest.finalize())
        };
        let producer = producer::private_json(
            &home.join("producer.json"),
            &json!({
            "format":"bitrouter.offline.source-producer-attestation.v1", "installation_id":source_provision.installation,
            "commit":commit,"artifact_sha256":artifact_digest,"database_evidence_digest":evidence["evidence_digest"]}),
        )?;
        let retirement = json_lines(
            &home.join("retirement.jsonl"),
            &[
                json!({"format":format::RETIREMENT,"source_installation_id":source_provision.installation,"database_evidence_digest":evidence["evidence_digest"],
                "fence_maintained":true,"target_ingress_fenced":true,"selection_exhaustive":true,"unresolved_effects_remain_unknown":true,
                "writer_count":1,"selected_file_count":0}),
                json!({"kind":"writer","id":"fixture-source","process_identity":format!("Service:{}",std::process::id()),"restart_source":"none","retired":true}),
            ],
        )?;
        let tables = evidence["evidence"]["inventory"]["tables"].as_array().context("inventory tables")?.iter().map(|t|
            json!({"table":t["name"],"rows":t["rows"],"value_bytes":t["value_bytes"],"digest":t["digest"],"disposition":"retained_archive"})).collect::<Vec<_>>();
        let plan = json!({"format":format::PLAN,
            "source":{"bundle":bundle,"evidence_digest":evidence["evidence_digest"],"installation_id":source_provision.installation,
                "producer":{"commit":commit,"artifact_sha256":artifact_digest,"evidence":producer},"tables":tables},
            "target":{"database":target,"installation_id":target_provision.installation,"provisioning":provisioning,
                "wrapping_key_env":producer::KEY_ENV,"static_manifest":static_manifest},"retirement":retirement});
        Ok(Self {
            _home: directory,
            home,
            target,
            source,
            bundle,
            plan,
            approval,
            old_secret: source_provision.secret,
        })
    }
    async fn invoke(&self, plan: &Value, name: &str) -> Result<Value> {
        let path = self.home.join(format!("{name}-plan.json"));
        producer::private_json(&path, plan)?;
        let output = self.home.join(name);
        let report = crate::offline::recovery::run_with_options(
            &path,
            &output,
            ValidationBudget::default(),
            options(&self.home),
        )
        .await?;
        Ok(serde_json::to_value(
            &report as &dyn crate::output::CliReport,
        )?)
    }
    async fn rejected(&self, plan: &Value, name: &str, reason: &str) -> Result<()> {
        let before = std::fs::read(&self.target)?;
        let archive = std::fs::read(self.bundle.join("snapshot.sqlite"))?;
        let error = self
            .invoke(plan, name)
            .await
            .err()
            .context("negative proof unexpectedly succeeded")?;
        ensure!(
            format!("{error:#}").contains(reason),
            "wrong rejection: {error:#}"
        );
        let report = self.home.join(name);
        assert!(report.join("INCOMPLETE").exists());
        assert!(!report.join("VERIFIED").exists() && !report.join("report.json").exists());
        unchanged(&self.target, &before)?;
        assert_eq!(std::fs::read(self.bundle.join("snapshot.sqlite"))?, archive);
        drop(crate::tenancy::ownership::acquire_path(
            &self.target,
            false,
        )?);
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires explicit BITROUTER_TEST_RECOVERY_KEY; real Service/SDK v2 recovery, no bro binary"]
async fn real_v2_runner_proves_closure_and_refuses_changed_approval_and_revoked_source_key()
-> Result<()> {
    tokio::time::timeout(Duration::from_secs(180), async {
        let fixture = Fixture::new().await?;
        let target = std::fs::read(&fixture.target)?;
        let source = std::fs::read(&fixture.source)?;
        let report = fixture.invoke(&fixture.plan, "verified").await?;
        assert_eq!(report["fresh_provisioning_verified"], true);
        assert_eq!(report["static_closure_authenticated"], true);
        assert_eq!(report["target"]["ordinary_operations"], 6);
        assert_eq!(report["source"]["ordinary_operations"], 5);
        assert_eq!(report["cutover_ready"], false);
        assert_eq!(report["history_transferred"], false);
        let published = fixture.home.join("verified");
        let bytes = std::fs::read(published.join("report.json"))?;
        assert_eq!(
            std::fs::read(published.join("VERIFIED"))?,
            format!("{}\n", files::sha(&bytes)).as_bytes()
        );
        assert!(!published.join("INCOMPLETE").exists());
        unchanged(&fixture.target, &target)?;
        unchanged(&fixture.source, &source)?;

        let mut wrong = fixture.approval.clone();
        wrong[1]["definition"]["credential"]["data"]["value"] = json!("different-approved-key");
        let mut plan = fixture.plan.clone();
        plan["target"]["static_manifest"] =
            json_lines(&fixture.home.join("changed-approval.jsonl"), &wrong)?;
        fixture
            .rejected(
                &plan,
                "changed-approval",
                "approved resource definition changed",
            )
            .await?;
        let mut missing = fixture.approval.clone();
        missing.pop();
        missing[0]["resource_count"] = json!(1);
        plan["target"]["static_manifest"] =
            json_lines(&fixture.home.join("missing-approval.jsonl"), &missing)?;
        fixture
            .rejected(
                &plan,
                "missing-approval",
                "approval coverage count mismatch",
            )
            .await?;

        // Only this negative hash is changed. The source credential was revoked
        // through the SDK before snapshot, so an active-only intersection query
        // would incorrectly let this target publish success.
        stopped_update(
            &fixture.target,
            "UPDATE tenancy_credentials SET key_hash=?",
            &crate::auth::keys::hash_key(&fixture.old_secret),
        )
        .await?;
        fixture
            .rejected(
                &fixture.plan,
                "old-key",
                "old ingress credential copied to target",
            )
            .await?;
        unchanged(&fixture.source, &source)?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("v2 semantic regression watchdog")?
}
