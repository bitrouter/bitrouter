//! Two real Block H consumers; no fabricated enrollment, intent or receipt.
mod process;

use super::*;
use crate::tenancy::transaction::{Disposition, Phase};
use futures::FutureExt;
use std::io::Write;
use tokio::io::{AsyncBufReadExt, BufReader};

const ROOT: &str = "BITROUTER_BLOCK_FAULT_ROOT";
const MODE: &str = "BITROUTER_BLOCK_FAULT_MODE";
const UPSTREAM: &str = "BITROUTER_BLOCK_FAULT_UPSTREAM";
const READY: &str = "block-fault-original-owner-retained";
const CHILD: &str = "tenancy::tests::execution::native::capture::block::faults::block_fault_child";

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn block_consuming_commit_error_retains_original_owner_and_denies_another_send() -> Result<()>
{
    process::isolated("commit-error").await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn block_lost_deadline_reply_keeps_one_committed_draw_without_provider_send() -> Result<()> {
    process::isolated("late-commit").await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "only the exact bounded Block fault parent invokes this runtime"]
async fn block_fault_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("Block fault root")?);
    let mode = std::env::var(MODE)?;
    ensure!(matches!(mode.as_str(), "commit-error" | "late-commit"));
    let upstream = std::env::var(UPSTREAM)?;
    exercise(&root, &mode, &upstream).await?;
    // All ordinary Host/App/Service/DB/Proof handles have dropped. The parent
    // probes the actual file lease while this SAME unknown owner stays alive.
    println!("\n{READY}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

async fn exercise(root: &Path, mode: &str, upstream: &str) -> Result<()> {
    let mut host = Host::new_in(tempfile::tempdir_in(root)?, Some(upstream)).await?;
    // Keep the real file present after ordinary handles drop. The parent owns
    // the containing TempDir and preserves it on any containment failure.
    host.directory.disable_cleanup(true);
    host.marker = root.join("child.json");
    tokio::fs::write(
        root.join("database.txt"),
        host.database.to_string_lossy().as_bytes(),
    )
    .await?;
    let gate_path = root.join("block.sock");
    let peer_gate = tokio::net::UnixListener::bind(&gate_path)?;
    let checked = std::panic::AssertUnwindSafe(async {
        let native = host.open_block_gate(&gate_path).await?;
        let headers = host.endpoint_headers().await?;
        ensure!(host.records("owned_block_intent_v1").await?.is_empty());
        ensure!(host.records("owned_block_enrollment_v1").await?.is_empty());
        let control_before = host.records("owned_block_control_v1").await?;
        ensure!(control_before.len() == 1);
        if mode == "commit-error" {
            consuming_error(&host).await?;
        }
        let reads = host.fixture.service.reads.available_permits();
        ensure!(reads > 0 && host.fixture.service.current().is_ok());
        let mut prompt = Box::pin(host.current()?.prompt(&native, "one real Block fault turn"));
        let (mut peer, _) = tokio::time::timeout(TIMEOUT, async {
            tokio::select! {
                result = &mut prompt => {
                    result?;
                    anyhow::bail!("actual prompt ended before its HTTP boundary");
                }
                result = peer_gate.accept() => Ok::<_, anyhow::Error>(result?),
            }
        })
        .await??;
        let mut ready = [0; 5];
        tokio::time::timeout(TIMEOUT, peer.read_exact(&mut ready)).await??;
        ensure!(&ready == b"ready");
        // No other operation is submitted between the real prompt append ACK
        // and this HTTP request. Receipt publication follows Entry selection.
        let gate = host.fixture.service.cleanup.hold(Phase::Committing);
        peer.write_all(b"x").await?;
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        ensure!(host.fixture.service.cleanup.disposition() == Disposition::Pending);
        ensure!(host.fixture.service.reads.available_permits() == reads - 1);
        ensure!(host.fixture.service.host_slots_for_test() == 0);
        ensure!(
            host.fixture
                .service
                .admission
                .wait_drained()
                .now_or_never()
                .is_none(),
            "pending Block transaction returned its admission lease"
        );
        ensure!(
            host.fixture
                .service
                .admission
                .receipt_capacity_for_test()
                .map_err(|error| anyhow::anyhow!("{error:?}"))?
                .0
                > 0
        );
        // These use real public facades and the configured endpoint. Neither
        // can turn the held selection into another owner or provider attempt.
        refuse_opener(&host).await?;
        refuse_http(&host, headers.clone(), false).await?;
        ensure!(host.fixture.service.cleanup.disposition() == Disposition::Pending);
        if mode == "late-commit" {
            // Advance beyond the ORIGINAL five-second selection/H deadline,
            // not a replacement test limit. The consuming COMMIT stays held.
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(6)).await;
            tokio::time::resume();
            tokio::time::timeout(TIMEOUT, host.fixture.service.cleanup.wait_fenced()).await?;
            ensure!(host.fixture.service.current().is_err());
            ensure!(host.fixture.service.cleanup.disposition() == Disposition::Pending);
            ensure!(host.fixture.service.reads.available_permits() == reads - 1);
        }
        // Dropping this public waiter cannot cancel the real registered owner.
        drop(prompt);
        host.fixture.service.cleanup.release_gate();
        tokio::time::timeout(TIMEOUT, host.fixture.service.cleanup.wait_fenced()).await?;
        // A real pool read waits behind the same consuming COMMIT/driver
        // rollback. It does not manufacture a transaction acknowledgement.
        let intents =
            tokio::time::timeout(TIMEOUT, host.records("owned_block_intent_v1")).await??;
        let enrollments = host.records("owned_block_enrollment_v1").await?;
        let control = host.records("owned_block_control_v1").await?;
        if mode == "commit-error" {
            ensure!(intents.is_empty() && enrollments.is_empty());
            ensure!(
                control == control_before,
                "consuming error left a partial Block write"
            );
            ensure!(host.fixture.service.reads.available_permits() == reads - 1);
        } else {
            ensure!(
                intents.len() == 1 && enrollments.len() == 1,
                "late commit redrew or lost the original selection"
            );
            ensure!(control.len() == 1 && control != control_before);
            ensure!(intents[0]["native"] == native);
            ensure!(enrollments[0]["value"]["identity"]["native_session_id"] == native);
            ensure!(matches!(
                intents[0]["intent"]["selected_route"].as_str(),
                Some("mock:model-a" | "mock:model-b")
            ));
            ensure!(intents[0]["intent"]["request_id"].as_str().is_some());
            // The original consuming COMMIT has now acknowledged. Its failed
            // HTTP pipeline may drain only after its own durable no-send receipt.
            tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
            acknowledged_no_send_receipt(&host, &intents[0]).await?;
        }
        refuse_opener(&host).await?;
        refuse_http(&host, headers, true).await?;
        ensure!(host.records("owned_block_intent_v1").await? == intents);
        ensure!(host.records("owned_block_enrollment_v1").await? == enrollments);
        ensure!(host.records("owned_block_control_v1").await? == control);
        Ok(())
    })
    .catch_unwind()
    .await;
    // Release the one-shot seam even on an assertion/unwind. This is not a
    // cleanup ACK: original H and Registry must still report unknown custody.
    host.fixture.service.cleanup.release_gate();
    host.stop.cancel();
    drop(host.session.take());
    let checked = match checked {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("Block fault observation unwound")),
    };
    let custody = async {
        tokio::time::timeout(TIMEOUT, host.fixture.service.cleanup.quarantined()).await?;
        ensure!(host.fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        ensure!(host.fixture.service.current().is_err());
        ensure!(
            tokio::time::timeout(TIMEOUT, host.fixture.service.shutdown())
                .await?
                .is_err()
        );
        ensure!(
            host.fixture
                .service
                .completion
                .clone()
                .now_or_never()
                .is_none()
        );
        ensure!(host.fixture.service.host_slots_for_test() == 0);
        if mode == "commit-error" {
            ensure!(
                host.fixture
                    .service
                    .admission
                    .wait_drained()
                    .now_or_never()
                    .is_none(),
                "unknown pipeline returned its admission lease"
            );
            ensure!(
                host.fixture
                    .service
                    .admission
                    .receipt_capacity_for_test()
                    .map_err(|error| anyhow::anyhow!("{error:?}"))?
                    .0
                    > 0,
                "unknown receipt returned its reservation"
            );
        } else {
            // Independent capture cleanup can remain unknown after this exact
            // provider pipeline and receipt have already acknowledged completion.
            ensure!(
                host.fixture
                    .service
                    .admission
                    .wait_drained()
                    .now_or_never()
                    .is_some()
            );
            ensure!(
                host.fixture
                    .service
                    .admission
                    .receipt_capacity_for_test()
                    .map_err(|error| anyhow::anyhow!("{error:?}"))?
                    .0
                    == 0
            );
        }
        tokio::time::timeout(TIMEOUT, host.fixture.db.clone().close()).await??;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // Listener cancellation only contains the isolated runtime's sockets. It
    // is deliberately not counted as a production clean-drain acknowledgement.
    drop(host.listener.take());
    drop(host);
    checked.and(custody)
}

async fn acknowledged_no_send_receipt(host: &Host, intent: &Value) -> Result<()> {
    use bitrouter_ai::tenancy::{
        execution::{Outcome, ReceiptState},
        operation::DispatchState,
    };
    let id = intent["intent"]["request_id"]
        .as_str()
        .context("original Block execution")?
        .try_into()?;
    let candidate = crate::tenancy::receipts::store::lookup_candidate(
        &host.fixture.db,
        &host.fixture.installation,
        &host.fixture.tenant,
        &id,
    )
    .await?
    .context("late Block pipeline requires its original durable receipt")?;
    let receipt = &candidate.receipt;
    ensure!(
        receipt.execution_id == id
            && receipt.principal_id == host.fixture.principal
            && receipt.correlation_id.as_str()
                == intent["correlation"]
                    .as_str()
                    .context("original correlation")?
            && receipt.outcome == Outcome::Failed
            && receipt.state == ReceiptState::Incomplete
            && receipt.attempts.is_empty()
            && !receipt.usage.complete
    );
    ensure!(candidate.experiment_disposition()?.1 == DispatchState::NotStarted);
    let count = host.fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT COUNT(*) AS count FROM tenancy_execution_receipts WHERE installation_id=? AND tenant_id=?",
        [host.fixture.installation.as_str().into(), host.fixture.tenant.as_str().into()])).await?.context("receipt count")?;
    ensure!(
        count.try_get::<i64>("", "count")? == 1,
        "late acknowledgement duplicated its receipt"
    );
    Ok(())
}

async fn consuming_error(host: &Host) -> Result<()> {
    // Only the real Block intent insert triggers this deferred constraint.
    // DML succeeds; SQLite's actual consuming COMMIT must reject it.
    for sql in [
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE block_fault_parent (id INTEGER PRIMARY KEY)",
        "CREATE TABLE block_fault_child (parent INTEGER REFERENCES block_fault_parent(id) DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TRIGGER block_fault_commit AFTER INSERT ON checkpoint_evolution WHEN NEW.kind='owned_block_intent_v1' BEGIN INSERT INTO block_fault_child(parent) VALUES (77); END",
    ] {
        host.fixture.db.execute_unprepared(sql).await?;
    }
    let row = host
        .fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA foreign_keys",
        ))
        .await?
        .context("foreign key setting")?;
    ensure!(row.try_get::<i64>("", "foreign_keys")? == 1);
    Ok(())
}

async fn refuse_opener(host: &Host) -> Result<()> {
    let marker = host.directory.path().join("must-not-open.json");
    let listener = host.listener.as_ref().context("actual listener")?;
    let app = Arc::new(
        host_app(
            &host.fixture,
            "http://127.0.0.1:1",
            Arc::new(HostSpies::default()),
        )?
        .with_tenancy(host.fixture.service.clone()),
    );
    let result = tokio::time::timeout(
        TIMEOUT,
        ControlledSession::open_authenticated(
            app,
            host.headers.as_ref().context("original headers")?.clone(),
            launch(&executable()?, &marker, &listener.origin, None),
            CancellationToken::new(),
        ),
    )
    .await?;
    ensure!(
        result.is_err(),
        "held original capture admitted another opener"
    );
    ensure!(!marker.exists() && host.fixture.service.host_slots_for_test() == 0);
    Ok(())
}

async fn refuse_http(host: &Host, headers: HeaderMap, fenced: bool) -> Result<()> {
    let response = reqwest::Client::builder().timeout(TIMEOUT).build()?
        .post(format!("{}/v1/chat/completions", host.listener.as_ref().context("actual listener")?.origin))
        .headers(headers).json(&json!({"model":"literal:subject","messages":[{"role":"user","content":"must not redraw"}]})).send().await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    ensure!(bytes.len() <= 8192);
    let error: bitrouter_ai::tenancy::operation::Error = serde_json::from_slice(&bytes)?;
    if fenced {
        ensure!(
            status == StatusCode::SERVICE_UNAVAILABLE
                && error.code == ErrorCode::AuthorityUnavailable
        );
    } else {
        ensure!(
            status == StatusCode::TOO_MANY_REQUESTS && error.code == ErrorCode::CapacityExceeded
        );
    }
    Ok(())
}
