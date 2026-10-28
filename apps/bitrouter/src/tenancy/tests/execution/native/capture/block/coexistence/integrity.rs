//! Negative mutations occur ONLY after the actual selected paid producer.
mod process;
use super::*;
use std::io::Write;
use tokio::io::{AsyncBufReadExt, BufReader};

const ROOT: &str = "BITROUTER_BLOCK_PAID_INTEGRITY_ROOT";
const MODE: &str = "BITROUTER_BLOCK_PAID_INTEGRITY_MODE";
const READY: &str = "block-paid-integrity-refused-owner-contained";
const CHILD: &str = "tenancy::tests::execution::native::capture::block::coexistence::integrity::block_paid_integrity_child";

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn actual_selected_paid_block_rejects_missing_job_changed_output_and_missing_judge_receipt()
-> Result<()> {
    for mode in ["job", "output", "receipt"] {
        process::isolated(mode).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "only the exact bounded Block paid-integrity parent invokes this runtime"]
async fn block_paid_integrity_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("integrity root")?);
    let mode = std::env::var(MODE)?;
    ensure!(matches!(mode.as_str(), "job" | "output" | "receipt"));
    exercise(&root, &mode).await?;
    println!("\n{READY}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

// Exact raw bytes/metadata; never a decoder that could hide a rewritten row.
// All tables are an isolated tiny fixture. LIMIT + cumulative bytes additionally
// bound this observation, independently of production's unchanged H budgets.
async fn snapshot(host: &Host) -> Result<Vec<(String, Vec<String>)>> {
    let projections = [
        (
            "checkpoint_evolution",
            "record_id,scope_id,kind,session_key,revision,body,installation_id,tenant_id,creator_principal_id",
            "record_id",
        ),
        (
            "acp_capture_connections",
            "connection_id,owner,source,controller_instance_id,route_scope_id,state,head,started_at,metadata_json,installation_id,tenant_id,creator_principal_id",
            "connection_id",
        ),
        (
            "acp_canonical_sessions",
            "session_key,owner,source,native_session_id,head,history_origin,deleted,parent_key,parent_watermark,installation_id,tenant_id,creator_principal_id",
            "session_key",
        ),
        (
            "acp_capture_events",
            "connection_id,sequence,session_key,session_sequence,replay,event_json,captured_at,installation_id,tenant_id,creator_principal_id",
            "connection_id,sequence",
        ),
        (
            "acp_checkpoints",
            "checkpoint_id,session_key,watermark,manifest_json,deleted,installation_id,tenant_id,creator_principal_id",
            "checkpoint_id",
        ),
        (
            "acp_checkpoint_members",
            "checkpoint_id,session_key,installation_id,tenant_id,creator_principal_id",
            "checkpoint_id,session_key",
        ),
        (
            "acp_assessment_revisions",
            "revision_id,session_key,checkpoint_id,created_at,revision_json,installation_id,tenant_id,creator_principal_id",
            "revision_id",
        ),
        (
            "acp_effective_assessments",
            "session_key,revision_id,installation_id,tenant_id,creator_principal_id",
            "session_key",
        ),
        (
            "tenancy_execution_receipts",
            "installation_id,tenant_id,execution_id,principal_id,correlation_id,receipt_json,content_digest,disposition_json,byte_count,unknown_outcome",
            "execution_id",
        ),
    ];
    let mut result = Vec::new();
    let mut bytes = 0_usize;
    for (table, columns, order) in projections {
        let rows = host
            .fixture
            .db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!(
                    "SELECT json_array({columns}) AS raw FROM {table} ORDER BY {order} LIMIT 513"
                ),
            ))
            .await?;
        ensure!(rows.len() <= 512);
        let mut values = Vec::new();
        for row in rows {
            let value: String = row.try_get("", "raw")?;
            bytes = bytes
                .checked_add(value.len())
                .context("snapshot overflow")?;
            ensure!(bytes <= 8 * 1024 * 1024);
            values.push(value);
        }
        result.push((table.to_owned(), values));
    }
    Ok(result)
}

async fn corrupt(host: &Host, mode: &str, job: &Value) -> Result<()> {
    let id = job["job"].as_str().context("actual paid job")?;
    let actual = match mode {
        "job" => host.fixture.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "DELETE FROM checkpoint_evolution WHERE record_id=? AND kind='owned_judge_job' AND installation_id=? AND tenant_id=? AND creator_principal_id=?",
            [id.into(), host.fixture.installation.as_str().into(), host.fixture.tenant.as_str().into(), host.fixture.principal.as_str().into()])).await?,
        "output" => {
            // Preserve the selected assessment/history/head/observed digest.
            // Only the original retained paid output is changed.
            let mut changed = job.clone();
            changed["output"]["evaluation"]["summary"] = json!("Changed retained judge output, not the selected rubric");
            // Serialize through the same typed Record is deliberately unavailable
            // to this caller; use an exact string replacement below so all other
            // stored bytes, order and metadata remain unchanged.
            let row = host.fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT body FROM checkpoint_evolution WHERE record_id=? AND kind='owned_judge_job'", [id.into()])).await?.context("actual terminal")?;
            let before: String = row.try_get("", "body")?;
            ensure!(before.len() <= 1024*1024);
            let old = serde_json::to_string(job["output"]["evaluation"]["summary"].as_str().context("summary")?)?;
            let replacement = serde_json::to_string(changed["output"]["evaluation"]["summary"].as_str().context("changed summary")?)?;
            ensure!(before.matches(&old).count() == 1, "one exact original output summary");
            let after = before.replacen(&old, &replacement, 1);
            ensure!(serde_json::from_str::<Value>(&after)? == changed);
            host.fixture.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "UPDATE checkpoint_evolution SET body=? WHERE record_id=? AND kind='owned_judge_job' AND installation_id=? AND tenant_id=? AND creator_principal_id=? AND body=?",
                [after.into(),id.into(),host.fixture.installation.as_str().into(),host.fixture.tenant.as_str().into(),host.fixture.principal.as_str().into(),before.into()])).await?
        },
        "receipt" => host.fixture.db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "DELETE FROM tenancy_execution_receipts WHERE installation_id=? AND tenant_id=? AND execution_id=?",
            [host.fixture.installation.as_str().into(),host.fixture.tenant.as_str().into(),job["execution"].as_str().context("original paid execution")?.into()])).await?,
        _ => anyhow::bail!("unknown negative fixture"),
    };
    ensure!(
        actual.rows_affected() == 1,
        "negative fixture did not affect its exact original row"
    );
    Ok(())
}

async fn exercise(root: &Path, mode: &str) -> Result<()> {
    let judge = Judge::new(false).await?;
    let made = Host::configured_in(
        tempfile::tempdir_in(root)?,
        Options {
            assessment: Some((&judge.url, AssessmentSource::Agentic)),
            ..Default::default()
        },
    )
    .await;
    let mut host = match made {
        Ok(host) => host,
        Err(error) => {
            judge.stop().await?;
            return Err(error);
        }
    };
    host.directory.disable_cleanup(true);
    host.marker = root.join("child.json");
    tokio::fs::write(
        root.join("database.txt"),
        host.database.to_string_lossy().as_bytes(),
    )
    .await?;
    let checked = std::panic::AssertUnwindSafe(async {
        let native = host.open().await?;
        let current = host.current()?;
        current
            .prompt(&native, "Reply with exactly native reply; no tools")
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        current.checkpoint(&native).await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        current.checkpoint(&native).await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let selected = control(&host).await?;
        ensure!(
            observation(&selected)?["quality"] == 1.0 && host.fixture.service.current().is_ok()
        );
        let view = current.review_checkpoint(&native, None).await?;
        ensure!(
            view.history.len() == 1
                && view.history[0].input.source == AssessmentSource::Agentic
                && view.history[0].selected_on_submission
        );
        let facts = original_facts(&host).await?;
        let jobs = paid_rows(&host, "owned_judge_job").await?;
        ensure!(jobs.len() == 1 && jobs[0]["status"] == "completed");
        one_budget(&host, &judge).await?;
        let before = snapshot(&host).await?;
        corrupt(&host, mode, &jobs[0]).await?;
        let damaged = snapshot(&host).await?;
        for ((name, old), (new_name, new)) in before.iter().zip(&damaged) {
            ensure!(name == new_name);
            let affected = if mode == "receipt" {
                "tenancy_execution_receipts"
            } else {
                "checkpoint_evolution"
            };
            if name == affected {
                ensure!(old != new);
            } else {
                ensure!(old == new, "negative fixture changed {name}");
            }
        }
        ensure!(
            current.checkpoint(&native).await.is_err(),
            "unverified Agentic source reached Block publication"
        );
        ensure!(
            host.fixture.service.current().is_err(),
            "integrity loss was not fenced"
        );
        ensure!(
            snapshot(&host).await? == damaged,
            "failed refresh repaired or published rows"
        );
        ensure!(
            control(&host).await? == selected,
            "failed refresh changed durable control"
        );
        ensure!(
            receipt(
                &host,
                facts.0["execution_id"]
                    .as_str()
                    .context("subject execution")?
            )
            .await?
                == facts.0
        );
        if mode != "receipt" {
            ensure!(
                receipt(
                    &host,
                    facts.1["execution_id"]
                        .as_str()
                        .context("judge execution")?
                )
                .await?
                    == facts.1
            );
        }
        one_budget(&host, &judge).await?;
        ensure!(
            host.upstream
                .received_requests()
                .await
                .context("provider")?
                .len()
                == 1
        );
        Ok(())
    })
    .catch_unwind()
    .await;
    // Integrity rejection is a confirmed SQL rollback, not a claim that this
    // fenced live host's disconnect could subsequently be acknowledged. Isolate
    // and retain it; the parent reaps both processes without fabricating cleanup.
    host.stop.cancel();
    drop(host.session.take());
    let checked = match checked {
        Ok(value) => value,
        Err(_) => Err(anyhow::anyhow!("integrity consumer unwound")),
    };
    let custody = async {
        ensure!(
            tokio::time::timeout(TIMEOUT, host.fixture.service.shutdown())
                .await?
                .is_err()
        );
        ensure!(host.fixture.service.host_slots_for_test() == 0);
        tokio::time::timeout(TIMEOUT, host.fixture.db.clone().close()).await??;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = judge.stop().await;
    drop(host.listener.take());
    drop(host);
    checked.and(custody).and(stopped)
}
