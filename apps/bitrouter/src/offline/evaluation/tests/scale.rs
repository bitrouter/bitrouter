//! Large fresh authority around the existing, deliberately bounded Eval graph.
use super::*;
use crate::offline::{
    large::{self, Options, Source},
    recovery::{
        plan::Anchor,
        tests::fixture::scale::{
            FileWitness, JsonLines, append_principals, observation::Observation, witness,
        },
        v2::format::{PROVISIONING, ProvisioningHeader, ProvisioningRecord},
    },
};
use sea_orm::sqlx::Row as _;
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
};

const TAIL: u64 = 4097;
const OPERATIONS: u64 = 8 + TAIL;
const PRINCIPALS: i64 = 4099;
const FLOOR: u64 = 512 * 1024 * 1024;
const BUFFER: usize = 64 * 1024;

fn options(home: &Path) -> Options {
    Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.into(),
        min_free_bytes: FLOOR,
    }
}
fn bundle_witness(path: &Path) -> Result<[FileWitness; 4]> {
    Ok([
        witness(&path.join("READY"))?,
        witness(&path.join("evidence.json"))?,
        witness(&path.join("inventory.key"))?,
        witness(&path.join("snapshot.sqlite"))?,
    ])
}

async fn released(path: &Path) -> Result<()> {
    let owner = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(owner) = crate::tenancy::ownership::acquire_path(path, false) {
                break owner;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("Eval scale owner did not acknowledge cleanup")?;
    Source::inspect(path)?;
    drop(owner);
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Counts {
    principals: i64,
    operations: i64,
    applied: i64,
    revision: i64,
    counter: i64,
    provenance: i64,
    samples: [String; 3],
}

async fn counts(path: &Path) -> Result<Counts> {
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let path = owner.path.clone();
    let mut connections = Connections::new(CancellationToken::new(), Some(owner));
    let checked = async {
        connections.open_immutable_source(&path).await?;
        let db = connections.source.as_mut().context("count connection")?;
        let row = sea_orm::sqlx::query(
            "SELECT (SELECT COUNT(*) FROM tenancy_principals) AS principals,
             (SELECT COUNT(*) FROM tenancy_operations) AS operations,
             (SELECT COUNT(*) FROM tenancy_operations WHERE json_extract(result_json,'$.status')='applied') AS applied,
             authority_revision,operation_ids,
             (SELECT COUNT(*) FROM eval_import_provenance) AS provenance
             FROM tenancy_installation WHERE singleton=1",
        )
        .fetch_one(&mut *db)
        .await?;
        let mut samples = std::array::from_fn(|_| String::new());
        for (slot, ordinal) in samples.iter_mut().zip([0_u64, 2048, 4096]) {
            let id = format!("{:0>128}", format!("scale-principal-{ordinal:020}"));
            let subject: String = sea_orm::sqlx::query_scalar(
                "SELECT subject FROM tenancy_principals WHERE id=? AND issuer='offline-scale-fixture'",
            )
            .bind(&id)
            .fetch_one(&mut *db)
            .await?;
            ensure!(subject == id, "unrelated SDK principal changed");
            *slot = subject;
        }
        Ok::<_, anyhow::Error>(Counts {
            principals: row.try_get("principals")?,
            operations: row.try_get("operations")?,
            applied: row.try_get("applied")?,
            revision: row.try_get("authority_revision")?,
            counter: row.try_get("operation_ids")?,
            provenance: row.try_get("provenance")?,
            samples,
        })
    }
    .await;
    let closed = connections.close().await;
    drop(connections);
    closed?;
    released(&path).await?;
    checked
}

fn malformed_tail(input: &Path, output: &Path) -> Result<(Anchor, usize)> {
    let mut reader = BufReader::with_capacity(BUFFER, File::open(input)?);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)?;
    let mut writer = BufWriter::with_capacity(BUFFER, file);
    let mut line = Vec::new();
    let mut maximum = 0;
    for index in 0..=OPERATIONS {
        line.clear();
        (&mut reader).take(8193).read_until(b'\n', &mut line)?;
        ensure!(
            !line.is_empty() && line.len() <= 8192 && line.last() == Some(&b'\n'),
            "bounded operation line required"
        );
        maximum = maximum.max(line.len());
        writer.write_all(if index == OPERATIONS { b"{}\n" } else { &line })?;
    }
    ensure!(
        reader.fill_buf()?.is_empty(),
        "unexpected provisioning section"
    );
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    Ok((
        Anchor {
            path: output.into(),
            sha256: witness(output)?.sha256,
        },
        maximum,
    ))
}

async fn extend(fixture: &mut Fixture, work_deadline: tokio::time::Instant) -> Result<FileWitness> {
    let home = fixture.directory.path().canonicalize()?;
    let path = home.join("scale-provisioning.jsonl");
    let mut stream = JsonLines::create(&path)?;
    stream.record(&ProvisioningHeader {
        format: PROVISIONING.into(),
        installation_id: fixture.provision.installation.clone(),
        operation_count: OPERATIONS,
        configuration_count: 0,
        active_count: 0,
        resource_count: 0,
    })?;
    let (live, _) = Live::open(&fixture.target, false).await?;
    let produced = tokio::time::timeout_at(work_deadline, async {
        let client = live.operator()?;
        let caller = PrincipalId::for_control_credential("offline-operator")?;
        ensure!(fixture.provision.operations.len() == 8);
        for (index, id) in fixture.provision.operations.iter().enumerate() {
            let result = client
                .operation(&fixture.provision.installation, &id.as_str().try_into()?)
                .await?;
            ensure!(
                result.status == bitrouter_ai::tenancy::operation::OperationStatus::Applied
                    && result
                        .authority_revision
                        .context("initial authority revision")?
                        .get()
                        == u64::try_from(index)? + 1
            );
            stream.record(&ProvisioningRecord::Operation {
                principal_id: caller.clone(),
                result,
            })?;
        }
        let revision = append_principals(
            &client,
            &fixture.provision.installation,
            &caller,
            0,
            TAIL,
            &mut stream,
        )
        .await?;
        ensure!(revision.get() == OPERATIONS, "actual final SDK revision");
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let closed = live.close(&fixture.target).await;
    closed?;
    produced??;
    std::fs::set_permissions(&fixture.target, std::fs::Permissions::from_mode(0o600))?;
    Source::inspect(&fixture.target)?;
    let (anchor, observed) = stream.finish()?;
    // The initial small target bundle predates the real SDK tail. Bind fresh
    // complete target evidence, rather than accepting stale provisioning bytes.
    large::probe(&home, FLOOR, witness(&fixture.target)?.bytes)?;
    let bundle = home.join("scale-target-bundle");
    let report =
        crate::offline::snapshot_with_options(&fixture.target, &bundle, options(&home)).await?;
    let report = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
    fixture.plan["format"] = json!("bitrouter.offline.eval-backfill.v2");
    fixture
        .plan
        .as_object_mut()
        .context("Eval plan")?
        .remove("provisioning_operations");
    fixture.plan["provisioning"] = serde_json::to_value(anchor)?;
    fixture.plan["target_bundle"] = json!(bundle);
    fixture.plan["target_evidence_digest"] = report["evidence_digest"].clone();
    Ok(observed)
}

#[tokio::test]
#[ignore = "explicit offline scale qualification; real SDK authority tail, 300-second gate"]
async fn bounded_selected_graph_imports_into_large_fresh_authority_and_refuses_replay() -> Result<()>
{
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(300);
    let work_deadline = deadline - Duration::from_secs(15);
    let observation = Observation::new(&std::env::temp_dir().canonicalize()?)?;
    // Retain a constructed fixture through errors/timeouts until the pre-deletion sample.
    let mut fixture = None;
    let result = {
        let work = async {
            tokio::time::timeout_at(deadline, async {
        let directory = tempfile::Builder::new().permissions(std::fs::Permissions::from_mode(0o700)).tempdir()?;
        large::probe(directory.path(), 4 * 1024 * 1024 * 1024, 0)?;
        let fixture = fixture.insert(Fixture::in_directory(directory).await?);
        let home = fixture.directory.path().canonicalize()?;
        let stream = extend(fixture, work_deadline).await?;
        let before = counts(&fixture.target).await?;
        ensure!(before.principals == PRINCIPALS && before.principals > 4096
            && before.operations == i64::try_from(OPERATIONS)? && before.applied == before.operations
            && before.counter == before.operations && before.revision == before.operations && before.provenance == 0);
        let source = witness(&fixture.source)?;
        let archive = bundle_witness(&fixture.bundle)?;
        let target_bundle = PathBuf::from(fixture.plan["target_bundle"].as_str().context("target bundle")?);
        let target_archive = bundle_witness(&target_bundle)?;
        let target = witness(&fixture.target)?;
        let good_plan = fixture.plan.clone(); // Small fixed control object, never the provisioning stream.
        let input = PathBuf::from(fixture.plan["provisioning"]["path"].as_str().context("provisioning path")?);
        let (bad, maximum_line) = malformed_tail(&input, &home.join("malformed-tail.jsonl"))?;
        fixture.plan["provisioning"] = serde_json::to_value(bad)?;
        let (path, anchor, output) = fixture.write_plan("scale-malformed")?;
        let result = tokio::time::timeout_at(work_deadline, apply::run_with_options(&path, &anchor, &output, options(&home))).await;
        released(&fixture.target).await?;
        observation.sample();
        let error = result?.err().context("malformed final operation was accepted")?;
        ensure!(format!("{error:#}").contains("invalid offline JSON-lines record"), "wrong malformed-tail stage: {error:#}");
        ensure!(!output.join("RECORDED").exists());
        ensure!(witness(&fixture.target)? == target && counts(&fixture.target).await? == before);
        fixture.plan = good_plan;
        large::probe(&home, FLOOR, target.bytes)?;
        let (path, anchor, output) = fixture.write_plan("scale-apply")?;
        let applied = tokio::time::timeout_at(work_deadline, apply::run_with_options(&path, &anchor, &output, options(&home))).await;
        released(&fixture.target).await?;
        observation.sample();
        let value = serde_json::to_value(applied??)?;
        ensure!(value["imported_counts"] == json!([2, 2, 2, 2]) && value["provenance_rows"] == 2
            && value["backfill_applied"] == true && value["complete"] == false && value["cutover_ready"] == false);
        ensure!(output.join("RECORDED").exists() && !output.join("INCOMPLETE").exists());
        let after = counts(&fixture.target).await?;
        ensure!(after.principals == before.principals && after.samples == before.samples
            && after.operations == before.operations && after.applied == before.applied
            && after.revision == before.revision && after.counter == before.counter && after.provenance == 2);
        // Exact replay refuses before a second import, preserving the committed DB.
        let committed = witness(&fixture.target)?;
        let replay = home.join("scale-replay");
        let result = tokio::time::timeout_at(work_deadline, apply::run_with_options(&path, &anchor, &replay, options(&home))).await;
        released(&fixture.target).await?;
        observation.sample();
        let error = result?.err().context("Eval replay was accepted")?;
        ensure!(format!("{error:#}").contains("target changed since provisioning evidence"), "wrong replay refusal: {error:#}");
        ensure!(!replay.join("RECORDED").exists() && witness(&fixture.target)? == committed && counts(&fixture.target).await? == after);
        let (live, _) = Live::open(&fixture.target, false).await?;
        let reads = tokio::time::timeout_at(work_deadline, async {
            for index in 0..2 {
                let tenant: TenantId = format!("target-{index}").try_into()?;
                let client = TenancyClient::new(&live.base, &fixture.provision.secrets[index])?;
                ensure!(client.eval_snapshot(&fixture.provision.installation, &tenant, &fixture.snapshots[index].evidence_root).await? == fixture.snapshots[index]);
                ensure!(client.eval_subject(&fixture.provision.installation, &tenant, &subject(index)?.eval_id).await? == subject(index)?);
                ensure!(client.eval_status(&fixture.provision.installation, &tenant).await?.results == 1);
                ensure!(client.eval_snapshot(&fixture.provision.installation, &tenant, &fixture.snapshots[1 - index].evidence_root).await.is_err());
                let row = live.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                    "SELECT manifest_json FROM eval_snapshots WHERE installation_id=? AND tenant_id=?",
                    [fixture.provision.installation.as_str().into(), tenant.as_str().into()])).await?.context("original Eval manifest")?;
                ensure!(row.try_get::<String>("", "manifest_json")? == fixture.manifests[index]);
            }
            Ok::<_, anyhow::Error>(())
        }).await;
        let closed = live.close(&fixture.target).await;
        closed?;
        observation.sample();
        reads??;
        ensure!(witness(&fixture.source)? == source && bundle_witness(&fixture.bundle)? == archive
            && bundle_witness(&target_bundle)? == target_archive);
        ensure!(counts(&fixture.target).await? == after);
        eprintln!("{}", json!({"qualification":"E","artifact":crate::artifact::current(),
            "test_executable_sha256":witness(&std::env::current_exe()?)?.sha256,
            "principal_rows":after.principals,"applied_operations":after.applied,"authority_revision":after.revision,
            "provisioning_bytes":stream.bytes,"provisioning_sha256":stream.sha256,"imported_counts":[2,2,2,2],"provenance_rows":after.provenance,
            "file_buffer_bytes":BUFFER,"max_operation_line_bytes":maximum_line,"initial_operation_ids_retained":8,
            "working_bytes":options(&home).working_bytes,"free_floor":FLOOR,"elapsed_ms":started.elapsed().as_millis(),
            "rss":"external exact-test process measurement required","cleanup":"owner reacquired after acknowledged close"}));
        Ok::<_, anyhow::Error>(())
            }).await?
        };
        tokio::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => break result,
                _ = observation.tick() => {}
            }
        }
    };
    observation.sample();
    drop(fixture);
    observation.sample();
    let measured = observation.finish("E:parent", result.is_ok(), false);
    result?;
    measured?;
    ensure!(
        tokio::time::Instant::now() <= deadline,
        "E qualification exceeded 300 seconds including cleanup and observations"
    );
    Ok(())
}
