//! Real connector/Service/SDK producers followed by the actual bro executable.
use super::*;
use anyhow::Context;
use serde_json::{Value, json};
use std::{ffi::OsStr, io::Read, os::unix::fs::PermissionsExt, process::Stdio};
use tokio::io::AsyncReadExt;
mod fixture;
mod saturation;
mod selected_files;

const OUTPUT: u64 = 64 * 1024;
// The producer test executable is evidence to hash, not a recovery DB/input.
// Keep its streaming bound independent from the unchanged product file ceiling.
const TEST_PRODUCER_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
#[derive(Clone)]
pub(super) struct Hold {
    pub entered: CancellationToken,
    pub release: CancellationToken,
}
struct Fixture {
    directory: tempfile::TempDir,
    source: PathBuf,
    target: PathBuf,
    bundle: PathBuf,
    plan: Value,
    source_provision: fixture::Provision,
    target_provision: fixture::Provision,
    provisioning: Value,
}
fn binary() -> Result<PathBuf> {
    let binary = PathBuf::from(
        std::env::var_os("BITROUTER_TEST_BRO")
            .context("explicit built BITROUTER_TEST_BRO required; test never builds or skips")?,
    );
    ensure!(binary.is_absolute(), "absolute bro binary required");
    let binary = binary.canonicalize()?;
    ensure!(
        std::fs::metadata(&binary)?.is_file(),
        "actual bro binary required"
    );
    crate::runtime_crypto::DeploymentKey::from_environment(fixture::KEY_ENV)?;
    Ok(binary)
}
fn executable_digest() -> Result<String> {
    use sha2::{Digest, Sha256};
    let file = std::fs::File::open(std::env::current_exe()?)?;
    ensure!(
        file.metadata()?.len() <= TEST_PRODUCER_ARTIFACT_BYTES,
        "producer executable evidence bound"
    );
    let mut reader = file.take(TEST_PRODUCER_ARTIFACT_BYTES + 1);
    let mut digest = Sha256::new();
    let mut buf = [0_u8; 65536];
    let mut total = 0_u64;
    loop {
        let count = reader.read(&mut buf)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(
            total <= TEST_PRODUCER_ARTIFACT_BYTES,
            "producer grew beyond bound"
        );
        digest.update(&buf[..count]);
    }
    Ok(hex::encode(digest.finalize()))
}
impl Fixture {
    async fn new() -> Result<Self> {
        Self::with_saturated_source(false).await
    }
    async fn with_saturated_source(saturate: bool) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("recovery-cli-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let source = directory.path().join("source.sqlite");
        let target = directory.path().join("target.sqlite");
        let source_live = fixture::Live::open(&source, None, None).await?;
        let source_result = async {
            let provision = fixture::provision(&source_live).await?;
            if saturate {
                saturation::fill(&source_live, &provision).await?;
            }
            Ok::<_, anyhow::Error>(provision)
        }
        .await;
        let source_close = source_live.close(&source).await;
        let source_provision = source_result?;
        source_close?;
        let target_live = fixture::Live::open(&target, None, Some(fixture::KEY_ENV)).await?;
        let target_result = fixture::provision(&target_live).await;
        let target_close = target_live.close(&target).await;
        let mut target_provision = target_result?;
        target_close?;
        ensure!(
            source_provision.installation != target_provision.installation
                && source_provision.secret != target_provision.secret,
            "fresh normal Service identity/credential missing"
        );
        let import_path = directory.path().join("approved-new-static.json");
        let manifest = fixture::private_json(
            &import_path,
            &fixture::manifest(&target_provision.installation),
        )?;
        let serving =
            fixture::Live::open(&target, Some(&import_path), Some(fixture::KEY_ENV)).await?;
        let configured =
            fixture::configure(&serving, &mut target_provision, &source_provision).await;
        let stopped = serving.close(&target).await;
        let provisioning = configured?;
        stopped?;
        let provisioning_anchor =
            fixture::private_json(&directory.path().join("provisioning.json"), &provisioning)?;
        let bundle = directory.path().join("archive");
        let evidence = crate::offline::snapshot_inner(&source, &bundle).await?;
        let commit = crate::artifact::current()
            .source_revision
            .context("source executable lacks actual revision evidence")?;
        let artifact_sha = executable_digest()?;
        let producer = fixture::private_json(
            &directory.path().join("producer.json"),
            &json!({"format":"bitrouter.offline.source-producer-attestation.v1","installation_id":source_provision.installation,"commit":commit,"artifact_sha256":artifact_sha,"database_evidence_digest":evidence.evidence_digest}),
        )?;
        let external_path = directory.path().join("retired-external.secret");
        let external_bytes = b"PRIVATE_RETIRED_EXTERNAL_BYTES";
        files::write_new(&external_path, external_bytes)?;
        let key: [u8; 32] = files::read(&bundle.join("inventory.key"), 32)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("fixture inventory key width"))?;
        let mut fingerprint = files::Fingerprint::new(&key, b"recovery-selected-file.v1")?;
        fingerprint.field(external_bytes);
        let selected = json!([{"path":external_path,"writer":"fixture-source","bytes":external_bytes.len(),"keyed_digest":hex::encode(fingerprint.finish())}]);
        let writers = json!([{"id":"fixture-source","process_identity":format!("fixture-process:{}:source-Service",std::process::id()),"restart_source":"none-isolated-fixture","retired":true}]);
        let retirement = fixture::private_json(
            &directory.path().join("retirement.json"),
            &json!({"format":"bitrouter.offline.retirement-attestation.v1","source_installation_id":source_provision.installation,"database_evidence_digest":evidence.evidence_digest,"fence_maintained":true,"target_ingress_fenced":true,"selection_exhaustive":true,"unresolved_effects_remain_unknown":true,"writers":writers,"selected_files":selected}),
        )?;
        let plan = json!({"format":plan::FORMAT,
            "source":{"bundle":bundle,"evidence_digest":evidence.evidence_digest,"installation_id":source_provision.installation,"producer":{"commit":commit,"artifact_sha256":artifact_sha,"evidence":producer},"tables":evidence.evidence.inventory.tables.iter().map(|t|json!({"table":t.name,"rows":t.rows,"value_bytes":t.value_bytes,"digest":t.digest,"disposition":"retained_archive"})).collect::<Vec<_>>()},
            "target":{"database":target,"installation_id":target_provision.installation,"provisioning":provisioning_anchor,"wrapping_key_env":fixture::KEY_ENV,"static_manifest":manifest},
            "retirement":{"fence_maintained":true,"target_ingress_fenced":true,"selection_exhaustive":true,"unresolved_effects_remain_unknown":true,"evidence":retirement,"writers":writers},"selected_files":selected});
        Ok(Self {
            directory,
            source,
            target,
            bundle,
            plan,
            source_provision,
            target_provision,
            provisioning,
        })
    }
    fn secret_free(&self, bytes: &[u8]) -> Result<()> {
        let key = std::env::var(fixture::KEY_ENV)?;
        for secret in [
            fixture::OPERATOR,
            "PRIVATE_RECOVERY_PROVIDER_KEY",
            "PRIVATE_RETIRED_EXTERNAL_BYTES",
            &self.source_provision.secret,
            &self.target_provision.secret,
            key.as_str(),
        ] {
            ensure!(
                !secret.is_empty() && !bytes.windows(secret.len()).any(|v| v == secret.as_bytes()),
                "CLI leaked fixture secret"
            );
        }
        Ok(())
    }
}
async fn invoke(
    binary: &Path,
    fixture: &Fixture,
    plan: &Path,
    report: &Path,
    key: Option<&OsStr>,
) -> Result<std::process::Output> {
    let mut command = tokio::process::Command::new(binary);
    command
        .args([
            OsStr::new("--json"),
            OsStr::new("tenancy"),
            OsStr::new("offline"),
            OsStr::new("recover"),
            OsStr::new("--plan"),
            plan.as_os_str(),
            OsStr::new("--report"),
            report.as_os_str(),
        ])
        .current_dir(fixture.directory.path())
        .env("BITROUTER_HOME", fixture.directory.path())
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("RUST_LOG", "error")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(key) = key {
        command.env(fixture::KEY_ENV, key);
    }
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take().context("CLI stdout")?;
    let mut stderr = child.stderr.take().context("CLI stderr")?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let (_, _, status) = tokio::try_join!(
            async {
                (&mut stdout).take(OUTPUT + 1).read_to_end(&mut out).await?;
                ensure!(out.len() as u64 <= OUTPUT, "CLI stdout bound");
                Ok::<_, anyhow::Error>(())
            },
            async {
                (&mut stderr).take(OUTPUT + 1).read_to_end(&mut err).await?;
                ensure!(err.len() as u64 <= OUTPUT, "CLI stderr bound");
                Ok::<_, anyhow::Error>(())
            },
            async { child.wait().await.map_err(anyhow::Error::from) }
        )?;
        Ok::<_, anyhow::Error>(status)
    })
    .await;
    if !matches!(result, Ok(Ok(_))) {
        let _ = child.start_kill();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("CLI child kill/reap timeout")??;
    }
    fixture.secret_free(&out)?;
    fixture.secret_free(&err)?;
    Ok(std::process::Output {
        status: result.context("CLI parent watchdog")??,
        stdout: out,
        stderr: err,
    })
}
fn rejected(output: &std::process::Output, report: &Path) -> Result<()> {
    ensure!(
        output.status.code() == Some(1),
        "CLI rejection status: {}",
        output.status
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        value["error"]["message"]
            == "offline recovery verification failed or cleanup is unconfirmed: retain incomplete evidence and the stopped target; do not release ingress",
        "wrong CLI rejection boundary"
    );
    ensure!(
        !report.join("VERIFIED").exists(),
        "failed verification published marker"
    );
    Ok(())
}
fn stopped_bytes(path: &Path) -> Result<Vec<u8>> {
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let bytes = files::read(path, 32 * 1024 * 1024)?;
    drop(owner);
    Ok(bytes)
}

#[tokio::test]
#[ignore = "actual bro CLI and normal Service/SDK; requires explicit BITROUTER_TEST_BRO and BITROUTER_TEST_RECOVERY_KEY"]
async fn actual_cli_verifies_fresh_static_provisioning_and_rejects_changed_evidence() -> Result<()>
{
    let binary = binary()?;
    tokio::time::timeout(Duration::from_secs(110), async {
        let fixture = Fixture::new().await?;
        let source = stopped_bytes(&fixture.source)?;
        let target = stopped_bytes(&fixture.target)?;
        let archive = files::read(&fixture.bundle.join("snapshot.sqlite"), 32 * 1024 * 1024)?;
        let evidence = files::read(&fixture.bundle.join("evidence.json"), format::MAX_EVIDENCE)?;
        let plan = fixture.directory.path().join("plan.json");
        fixture::private_json(&plan, &fixture.plan)?;
        let report = fixture.directory.path().join("verified");
        let result = invoke(&binary, &fixture, &plan, &report, None).await?;
        ensure!(
            result.status.code() == Some(0),
            "actual recover CLI failed: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        let value: Value = serde_json::from_slice(&result.stdout)?;
        ensure!(
            value["fresh_provisioning_verified"] == true
                && value["static_closure_authenticated"] == true
                && value["complete"] == false
                && value["cutover_ready"] == false
                && value["external_writers_independently_excluded"] == false
                && value["source"]["ordinary_operations"] == 4
                && value["target"]["ordinary_operations"] == 6,
            "recovery evidence claims/counts"
        );
        let report_bytes = files::read(&report.join("report.json"), format::MAX_EVIDENCE)?;
        ensure!(
            files::read(&report.join("VERIFIED"), 65)?
                == format!("{}\n", files::sha(&report_bytes)).as_bytes()
                && !report.join("INCOMPLETE").exists(),
            "actual marker/hash publication"
        );
        fixture.secret_free(&report_bytes)?;
        ensure!(
            files::read(&report.join("selected-000.opaque"), 1024)?
                == b"PRIVATE_RETIRED_EXTERNAL_BYTES",
            "retired external opaque bytes were not preserved"
        );
        let repeated = invoke(&binary, &fixture, &plan, &report, None).await?;
        ensure!(
            repeated.status.code() == Some(1)
                && files::read(&report.join("report.json"), format::MAX_EVIDENCE)? == report_bytes,
            "existing evidence was overwritten"
        );
        for (index, mut altered) in [
            fixture.plan.clone(),
            fixture.plan.clone(),
            fixture.plan.clone(),
        ]
        .into_iter()
        .enumerate()
        {
            match index {
                0 => {
                    altered["source"]["tables"]
                        .as_array_mut()
                        .context("tables")?
                        .pop();
                }
                1 => {
                    altered["target"]["installation_id"] =
                        fixture.plan["source"]["installation_id"].clone()
                }
                _ => altered["retirement"]["fence_maintained"] = json!(false),
            }
            let path = fixture.directory.path().join(format!("bad-{index}.json"));
            fixture::private_json(&path, &altered)?;
            let report = fixture.directory.path().join(format!("bad-report-{index}"));
            rejected(
                &invoke(&binary, &fixture, &path, &report, None).await?,
                &report,
            )?;
            ensure!(
                stopped_bytes(&fixture.target)? == target,
                "negative verification modified target"
            );
        }
        let wrong_key = fixture.directory.path().join("wrong-key-report");
        use base64::Engine;
        let mut invalid_key = base64::engine::general_purpose::STANDARD.encode([255_u8; 32]);
        if std::env::var(fixture::KEY_ENV)? == invalid_key {
            invalid_key = base64::engine::general_purpose::STANDARD.encode([254_u8; 32]);
        }
        rejected(
            &invoke(
                &binary,
                &fixture,
                &plan,
                &wrong_key,
                Some(OsStr::new(&invalid_key)),
            )
            .await?,
            &wrong_key,
        )?;
        let busy = crate::tenancy::ownership::acquire_path(&fixture.target, false)?;
        let busy_report = fixture.directory.path().join("busy-report");
        let busy_result = invoke(&binary, &fixture, &plan, &busy_report, None).await;
        drop(busy);
        rejected(&busy_result?, &busy_report)?;
        ensure!(
            stopped_bytes(&fixture.source)? == source
                && stopped_bytes(&fixture.target)? == target
                && files::read(&fixture.bundle.join("snapshot.sqlite"), 32 * 1024 * 1024)?
                    == archive
                && files::read(&fixture.bundle.join("evidence.json"), format::MAX_EVIDENCE)?
                    == evidence,
            "read-only verification changed source/target/evidence bytes"
        );
        let live = fixture::Live::open(&fixture.target, None, Some(fixture::KEY_ENV)).await?;
        let result = async {
            let client = bitrouter_sdk::tenancy::TenancyClient::new(
                &live.base,
                &fixture.target_provision.secret,
            )?;
            let expected: bitrouter_ai::tenancy::configuration::ConfigurationRevision =
                serde_json::from_value(fixture.provisioning["configurations"][0].clone())?;
            let actual = client
                .configuration(
                    &fixture.target_provision.installation,
                    &expected.tenant_id,
                    &expected.revision_id,
                )
                .await?;
            ensure!(
                serde_json::to_value(actual)? == serde_json::to_value(expected)?,
                "ordinary restarted target reader changed"
            );
            let operator = live.operator()?;
            let operator_id = bitrouter_ai::tenancy::identity::PrincipalId::for_control_credential(
                fixture::OPERATOR_ID,
            )?;
            for observed in fixture.provisioning["operations"]
                .as_array()
                .context("declared operations")?
            {
                let expected: bitrouter_ai::tenancy::operation::OperationResult =
                    serde_json::from_value(observed["result"].clone())?;
                let reader = match observed["principal_id"]
                    .as_str()
                    .context("operation principal")?
                {
                    "owner" => &client,
                    principal if principal == operator_id.as_str() => &operator,
                    _ => anyhow::bail!("unexpected provisioning principal"),
                };
                let actual = reader
                    .operation(
                        &fixture.target_provision.installation,
                        &expected.operation_id,
                    )
                    .await?;
                ensure!(
                    serde_json::to_value(actual)? == serde_json::to_value(expected)?,
                    "restarted ordinary operation lookup differs from verified provisioning"
                );
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = live.close(&fixture.target).await;
        result?;
        cleanup?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("whole actual CLI campaign exceeded original finite parent watchdog")?
}

#[tokio::test]
#[ignore = "actual bro CLI and negative-only operation corruption; explicit binary/key required"]
async fn actual_cli_refuses_unreadable_operation_context_and_timestamp() -> Result<()> {
    let binary = binary()?;
    tokio::time::timeout(Duration::from_secs(110), async {
        for (name, mutation) in [
            ("context", "UPDATE tenancy_operations SET read_context_json='{}' WHERE operation_id='draft'"),
            ("timestamp", "UPDATE tenancy_operations SET created_at='not-a-timestamp' WHERE operation_id='draft'"),
        ] {
            // Every positive row was produced through the actual Service/SDK.
            // Only this declared operation field is corrupted for rejection.
            let fixture = Fixture::new().await?;
            let source = stopped_bytes(&fixture.source)?;
            let owner = crate::tenancy::ownership::acquire_path(&fixture.target, false)?;
            use sea_orm::sqlx::{ConnectOptions, Connection};
            let mut db = sea_orm::sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&fixture.target)
                .create_if_missing(false)
                .connect()
                .await?;
            let altered = sea_orm::sqlx::query(mutation).execute(&mut db).await;
            let closed = db.close().await;
            drop(owner);
            ensure!(altered?.rows_affected() == 1, "negative fixture did not alter one actual operation");
            closed?;
            let target = stopped_bytes(&fixture.target)?;
            let plan = fixture.directory.path().join("operation-plan.json");
            fixture::private_json(&plan, &fixture.plan)?;
            let report = fixture.directory.path().join(format!("bad-operation-{name}"));
            rejected(&invoke(&binary, &fixture, &plan, &report, None).await?, &report)?;
            ensure!(stopped_bytes(&fixture.target)? == target && stopped_bytes(&fixture.source)? == source,
                "operation metadata rejection modified source or target bytes");
        }
        Ok::<_, anyhow::Error>(())
    }).await.context("operation metadata campaign exceeded original finite parent watchdog")?
}

#[tokio::test]
#[ignore = "actual bro CLI, Service/SDK and negative-only corruption; explicit binary/key required"]
async fn actual_cli_refuses_definition_substitution_and_copied_old_credential() -> Result<()> {
    let binary = binary()?;
    tokio::time::timeout(Duration::from_secs(110), async {
        let fixture = Fixture::new().await?;
        let mut changed = fixture::manifest(&fixture.target_provision.installation);
        changed["resources"][0]["definition"]["credential"]["data"]["value"] =
            json!("DIFFERENT_APPROVED_STATIC_KEY");
        let anchor = fixture::private_json(
            &fixture.directory.path().join("changed-static.json"),
            &changed,
        )?;
        let mut plan = fixture.plan.clone();
        plan["target"]["static_manifest"] = anchor;
        let path = fixture.directory.path().join("changed-static-plan.json");
        fixture::private_json(&path, &plan)?;
        let original = stopped_bytes(&fixture.target)?;
        let report = fixture.directory.path().join("changed-static-report");
        rejected(
            &invoke(&binary, &fixture, &path, &report, None).await?,
            &report,
        )?;
        ensure!(
            stopped_bytes(&fixture.target)? == original,
            "failed authenticated comparison wrote target"
        );
        // Deliberate corruption is a NEGATIVE fixture only. All positive rows,
        // credentials, grants, configuration and operations came from Service.
        let owner = crate::tenancy::ownership::acquire_path(&fixture.target, false)?;
        use sea_orm::sqlx::{ConnectOptions, Connection};
        let mut db = sea_orm::sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&fixture.target)
            .create_if_missing(false)
            .connect()
            .await?;
        let altered = sea_orm::sqlx::query("UPDATE tenancy_credentials SET key_hash=?")
            .bind(crate::auth::keys::hash_key(
                &fixture.source_provision.secret,
            ))
            .execute(&mut db)
            .await;
        let closed = db.close().await;
        let changed_rows = altered?;
        closed?;
        ensure!(
            changed_rows.rows_affected() == 1,
            "negative fixture did not hit actual credential"
        );
        drop(owner);
        let changed_target = stopped_bytes(&fixture.target)?;
        ensure!(
            changed_target != original,
            "negative credential bytes unchanged"
        );
        let plan = fixture.directory.path().join("copied-credential-plan.json");
        fixture::private_json(&plan, &fixture.plan)?;
        let report = fixture.directory.path().join("copied-credential-report");
        rejected(
            &invoke(&binary, &fixture, &plan, &report, None).await?,
            &report,
        )?;
        ensure!(
            stopped_bytes(&fixture.target)? == changed_target,
            "rejection repaired or removed copied credential"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("negative campaign watchdog")?
}

#[tokio::test]
#[ignore = "real Service/SDK fixture and held actual read transaction; explicit binary/key provenance required"]
async fn abandoned_recovery_waiter_keeps_actual_file_owner_until_read_rollback_and_close()
-> Result<()> {
    let _binary = binary()?;
    tokio::time::timeout(Duration::from_secs(110), async {
        let fixture = Fixture::new().await?;
        let path = fixture.directory.path().join("cancel-plan.json");
        fixture::private_json(&path, &fixture.plan)?;
        let report = fixture.directory.path().join("cancel-report");
        let before = stopped_bytes(&fixture.target)?;
        let held = Hold {
            entered: CancellationToken::new(),
            release: CancellationToken::new(),
        };
        let _release_on_failure = held.release.clone().drop_guard();
        let task_held = held.clone();
        let task_report = report.clone();
        let caller =
            tokio::spawn(
                async move { super::run_inner(&path, &task_report, Some(task_held)).await },
            );
        tokio::time::timeout(Duration::from_secs(10), held.entered.cancelled())
            .await
            .context("real BEGIN did not reach hold")?;
        caller.abort();
        ensure!(caller.await.is_err(), "waiter did not cancel");
        ensure!(
            crate::tenancy::ownership::acquire_path(&fixture.target, false).is_err(),
            "cancelled waiter released original live SQL owner"
        );
        ensure!(
            !report.join("VERIFIED").exists(),
            "held read published evidence"
        );
        held.release.cancel();
        let owner = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(owner) = crate::tenancy::ownership::acquire_path(&fixture.target, false) {
                    break owner;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("actual rollback/worker close did not release owner")?;
        drop(owner);
        ensure!(
            stopped_bytes(&fixture.target)? == before
                && report.join("INCOMPLETE").exists()
                && !report.join("VERIFIED").exists(),
            "cancelled verification changed target or published success"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("cancelled recovery campaign watchdog")?
}
