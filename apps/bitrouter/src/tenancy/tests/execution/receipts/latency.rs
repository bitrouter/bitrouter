//! Authenticated receipts and a real bounded OTLP receiver, in an env-isolated
//! child. No proof, owner, attempt, grant, receipt or inventory is fabricated.
//! The escalation case passes an explicit age to real restored accounting; it
//! never rewrites receipt timestamps or changes the production clock.
use super::*;
use crate::telemetry::{
    config::OtelConfig,
    exporter::OtelExporter,
    receipt_latency::{Config, Sink},
};
use crate::tenancy::transaction::{Disposition, Phase};
use bitrouter_ai::tenancy::execution::Outcome;
use sea_orm::ConnectionTrait;
use std::process::{Child, Command, Stdio};
#[cfg(feature = "otel-http")]
pub(in crate::tenancy::tests::execution) mod callback_schedule;
mod capacity;
pub(in crate::tenancy::tests::execution) mod collector;
mod escalation;
use collector::Collector;
const CHILD: &str = "BITROUTER_TEST_RECEIPT_LATENCY_CHILD";
const MARKER: &str = "BITROUTER_TEST_RECEIPT_LATENCY_MARKER";
const COMPLETED: &[u8] = b"receipt-latency-capacity-cases-complete\n";

struct ChildOwner(Child);
impl Drop for ChildOwner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn actual_receipt_latency_transport_and_lifecycle() -> Result<()> {
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir()?;
        let marker = directory.path().join("complete");
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["--exact","tenancy::tests::execution::receipts::latency::actual_receipt_latency_transport_and_lifecycle","--nocapture","--test-threads=1"]);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("OTEL_") || name.starts_with("BITROUTER_TELEMETRY_") {
                command.env_remove(key);
            }
        }
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(key);
        }
        let mut child = ChildOwner(
            command
                .env(CHILD, "1")
                .env(MARKER, &marker)
                .env(
                    "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE",
                    "cumulative",
                )
                .stdin(Stdio::null())
                .spawn()?,
        );
        let end = std::time::Instant::now() + Duration::from_secs(110);
        loop {
            if let Some(status) = child.0.try_wait()? {
                ensure!(status.success(), "receipt latency child failed: {status}");
                ensure!(
                    std::fs::metadata(&marker)?.len() == COMPLETED.len() as u64,
                    "missing exact child completion"
                );
                ensure!(
                    std::fs::read(&marker)? == COMPLETED,
                    "zero-match or incomplete child"
                );
                return Ok(());
            }
            ensure!(
                std::time::Instant::now() < end,
                "receipt latency child exceeded watchdog"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(async {
            positive_after_run_export("").await?;
            #[cfg(feature = "otel-http")]
            for endpoint_path in ["/", "/v1/metrics", "/v1/metrics?source=receipt-test"] {
                positive_after_run_export(endpoint_path).await?;
            }
            queued_and_sql_boundaries().await?;
            rollback_retry_keeps_original_ready().await?;
            caller_disconnect_does_not_ack_or_reset().await?;
            receiver_failure_is_cached().await?;
            uncertain_commit_is_not_success(false).await?;
            uncertain_commit_is_not_success(true).await?;
            capacity::durable_unknown_and_incomplete_restart().await?;
            #[cfg(feature = "otel-http")]
            callback_schedule::real_callbacks_contend_and_straddle_seal().await?;
            capacity::queued_cancel_and_unknown_length_ingress().await?;
            capacity::aggregate_cross_tenant_capacity().await?;
            #[cfg(unix)]
            capacity::sealed_gauges_do_not_retain_file_owner().await?;
            Ok::<_, anyhow::Error>(())
        })?;
    use std::io::Write;
    let marker = std::env::var_os(MARKER).context("child completion path")?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)?;
    file.write_all(COMPLETED)?;
    file.sync_all()?;
    Ok(())
}

pub(in crate::tenancy::tests::execution) fn exporter(
    collector: &Collector,
    interval: u64,
    selector: Option<&str>,
) -> Result<(Arc<OtelExporter>, Sink)> {
    let mut app = crate::config::Config::default();
    app.tenancy.enabled = true;
    app.plugins.insert("bitrouter-telemetry".into(),json!({"otel":{"endpoint":collector.endpoint},"receipt_latency":{"enabled":true,"correlation_id":selector}}));
    let observer = Config::read(&app)?;
    let mut config = OtelConfig {
        endpoint: collector.endpoint.clone(),
        sampler: crate::telemetry::config::SamplerKind::AlwaysOff,
        ..Default::default()
    };
    config.metrics.export_interval_ms = interval;
    config.traces.batch.flush_ms = 3_600_000;
    let exporter = Arc::new(
        OtelExporter::new(config, None).map_err(|_| anyhow::anyhow!("exporter initialization"))?,
    );
    let sink = exporter.receipt_observer(&observer)?;
    Ok((exporter, sink))
}
async fn harness(sink: Sink, gate: Option<Arc<Semaphore>>) -> Result<Harness> {
    Harness::with_observer(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        gate,
        false,
        sink,
    )
    .await
}
pub(in crate::tenancy::tests::execution) async fn flush(
    exporter: &Arc<OtelExporter>,
) -> Result<bool> {
    let owner = exporter.clone();
    let report = tokio::time::timeout(
        TIMEOUT,
        tokio::task::spawn_blocking(move || owner.shutdown_recorded(true)),
    )
    .await??;
    ensure!(report.is_clean() == exporter.recorded_shutdown_is_clean());
    Ok(report.is_clean())
}
async fn completed(h: &Harness) -> Result<ExecutionId> {
    let response = h.send(&Harness::client(false)?).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    ensure!(!response.bytes().await?.is_empty());
    let receipt = h.receipt(&Harness::client(false)?, &id).await?;
    ensure!(
        receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed
            && receipt.outcome == Outcome::Completed
            && receipt.usage.complete
            && receipt.attempts.len() == 1
    );
    Ok(id)
}
async fn positive_after_run_export(endpoint_path: &str) -> Result<()> {
    let mut collector = Collector::start().await?;
    // Exercise the real SDK transport with both a collector origin and an
    // explicit signal route; the receiver still accepts only /v1/metrics.
    collector.endpoint.push_str(endpoint_path);
    let (exporter, sink) = exporter(&collector, 3_600_000, Some("same-correlation"))?;
    let mut h = harness(sink.clone(), None).await?;
    // Warmup obtains another real capability binding; it only changes the
    // observation cohort, never the already granted execution authority.
    let original = h.headers.clone();
    let token = h.credential.secret.as_deref().context("credential")?;
    let caller = h.fixture.service.authenticate(token).map_err(failure)?;
    let binding = h
        .fixture
        .service
        .capabilities(&caller)
        .map_err(failure)?
        .bind(h.revision.revision_id.clone(), "warmup".try_into()?)?;
    let object = serde_json::to_value(binding)?;
    for (name, value) in object.as_object().context("binding")? {
        h.headers.insert(
            ::http::HeaderName::from_bytes(name.as_bytes())?,
            value.as_str().context("binding text")?.parse()?,
        );
    }
    completed(&h).await?;
    h.headers = original;
    completed(&h).await?;
    completed(&h).await?;
    ensure!(h.calls.load(Ordering::SeqCst) == 3 && collector.empty()?);
    let expected = capacity::stored_policy_bytes(&h).await?;
    tokio::time::timeout(TIMEOUT, h.fixture.service.shutdown()).await??;
    ensure!(sink.seal(true) && sink.seal(true));
    ensure!(flush(&exporter).await?);
    let view = collector.view()?;
    ensure!(
        view.event("admitted") == 2
            && view.event("first_queued") == 2
            && view.event("sql_ack") == 2
            && view.event("sealed_clean") == 1
    );
    ensure!(
        view.count("durable:complete_completed") == 2
            && view.count("sql_commit:acknowledged") == 2
            && view.count("first_queue:observed") == 2
    );
    ensure!(view.event("sample_limit") == 0 && view.event("sql_unconfirmed") == 0);
    ensure!(view.sum("durable:complete_completed")? > 0.0);
    capacity::assert_frozen(&view, 0, expected, 0)?;
    collector.close().await
}
struct GateRelease(crate::tenancy::Service);
impl Drop for GateRelease {
    fn drop(&mut self) {
        self.0.cleanup.release_gate();
    }
}
async fn wait_event(collector: &Collector, name: &str) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if collector.view()?.event(name) > 0 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    Ok(())
}
async fn queued_and_sql_boundaries() -> Result<()> {
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 100, None)?;
    let provider_gate = Arc::new(Semaphore::new(0));
    let mut h = harness(sink.clone(), Some(provider_gate.clone())).await?;
    let request = h.send(&Harness::client(false)?);
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        let response = request.send().await?;
        let id = execution_id(&response)?;
        ensure!(response.status() == StatusCode::OK);
        let _ = response.bytes().await?;
        Ok::<_, anyhow::Error>(id)
    }));
    tokio::time::timeout(TIMEOUT, h.received.recv())
        .await?
        .context("actual provider send")?;
    capacity::wait_current(&collector, "completion_reserved", 4 * 1024 * 1024).await?;
    capacity::wait_current(&collector, "potential_unknown", 1).await?;
    capacity::wait_current(&collector, "active", 1).await?;
    let gate = h.fixture.service.cleanup.hold(Phase::Active);
    let release = GateRelease(h.fixture.service.clone());
    let page = h.page();
    tokio::pin!(page);
    tokio::select! {result=&mut page=>{let _=result?;anyhow::bail!("read did not enter H gate")},()=gate.entered.notified()=>{}}
    provider_gate.add_permits(1);
    wait_event(&collector, "first_queued").await?;
    ensure!(
        collector.view()?.count("first_queue:observed") == 0,
        "receipt overtook held writer"
    );
    tokio::time::sleep(Duration::from_millis(40)).await;
    // Replace the held gate before releasing its original future. The next
    // receipt is allowed to BEGIN, and stops before its consuming COMMIT.
    let commit_gate = h.fixture.service.cleanup.hold(Phase::Committing);
    gate.release.notify_one();
    let _ = tokio::time::timeout(TIMEOUT, &mut page).await??;
    tokio::time::timeout(TIMEOUT, commit_gate.entered.notified()).await?;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if collector.view()?.count("prepare:observed") == 1 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    tokio::time::sleep(Duration::from_millis(60)).await;
    let before = collector.view()?;
    ensure!(before.count("first_queue:observed") == 1 && before.count("prepare:observed") == 1);
    ensure!(
        before.count("sql_commit:acknowledged") == 0
            && before.event("sql_ack") == 0
            && before.count("durable:complete_completed") == 0
    );
    ensure!(!task.is_finished() && h.fixture.service.cleanup.disposition() == Disposition::Pending);
    drop(release);
    let id = tokio::time::timeout(TIMEOUT, task).await???;
    ensure!(
        h.receipt(&Harness::client(false)?, &id)
            .await?
            .attempts
            .len()
            == 1
    );
    h.fixture.service.shutdown().await?;
    ensure!(sink.seal(true) && flush(&exporter).await?);
    let view = collector.view()?;
    ensure!(view.count("durable:complete_completed") == 1 && view.event("sql_ack") == 1);
    ensure!(
        view.sum("first_queue:observed")? >= 40_000_000.0
            && view.sum("durable:complete_completed")? >= 100_000_000.0
    );
    ensure!(
        view.p99_upper("durable:complete_completed")? > 20_000_000.0,
        "held receipt must not be rounded into a 20ms pass"
    );
    ensure!(h.calls.load(Ordering::SeqCst) == 1);
    capacity::assert_frozen(&view, 0, capacity::stored_policy_bytes(&h).await?, 0)?;
    collector.close().await
}
async fn rollback_retry_keeps_original_ready() -> Result<()> {
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 3_600_000, None)?;
    let h = harness(sink.clone(), None).await?;
    h.fixture.db.execute_unprepared("CREATE TRIGGER latency_refusal BEFORE INSERT ON tenancy_execution_receipts BEGIN SELECT RAISE(ABORT, 'receipt latency fixture'); END").await?;
    let response = h.send(&Harness::client(false)?).send().await?;
    ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
    let id = execution_id(&response)?;
    let _ = response.bytes().await?;
    ensure!(h.fixture.service.cleanup.disposition() == Disposition::RolledBack);
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.fixture
        .db
        .execute_unprepared("DROP TRIGGER latency_refusal")
        .await?;
    tokio::time::timeout(TIMEOUT, h.fixture.service.shutdown()).await??;
    let receipt = store::lookup(
        &h.fixture.db,
        &h.fixture.installation,
        &h.fixture.tenant,
        &id,
    )
    .await?
    .context("actual reconciled receipt")?;
    ensure!(
        receipt.attempts.len() == 1
            && receipt.usage.input_tokens == Some(11)
            && h.calls.load(Ordering::SeqCst) == 1
    );
    ensure!(sink.seal(true) && flush(&exporter).await?);
    let view = collector.view()?;
    ensure!(
        view.event("admitted") == 1
            && view.event("first_queued") == 1
            && view.event("retry") == 1
            && view.event("rollback_ack") == 1
            && view.event("sql_ack") == 1
    );
    ensure!(
        view.count("prepare:observed") == 2
            && view.count("durable:complete_completed") == 1
            && view.sum("durable:complete_completed")? >= 50_000_000.0
    );
    collector.close().await
}
async fn caller_disconnect_does_not_ack_or_reset() -> Result<()> {
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 3_600_000, None)?;
    let h = harness(sink.clone(), None).await?;
    let gate = h.fixture.service.cleanup.hold(Phase::Committing);
    let release = GateRelease(h.fixture.service.clone());
    let request = h.send(&Harness::client(false)?);
    let caller =
        AbortOnDropHandle::new(tokio::spawn(
            async move { request.send().await?.bytes().await },
        ));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    caller.abort();
    ensure!(caller.await.is_err_and(|e| e.is_cancelled()));
    ensure!(
        tokio::time::timeout(
            Duration::from_millis(20),
            h.fixture.service.admission.wait_drained()
        )
        .await
        .is_err()
    );
    drop(release);
    h.fixture.service.shutdown().await?;
    ensure!(sink.seal(true) && flush(&exporter).await?);
    let view = collector.view()?;
    ensure!(
        view.event("admitted") == 1
            && view.event("first_queued") == 1
            && view.event("sql_ack") == 1
            && view.count("durable:complete_completed") == 1
            && view.event("retry") == 0
    );
    ensure!(h.calls.load(Ordering::SeqCst) == 1);
    collector.close().await
}
async fn receiver_failure_is_cached() -> Result<()> {
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 3_600_000, None)?;
    let h = harness(sink.clone(), None).await?;
    completed(&h).await?;
    h.fixture.service.shutdown().await?;
    ensure!(sink.seal(true));
    collector.refuse();
    ensure!(!flush(&exporter).await?);
    ensure!(!flush(&exporter).await?);
    ensure!(collector.empty()?);
    collector.close().await
}
async fn uncertain_commit_is_not_success(unwind_before_sql: bool) -> Result<()> {
    let collector = Collector::start().await?;
    let (exporter, sink) = exporter(&collector, 3_600_000, None)?;
    let h = harness(sink.clone(), None).await?;
    if !unwind_before_sql {
        for sql in [
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE latency_parent(id INTEGER PRIMARY KEY)",
            "CREATE TABLE latency_child(parent INTEGER REFERENCES latency_parent(id) DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TRIGGER latency_commit_fault AFTER INSERT ON tenancy_execution_receipts BEGIN INSERT INTO latency_child(parent) VALUES (77); END",
        ] {
            h.fixture.db.execute_unprepared(sql).await?;
        }
    }
    let gate = unwind_before_sql.then(|| h.fixture.service.cleanup.hold(Phase::Committing));
    let release = GateRelease(h.fixture.service.clone());
    let request = h.send(&Harness::client(false)?);
    let caller = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
    if let Some(gate) = gate {
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        h.fixture.service.cleanup.unwind_gate()?;
    }
    drop(release);
    let response = tokio::time::timeout(TIMEOUT, caller).await???;
    ensure!(!response.status().is_success());
    let _ = response.bytes().await?;
    tokio::time::timeout(TIMEOUT, h.fixture.service.cleanup.quarantined()).await?;
    ensure!(
        h.fixture.service.cleanup.disposition() == Disposition::Ambiguous
            && h.fixture.service.cleanup.ensure_clean().is_err()
    );
    ensure!(
        tokio::time::timeout(
            Duration::from_millis(20),
            h.fixture.service.admission.wait_drained()
        )
        .await
        .is_err()
    );
    let (_, _, active, queued) = h
        .fixture
        .service
        .admission
        .ingress_for_test()
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    ensure!(
        active == 1 && queued == 0,
        "the actual ambiguous receipt lost its original admission"
    );
    ensure!(!sink.seal(false) && flush(&exporter).await?);
    let view = collector.view()?;
    ensure!(view.immutable.get("final:available") == Some(&0) && view.frozen.is_none());
    ensure!(
        view.event("admitted") == 1
            && view.event("sql_unconfirmed") == u64::from(!unwind_before_sql)
            && view.event("sql_ack") == 0
            && view.count("sql_commit:unconfirmed") == u64::from(!unwind_before_sql)
            && view.count("transaction:unconfirmed") == 1
            && view.count("durable:complete_completed") == 0
            && view.event("sealed_failed") == 1
    );
    ensure!(h.calls.load(Ordering::SeqCst) == 1);
    collector.close().await
    // The real ambiguous actor remains quarantined until this dedicated child
    // exits. This case makes no clean shutdown or file-lease-release claim.
}
