use super::*;

pub(super) async fn produce_unadmitted(db: &DatabaseConnection, owner: &str) -> Result<()> {
    let store = crate::core::eval::store::EvalStore::new(db.clone());
    let subject = subject(2)?;
    store.insert_subject_owned(&subject, owner).await?;
    let result: EvaluationResult = serde_json::from_value(json!({
        "schema_version":1,"eval_id":subject.eval_id,"evidence_digest":subject.evidence_digest,
        "evaluator":{"authority_id":"legacy-author","evaluator_id":"human","kind":"human","version":"1","config_digest":subject.policy_digest},
        "verdict":"pass","metrics":{},"hard_violations":[],"evidence_refs":[],"decision_credit":{},
        "idempotency_key":"committed-before-admission","submitted_at":"2026-08-01T00:01:00Z"
    }))?;
    // This public Legacy call commits its own result transaction. Deliberately
    // stop before append_admission_event, exactly as an interrupted producer can.
    let inserted = store.insert_result_owned(&result, owner).await?;
    let crate::core::eval::store::ResultInsertOutcome::Inserted { result_id } = inserted else {
        anyhow::bail!("incomplete fixture did not create a new result");
    };
    ensure!(
        store
            .result(&result_id)
            .await?
            .context("committed Legacy result")?
            .result
            == result
    );
    ensure!(
        !store
            .latest_admissions_for_owner(owner)
            .await?
            .contains_key(&result_id),
        "incomplete fixture unexpectedly has admission history"
    );
    let snapshot = store
        .materialize_snapshot("2026-08-01T08:04:00+08:00")
        .await?;
    ensure!(
        snapshot.entries.len() == 2
            && snapshot
                .entries
                .iter()
                .all(|entry| entry.result_id != result_id),
        "Legacy materialization must preserve the two admitted controls and omit the unadmitted result"
    );
    Ok(())
}

async fn unchanged_target(fixture: &Fixture, before: &[u8]) -> Result<()> {
    // A real file lease is reacquired before any evidence descriptor is opened.
    let owner = crate::tenancy::ownership::acquire_path(&fixture.target, false)?;
    ensure!(
        std::fs::read(&fixture.target)? == before,
        "failed preflight changed target bytes"
    );
    drop(owner);
    Ok(())
}
fn revise_mapping(
    fixture: &mut Fixture,
    name: &str,
    update: impl FnOnce(&mut Value) -> Result<()>,
) -> Result<()> {
    let path = Path::new(fixture.plan["manifest"].as_str().context("manifest path")?);
    let mut value: Value = serde_json::from_slice(&files::read(path, 8 * 1024 * 1024)?)?;
    update(&mut value)?;
    let path = fixture
        .directory
        .path()
        .join(format!("{name}-mapping.json"));
    let bytes = serde_json::to_vec(&value)?;
    files::write_new(&path, &bytes)?;
    fixture.plan["manifest"] = json!(path);
    fixture.plan["manifest_digest"] = json!(files::sha(&bytes));
    Ok(())
}

#[tokio::test]
async fn wrong_partition_namespace_counts_digest_and_merge_collision_do_not_write() -> Result<()> {
    tokio::time::timeout(TIMEOUT,async {
        let mut fixture=Fixture::new().await?;
        let original=fixture.plan.clone();let before=std::fs::read(&fixture.target)?;
        for case in ["namespace","count","digest","foreign-owner","collision"] {
            fixture.plan=original.clone();
            revise_mapping(&mut fixture,case,|mapping|{
                let entries=mapping["mappings"].as_array_mut().context("mapping entries")?;
                let first=entries.iter_mut().find(|e|e["old_owner"]=="owner").context("owner mapping")?;
                match case {
                    "namespace"=>first["source_kind"]=json!("shared_user"),
                    "count"=>first["source_count"]=json!(3),
                    "digest"=>first["source_content_digest"]=json!("0".repeat(64)),
                    "foreign-owner"=>first["old_owner"]=json!("unrelated"),
                    "collision"=>{
                        // Actual equal logical idempotency names in distinct
                        // owners collide after an explicitly approved owner merge.
                        first["tenant_id"]=json!("target-1");first["principal_id"]=json!("reader-1");
                        mapping["tenant_merge_approvals"]=json!([{"tenant_id":"target-1",
                            "operator_principal_id":"operator.offline","evidence_digest":"e".repeat(64),"rationale":"Explicit fixture merge"}]);
                    },_=>anyhow::bail!("case"),
                }
                Ok(())
            })?;
            let (plan,anchor,report)=fixture.write_plan(case)?;
            let error=apply::run(&plan,&anchor,&report).await.err().context("invalid mapping accepted")?;
            if case=="collision" {ensure!(error.to_string().contains("merged idempotency collision"),"collision guard not reached: {error:#}");}
            ensure!(!report.join("RECORDED").exists());unchanged_target(&fixture,&before).await?;
        }
        Ok::<_,anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn actual_reader_requires_exact_import_provenance_and_unmodified_snapshot() -> Result<()> {
    tokio::time::timeout(TIMEOUT,async {
        let fixture=Fixture::new().await?;let (plan,anchor,report)=fixture.write_plan("applied")?;
        apply::run(&plan,&anchor,&report).await?;
        for field in ["source_installation","source_kind","source_owner","original_root","mapping_digest","database_evidence_digest","target_tenant","snapshot_key","oversized-proof","manifest-bytes","absent-proof"] {
            // Integrity failures deliberately fence this Service. Each case gets
            // an actual new startup and a positive read before its own corruption.
            let (live,_)=Live::open(&fixture.target,false).await?;
            let checked=async {
                let tenant:TenantId="target-0".try_into()?;
                let client=TenancyClient::new(&live.base,&fixture.provision.secrets[0])?;
                let original=&fixture.snapshots[0];
                ensure!(client.eval_snapshot(&fixture.provision.installation,&tenant,&original.evidence_root).await?==*original);
                let row=live.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
                    "SELECT snapshot_key,provenance_json FROM eval_import_provenance WHERE tenant_id='target-0'".to_owned())).await?.context("actual provenance row")?;
                let key:String=row.try_get("","snapshot_key")?;let proof:String=row.try_get("","provenance_json")?;
                match field {
                    "oversized-proof"=>{live.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                        "UPDATE eval_import_provenance SET provenance_json=? WHERE snapshot_key=?",[format!("{proof}{}"," ".repeat(16*1024)).into(),key.clone().into()])).await?;},
                    "manifest-bytes"=>{live.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                        "UPDATE eval_snapshots SET manifest_json=? WHERE evidence_root=?",[format!(" {}",fixture.manifests[0]).into(),key.clone().into()])).await?;},
                    "absent-proof"=>{live.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                        "DELETE FROM eval_import_provenance WHERE snapshot_key=?",[key.clone().into()])).await?;},
                    _=>{
                        let mut altered:Value=serde_json::from_str(&proof)?;
                        altered[field]=json!(if field.contains("digest"){ "0".repeat(64) }else{"foreign".into()});
                        live.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                            "UPDATE eval_import_provenance SET provenance_json=? WHERE snapshot_key=?",[serde_json::to_string(&altered)?.into(),key.clone().into()])).await?;
                    },
                }
                let rejected=client.eval_snapshot(&fixture.provision.installation,&tenant,&original.evidence_root).await;
                // Restore exact saved bytes only after this real negative request;
                // do not ask an already-fenced service to supply a false control.
                if field=="manifest-bytes" {
                    live.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                        "UPDATE eval_snapshots SET manifest_json=? WHERE evidence_root=?",[fixture.manifests[0].clone().into(),key.clone().into()])).await?;
                } else if field!="absent-proof" {
                    live.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                        "UPDATE eval_import_provenance SET provenance_json=? WHERE snapshot_key=?",[proof.into(),key.into()])).await?;
                }
                let expected=if field=="oversized-proof" {bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded}
                    else {bitrouter_ai::tenancy::operation::ErrorCode::StorageUnavailable};
                ensure!(matches!(rejected,Err(bitrouter_sdk::error::Error::Tenancy(ref error))
                    if error.body.as_ref().is_some_and(|b|b.code==expected)),
                    "case did not reach integrity guard: {field}");
                Ok::<_,anyhow::Error>(())
            }.await;
            let close=live.close(&fixture.target).await;checked?;close?;
        }
        Ok::<_,anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn target_anchor_counter_and_evidence_mismatches_refuse_before_dml() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let mut fixture = Fixture::new().await?;
        let original = fixture.plan.clone();
        let before = std::fs::read(&fixture.target)?;
        for case in [
            "wrong-target",
            "missing-operation",
            "wrong-evidence",
            "wrong-plan-anchor",
        ] {
            fixture.plan = original.clone();
            match case {
                "wrong-target" => {
                    fixture.plan["target_installation"] = json!("another-installation")
                }
                "missing-operation" => {
                    fixture.plan["provisioning_operations"]
                        .as_array_mut()
                        .context("operations")?
                        .pop();
                }
                "wrong-evidence" => {
                    fixture.plan["ownership_evidence_digest"] = json!("0".repeat(64))
                }
                "wrong-plan-anchor" => {}
                _ => anyhow::bail!("case"),
            }
            let (plan, anchor, report) = fixture.write_plan(case)?;
            let anchor = if case == "wrong-plan-anchor" {
                "0".repeat(64)
            } else {
                anchor
            };
            ensure!(apply::run(&plan, &anchor, &report).await.is_err());
            ensure!(!report.join("RECORDED").exists());
            unchanged_target(&fixture, &before).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn reanchored_actual_source_orphans_and_partial_metadata_fail_graph_preflight() -> Result<()>
{
    tokio::time::timeout(TIMEOUT, async {
        for (name, sql, guard) in [
            (
                "orphan",
                "DELETE FROM eval_results WHERE owner_user_id='owner'",
                "admission parent missing",
            ),
            (
                "metadata",
                "UPDATE eval_snapshots SET tenant_id='partial' WHERE owner_user_id='owner'",
                "mixed ownership",
            ),
        ] {
            let mut fixture = Fixture::new().await?;
            let database =
                crate::db::connect_owned_tenancy(&format!("sqlite://{}", fixture.source.display()))
                    .await?;
            database
                .database()
                .execute(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    sql.to_owned(),
                ))
                .await?;
            database.database().clone().close().await?;
            drop(database);
            let owner = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Ok(owner) =
                        crate::tenancy::ownership::acquire_path(&fixture.source, false)
                    {
                        break owner;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            drop(owner);
            let bundle = fixture.directory.path().join(format!("{name}-bundle"));
            let evidence = super::super::super::snapshot_inner(&fixture.source, &bundle).await?;
            fixture.plan["bundle"] = json!(bundle);
            fixture.plan["database_evidence_digest"] = json!(evidence.evidence_digest);
            revise_mapping(&mut fixture, name, |mapping| {
                mapping["source_snapshot_digest"] = json!(evidence.evidence.snapshot_digest);
                Ok(())
            })?;
            let before = std::fs::read(&fixture.target)?;
            let (plan, anchor, report) = fixture.write_plan(name)?;
            let error = apply::run(&plan, &anchor, &report)
                .await
                .err()
                .context("malformed graph imported")?;
            ensure!(
                error.to_string().contains(guard),
                "did not reach exact graph guard: {error:#}"
            );
            unchanged_target(&fixture, &before).await?;
            ensure!(!report.join("RECORDED").exists());
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn real_service_provisioned_viewer_is_not_promoted_by_offline_mapping() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::in_directory_role(tempfile::tempdir()?, TenantRole::Viewer).await?;
        let before = std::fs::read(&fixture.target)?;
        let (plan, anchor, report) = fixture.write_plan("viewer-refusal")?;
        let error = apply::run(&plan, &anchor, &report)
            .await
            .err()
            .context("viewer mapping accepted")?;
        ensure!(
            error
                .to_string()
                .contains("current tenant-wide admin binding"),
            "wrong refusal boundary: {error:#}"
        );
        unchanged_target(&fixture, &before).await?;
        ensure!(!report.join("RECORDED").exists());
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

fn incomplete_partition(fixture: &Fixture) -> Result<Value> {
    let packet: Value = serde_json::from_slice(&files::read(
        &fixture.directory.path().join("partitions/partitions.json"),
        2 * 1024 * 1024,
    )?)?;
    let partition = packet["partitions"]
        .as_array()
        .context("actual source partitions")?
        .iter()
        .find(|partition| partition["owner"] == "OWNER")
        .context("incomplete owner partition was dropped from inventory")?;
    ensure!(
        partition["table_counts"] == json!([2, 2, 1, 1]) && partition["source_count"] == 6,
        "inventory must retain both committed results, including the unadmitted one"
    );
    Ok(partition.clone())
}

#[tokio::test]
async fn committed_legacy_result_without_admission_refuses_selected_partition_before_dml()
-> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::in_directory_role_with_unadmitted(
            tempfile::tempdir()?,
            TenantRole::TenantAdmin,
            Some("OWNER"),
        )
        .await?;
        incomplete_partition(&fixture)?;
        let before = std::fs::read(&fixture.target)?;
        let source_before = std::fs::read(&fixture.source)?;
        let bundle_before = std::fs::read(fixture.bundle.join("snapshot.sqlite"))?;
        let (plan, anchor, report) = fixture.write_plan("missing-admission")?;
        let error = apply::run(&plan, &anchor, &report)
            .await
            .err()
            .context("mapped result without admission was imported")?;
        ensure!(
            error.to_string() == "mapped result lacks admission history",
            "did not reach destination admission preflight: {error:#}"
        );
        // This guard runs before report creation and before the target writer
        // opens; exact target bytes also prove no partial complete partition.
        ensure!(!report.exists());
        unchanged_target(&fixture, &before).await?;
        ensure!(std::fs::read(&fixture.source)? == source_before);
        ensure!(std::fs::read(fixture.bundle.join("snapshot.sqlite"))? == bundle_before);
        let (live, _) = Live::open(&fixture.target, false).await?;
        let checked = async {
            for index in 0..2 {
                let tenant: TenantId = format!("target-{index}").try_into()?;
                let client = TenancyClient::new(&live.base, &fixture.provision.secrets[index])?;
                let status = client
                    .eval_status(&fixture.provision.installation, &tenant)
                    .await?;
                ensure!(status.subjects == 0 && status.results == 0);
            }
            ensure!(
                live.operator()?.capabilities().await?.installation_id
                    == fixture.provision.installation
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = live.close(&fixture.target).await;
        checked?;
        closed?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn unselected_incomplete_legacy_partition_is_quarantined_while_complete_owner_imports()
-> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let mut fixture = Fixture::in_directory_role_with_unadmitted(
            tempfile::tempdir()?,
            TenantRole::TenantAdmin,
            Some("OWNER"),
        )
        .await?;
        let partition = incomplete_partition(&fixture)?;
        // Record a distinct explicit operator disposition, retaining the same
        // source partition counts/digests and reanchoring all changed evidence.
        let original = Path::new(
            fixture.plan["ownership_evidence"].as_str().context("ownership path")?,
        );
        let mut evidence: Value = serde_json::from_slice(&files::read(original, 2 * 1024 * 1024)?)?;
        evidence["decision"] = json!("import owner; quarantine incomplete OWNER partition and wildcard");
        evidence["quarantined_owners"] = json!(["OWNER"]);
        let evidence = serde_json::to_vec(&evidence)?;
        let path = fixture.directory.path().join("quarantine-evidence.json");
        files::write_new(&path, &evidence)?;
        let digest = files::sha(&evidence);
        fixture.plan["ownership_evidence"] = json!(path);
        fixture.plan["ownership_evidence_digest"] = json!(digest);
        revise_mapping(&mut fixture, "quarantined-incomplete", |mapping| {
            let entries = mapping["mappings"].as_array_mut().context("mapping entries")?;
            for entry in entries.iter_mut() {
                entry["evidence_digest"] = json!(digest);
                entry["evidence_ref"] = json!("quarantine-evidence.json");
            }
            let selected = entries.iter_mut().find(|entry| entry["old_owner"] == "OWNER")
                .context("incomplete owner mapping")?;
            ensure!(selected["disposition"] == "mapped");
            selected["disposition"] = json!("quarantined");
            selected["tenant_id"] = Value::Null;
            selected["principal_id"] = Value::Null;
            selected["rationale"] = json!("Preserve the whole owner partition outside the live target until admission closure is established");
            Ok(())
        })?;
        let source_before = std::fs::read(&fixture.source)?;
        let bundle_before = std::fs::read(fixture.bundle.join("snapshot.sqlite"))?;
        let (plan, anchor, output) = fixture.write_plan("quarantine-incomplete")?;
        let report = serde_json::to_value(apply::run(&plan, &anchor, &output).await?)?;
        ensure!(report["imported_counts"] == json!([1, 1, 1, 1]) && report["provenance_rows"] == 1);
        let quarantine = report["quarantined_eval"].as_array().context("quarantine report")?;
        ensure!(quarantine.len() == 2 && quarantine.contains(&partition));
        ensure!(report["backfill_applied"] == true && report["complete"] == false && report["cutover_ready"] == false);
        ensure!(output.join("RECORDED").exists() && !output.join("INCOMPLETE").exists());
        ensure!(std::fs::read(&fixture.source)? == source_before);
        ensure!(std::fs::read(fixture.bundle.join("snapshot.sqlite"))? == bundle_before);
        let (live, _) = Live::open(&fixture.target, false).await?;
        let checked = async {
            let tenant: TenantId = "target-0".try_into()?;
            let client = TenancyClient::new(&live.base, &fixture.provision.secrets[0])?;
            let status = client.eval_status(&fixture.provision.installation, &tenant).await?;
            ensure!(status.subjects == 1 && status.results == 1);
            ensure!(client.eval_subject(&fixture.provision.installation, &tenant, &subject(0)?.eval_id).await? == subject(0)?);
            ensure!(client.eval_snapshot(&fixture.provision.installation, &tenant, &fixture.snapshots[0].evidence_root).await? == fixture.snapshots[0]);
            let tenant: TenantId = "target-1".try_into()?;
            let client = TenancyClient::new(&live.base, &fixture.provision.secrets[1])?;
            let status = client.eval_status(&fixture.provision.installation, &tenant).await?;
            ensure!(status.subjects == 0 && status.results == 0);
            ensure!(live.operator()?.capabilities().await?.installation_id == fixture.provision.installation);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = live.close(&fixture.target).await;
        checked?;
        closed?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
