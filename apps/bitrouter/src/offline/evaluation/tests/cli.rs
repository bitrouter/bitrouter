//! Explicit built-binary consumer using the real stopped Service/SDK fixture.
use super::*;
use std::{ffi::OsStr, io::Read, process::Stdio};
use tokio::io::AsyncReadExt;

const BINARY_ENV: &str = "BITROUTER_TEST_BRO";
const OUTPUT_LIMIT: usize = 64 * 1024;
const FILE_LIMIT: u64 = 32 * 1024 * 1024;

fn binary() -> Result<PathBuf> {
    let path = PathBuf::from(
        std::env::var_os(BINARY_ENV)
            .context("set BITROUTER_TEST_BRO to the explicitly built Core-enabled bro binary")?,
    );
    ensure!(path.is_absolute(), "BITROUTER_TEST_BRO must be absolute");
    let path = path.canonicalize()?;
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "BITROUTER_TEST_BRO is not an executable file"
    );
    Ok(path)
}

fn arguments(action: &str, fields: &[(&str, &OsStr)]) -> Vec<std::ffi::OsString> {
    let mut args = ["--json", "tenancy", "offline", action]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect::<Vec<_>>();
    for (flag, value) in fields {
        args.push((*flag).into());
        args.push((*value).into());
    }
    args
}

fn no_secrets(fixture: &Fixture, stdout: &[u8], stderr: &[u8]) -> Result<()> {
    for secret in
        std::iter::once(OPERATOR).chain(fixture.provision.secrets.iter().map(String::as_str))
    {
        ensure!(!secret.is_empty(), "fixture secret missing");
        ensure!(
            !stdout
                .windows(secret.len())
                .any(|bytes| bytes == secret.as_bytes())
                && !stderr
                    .windows(secret.len())
                    .any(|bytes| bytes == secret.as_bytes()),
            "built CLI exposed a fixture credential"
        );
    }
    Ok(())
}

async fn invoke(
    binary: &Path,
    fixture: &Fixture,
    args: &[std::ffi::OsString],
) -> Result<std::process::Output> {
    let mut child = tokio::process::Command::new(binary)
        .args(args)
        .current_dir(fixture.directory.path())
        .env("BITROUTER_HOME", fixture.directory.path())
        .env("RUST_LOG", "error")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().context("CLI stdout")?;
    let mut stderr = child.stderr.take().context("CLI stderr")?;
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        let (_, _, status) = tokio::try_join!(
            async {
                (&mut stdout)
                    .take(OUTPUT_LIMIT as u64 + 1)
                    .read_to_end(&mut output)
                    .await?;
                ensure!(output.len() <= OUTPUT_LIMIT, "CLI stdout bound");
                Ok::<_, anyhow::Error>(())
            },
            async {
                (&mut stderr)
                    .take(OUTPUT_LIMIT as u64 + 1)
                    .read_to_end(&mut errors)
                    .await?;
                ensure!(errors.len() <= OUTPUT_LIMIT, "CLI stderr bound");
                Ok::<_, anyhow::Error>(())
            },
            async { child.wait().await.map_err(anyhow::Error::from) }
        )?;
        Ok::<_, anyhow::Error>(status)
    })
    .await;
    if !matches!(&outcome, Ok(Ok(_))) {
        let _ = child.start_kill();
        // A timed-out/overflowing CLI is a failure even after a successful reap.
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("CLI failed to exit after kill")??;
    }
    // Check partial output before including any diagnostics in a failure.
    no_secrets(fixture, &output, &errors)?;
    let diagnostic = || {
        format!(
            "CLI stdout ({} bytes):\n{}\nCLI stderr ({} bytes):\n{}",
            output.len(),
            String::from_utf8_lossy(&output),
            errors.len(),
            String::from_utf8_lossy(&errors)
        )
    };
    let status = outcome
        .map_err(|error| anyhow::anyhow!("CLI watchdog elapsed: {error}; {}", diagnostic()))?
        .map_err(|error| anyhow::anyhow!("CLI output/wait failed: {error:#}; {}", diagnostic()))?;
    Ok(std::process::Output {
        status,
        stdout: output,
        stderr: errors,
    })
}

fn success(output: &std::process::Output) -> Result<Value> {
    ensure!(
        output.status.code() == Some(0),
        "CLI status={}; stdout={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn rejected(output: &std::process::Output) -> Result<()> {
    ensure!(
        output.status.code() == Some(1),
        "CLI rejection exit={}",
        output.status
    );
    let report: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        report["error"]["message"]
            == "offline Eval backfill failed or outcome is unconfirmed: retain the incomplete packet and stopped destination for reconciliation; do not retry or activate",
        "CLI did not reach the offline backfill consumer"
    );
    Ok(())
}

fn bounded_bytes(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() <= FILE_LIMIT,
        "CLI fixture file bound"
    );
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= FILE_LIMIT,
        "CLI fixture file grew beyond bound"
    );
    Ok(bytes)
}

fn stopped_bytes(path: &Path) -> Result<Vec<u8>> {
    let owner = crate::tenancy::ownership::acquire_path(path, false)?;
    let bytes = bounded_bytes(path)?;
    drop(owner);
    Ok(bytes)
}

fn backfill_arguments(plan: &Path, anchor: &str, report: &Path) -> Vec<std::ffi::OsString> {
    arguments(
        "eval-backfill",
        &[
            ("--plan", plan.as_os_str()),
            ("--plan-digest", OsStr::new(anchor)),
            ("--report", report.as_os_str()),
        ],
    )
}

#[tokio::test]
#[ignore = "actual built bro CLI; requires explicit BITROUTER_TEST_BRO, no nested build"]
async fn built_cli_inventory_backfill_rejections_and_real_scoped_readers() -> Result<()> {
    let binary = binary()?;
    tokio::time::timeout(TIMEOUT, async {
        let mut fixture = Fixture::new().await?;
        let source_before = stopped_bytes(&fixture.source)?;
        let bundle_before = bounded_bytes(&fixture.bundle.join("snapshot.sqlite"))?;
        let evidence_before = bounded_bytes(&fixture.bundle.join("evidence.json"))?;
        let mapping_path = PathBuf::from(fixture.plan["manifest"].as_str().context("mapping path")?);
        let mapping_before = bounded_bytes(&mapping_path)?;
        let target_before = stopped_bytes(&fixture.target)?;

        // Invoke the executable adapter with exactly the same anchored evidence
        // produced by the real stopped connector/Service fixture.
        let inventory_output = fixture.directory.path().join("cli-partitions");
        let args = arguments("eval-inventory", &[
            ("--bundle", fixture.bundle.as_os_str()),
            ("--evidence-digest", OsStr::new(fixture.plan["database_evidence_digest"].as_str().context("source anchor")?)),
            ("--source-kind", OsStr::new("local_user")),
            ("--report", inventory_output.as_os_str()),
        ]);
        let report = success(&invoke(&binary, &fixture, &args).await?)?;
        ensure!(report["format"] == "bitrouter.offline.eval-partition.v1"
            && report["eval_rows"] == 9 && report["owner_partitions"] == 3
            && report["complete"] == false && report["cutover_ready"] == false);
        let packet_bytes = files::read(&inventory_output.join("partitions.json"), 2 * 1024 * 1024)?;
        ensure!(report["packet_digest"] == files::sha(&packet_bytes));
        ensure!(files::read(&inventory_output.join("RECORDED"), 128)?
            == format!("{}\n", files::sha(&packet_bytes)).as_bytes());
        let packet: Value = serde_json::from_slice(&packet_bytes)?;
        let original: Value = serde_json::from_slice(&files::read(
            &fixture.directory.path().join("partitions/partitions.json"), 2 * 1024 * 1024,
        )?)?;
        ensure!(packet == original, "binary inventory changed exact source partitions");
        ensure!(inventory_output.join("RECORDED").exists() && !inventory_output.join("INCOMPLETE").exists());

        // Change only the operator mapping count, and bind the altered bytes in
        // a distinct plan. A parser/anchor failure cannot be the positive control.
        let original_plan = fixture.plan.clone();
        let mut mapping: Value = serde_json::from_slice(&mapping_before)?;
        let entry = mapping["mappings"].as_array_mut().context("mappings")?
            .iter_mut().find(|entry| entry["old_owner"] == "owner").context("named owner")?;
        ensure!(entry["source_count"] == 4);
        entry["source_count"] = json!(5);
        let wrong_mapping = fixture.directory.path().join("cli-wrong-mapping.json");
        let wrong_bytes = serde_json::to_vec(&mapping)?;
        files::write_new(&wrong_mapping, &wrong_bytes)?;
        fixture.plan["manifest"] = json!(wrong_mapping);
        fixture.plan["manifest_digest"] = json!(files::sha(&wrong_bytes));
        let (plan, anchor, report) = fixture.write_plan("cli-wrong-count")?;
        let output = invoke(&binary, &fixture, &backfill_arguments(&plan, &anchor, &report)).await?;
        rejected(&output)?;
        ensure!(!report.exists(), "mapping preflight reached destination DML");
        ensure!(stopped_bytes(&fixture.target)? == target_before);

        fixture.plan = original_plan;
        let (plan, anchor, report) = fixture.write_plan("cli-apply")?;
        let plan_before = bounded_bytes(&plan)?;
        let output = invoke(&binary, &fixture, &backfill_arguments(&plan, &anchor, &report)).await?;
        let imported = success(&output)?;
        ensure!(imported["format"] == "bitrouter.offline.eval-backfill-report.v1"
            && imported["imported_counts"] == json!([2,2,2,2]) && imported["provenance_rows"] == 2
            && imported["backfill_applied"] == true && imported["complete"] == false
            && imported["cutover_ready"] == false);
        ensure!(imported["quarantined_eval"].as_array().context("quarantine")?.len() == 1);
        let committed_bytes = files::read(&report.join("report.json"), 2 * 1024 * 1024)?;
        let committed_report: Value = serde_json::from_slice(&committed_bytes)?;
        ensure!(imported == committed_report);
        ensure!(files::read(&report.join("RECORDED"), 128)?
            == format!("{}\n", files::sha(&committed_bytes)).as_bytes());
        ensure!(report.join("RECORDED").exists() && !report.join("INCOMPLETE").exists());
        ensure!(bounded_bytes(&plan)? == plan_before && bounded_bytes(&mapping_path)? == mapping_before);

        // The same exact plan cannot overwrite an already populated target.
        // This is a fresh report path, so output-directory reuse cannot mask it.
        let target_imported = stopped_bytes(&fixture.target)?;
        ensure!(target_imported != target_before, "successful CLI imported no rows");
        let repeat_report = fixture.directory.path().join("cli-repeat");
        let output = invoke(&binary, &fixture, &backfill_arguments(&plan, &anchor, &repeat_report)).await?;
        rejected(&output)?;
        ensure!(repeat_report.join("INCOMPLETE").exists() && !repeat_report.join("RECORDED").exists());
        ensure!(stopped_bytes(&fixture.target)? == target_imported);
        ensure!(stopped_bytes(&fixture.source)? == source_before);
        ensure!(bounded_bytes(&fixture.bundle.join("snapshot.sqlite"))? == bundle_before);
        ensure!(bounded_bytes(&fixture.bundle.join("evidence.json"))? == evidence_before);
        ensure!(bounded_bytes(&mapping_path)? == mapping_before && bounded_bytes(&plan)? == plan_before);

        // No fixture credential is passed in CLI arguments or environment values.
        // The actual restarted Service and SDK perform owner-bound reads here.
        let (live, _) = Live::open(&fixture.target, false).await?;
        let checked = async {
            for index in 0..2 {
                let tenant: TenantId = format!("target-{index}").try_into()?;
                let client = TenancyClient::new(&live.base, &fixture.provision.secrets[index])?;
                let status = client.eval_status(&fixture.provision.installation, &tenant).await?;
                ensure!(status.subjects == 1 && status.results == 1);
                ensure!(client.eval_subject(&fixture.provision.installation, &tenant, &subject(index)?.eval_id).await? == subject(index)?);
                ensure!(client.eval_snapshot(&fixture.provision.installation, &tenant, &fixture.snapshots[index].evidence_root).await? == fixture.snapshots[index]);
                ensure!(client.eval_snapshot(&fixture.provision.installation, &tenant, &fixture.snapshots[1-index].evidence_root).await.is_err());
                let row = live.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                    "SELECT manifest_json FROM eval_snapshots WHERE installation_id=? AND tenant_id=?",
                    [fixture.provision.installation.as_str().into(), tenant.as_str().into()],
                )).await?.context("actual imported manifest")?;
                ensure!(row.try_get::<String>("", "manifest_json")? == fixture.manifests[index], "CLI rewrote original manifest bytes");
                ensure!(client.eval_status(&fixture.provision.installation, &tenant).await?.results == 1);
            }
            let legacy = crate::core::eval::store::EvalStore::new(live.db.clone());
            ensure!(legacy.list_subjects().await?.is_empty());
            ensure!(legacy.snapshot_by_root(&fixture.snapshots[0].evidence_root).await?.is_none());
            Ok::<_, anyhow::Error>(())
        }.await;
        let closed = live.close(&fixture.target).await;
        checked?;
        closed?;
        Ok::<_, anyhow::Error>(())
    }).await?
}
