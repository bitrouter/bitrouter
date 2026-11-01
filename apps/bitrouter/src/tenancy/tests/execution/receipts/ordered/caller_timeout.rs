//! Caller expiry keeps the original queued intent and its cleanup owner.
use super::*;
use crate::tenancy::{receipts::store::format::Terminal, transaction::Phase};
use base64::Engine as _;
use bitrouter_ai::types::ApiProtocol;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use tokio::time::{Instant, timeout};

struct ReleaseGate(Arc<crate::tenancy::transaction::Proof>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release_gate();
    }
}

fn ordered_headers(h: &OrderedHarness, binding: &OrderedDispatchBindingV2) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", format!("Bearer {}", h.token()?)),
        (
            "bitrouter-tenancy-version",
            binding.contract_version.to_string(),
        ),
        (
            "bitrouter-installation-id",
            binding.installation_id.to_string(),
        ),
        ("bitrouter-instance-id", binding.instance_id.to_string()),
        (
            "bitrouter-capability-revision",
            binding.capability_revision.to_string(),
        ),
        (
            "bitrouter-execution-revision",
            binding.execution_revision.to_string(),
        ),
        (
            "bitrouter-correlation-id",
            binding.correlation_id.to_string(),
        ),
        ("bitrouter-receipt-mode", binding.receipt_mode.to_string()),
        (
            "bitrouter-candidate-selection",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&binding.selection)?),
        ),
    ] {
        headers.insert(
            ::http::HeaderName::from_bytes(name.as_bytes())?,
            value.parse()?,
        );
    }
    Ok(headers)
}

#[tokio::test]
async fn caller_wait_timeout_retains_cancelled_cleanup_without_fencing() -> Result<()> {
    let h = OrderedHarness::new(1, 64, None).await?;
    let mut binding = h
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    let service = &h.fixture.service;
    // Real admission warms retained immutable configuration before the actor is held.
    binding.correlation_id = "independent-caller-warmup".try_into()?;
    let warm = service
        .admit_execution(
            service
                .submission(&ordered_headers(&h, &binding)?)
                .map_err(failure)?,
            MODEL,
            &ApiProtocol::ChatCompletions,
        )
        .await
        .map_err(failure)?;
    let warm_owner = warm.receipt_owner()?.clone();
    drop(warm);
    timeout(Duration::from_secs(3), async {
        while !warm_owner.durable()
            || service
                .admission
                .ingress_for_test()
                .map_err(|e| anyhow::anyhow!("active {e:?}"))?
                .2
                != 0
        {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    drop(warm_owner);

    // The unrelated management transaction has a longer deadline than the caller.
    let gate = service.cleanup.hold(Phase::Committing);
    let _release = ReleaseGate(service.cleanup.clone());
    let blocking_service = service.clone();
    let operator = h.fixture.operator.clone();
    let command = crate::tenancy::command::Command::Issue(
        h.fixture.tenant.clone(),
        h.fixture.issuance("independent-held-management")?,
    );
    let management = AbortOnDropHandle::new(tokio::spawn(async move {
        blocking_service.execute(operator, command).await
    }));
    timeout(Duration::from_secs(2), gate.entered.notified()).await?;
    let before = service.cleanup.sql_counts_for_test();
    let before_queue = service.commands.capacity();
    binding.correlation_id = "independent-caller-timeout".try_into()?;
    let start = Instant::now();
    let submission = service
        .submission(&ordered_headers(&h, &binding)?)
        .map_err(failure)?
        .narrow_deadline_for_test(start + Duration::from_millis(100));
    // This calls finish_admission -> Registry::admit -> actual prepare_ordered_intent.
    let caller = timeout(
        Duration::from_secs(1),
        service.admit_execution(submission, MODEL, &ApiProtocol::ChatCompletions),
    )
    .await?;
    let error = caller
        .err()
        .context("caller unexpectedly got a dispatch execution")?;
    ensure!(error.0.code == bitrouter_ai::tenancy::operation::ErrorCode::AuthorityUnavailable);
    ensure!(
        start.elapsed() >= Duration::from_millis(90),
        "failed before actual caller wait"
    );
    ensure!(
        service.cleanup.sql_counts_for_test() == before,
        "intent began before management release"
    );
    ensure!(
        service.commands.capacity() + 1 == before_queue,
        "no original intent queued"
    );
    ensure!(
        service
            .admission
            .ingress_for_test()
            .map_err(|e| anyhow::anyhow!("active {e:?}"))?
            .2
            == 1,
        "caller lost registry owner"
    );
    ensure!(
        service.admission.receipts_ready() && service.current().is_ok(),
        "caller wait timeout poisoned admission before actor settled"
    );
    ensure!(
        h.observed()?.is_empty(),
        "caller timeout caused provider I/O"
    );
    service.cleanup.release_gate();
    timeout(Duration::from_secs(2), management)
        .await??
        .map_err(failure)?;
    let terminal = timeout(Duration::from_secs(3), async {
        loop {
            let row = service
                .db
                .query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Sqlite,
                    "SELECT execution_id FROM tenancy_execution_receipts WHERE correlation_id=?",
                    [binding.correlation_id.as_str().into()],
                ))
                .await?;
            if let Some(row) = row {
                let id: String = row.try_get("", "execution_id")?;
                let id: ExecutionId = id.try_into()?;
                break store::lookup_terminal(
                    &service.db,
                    &h.fixture.installation,
                    &h.fixture.tenant,
                    &id,
                )
                .await?
                .context("terminal read");
            }
            ensure!(
                service.admission.receipts_ready(),
                "cleanup fenced shared admission"
            );
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let Terminal::V2(saved) = terminal else {
        anyhow::bail!("not v2");
    };
    ensure!(saved.receipt.outcome == Outcome::Cancelled && saved.receipt.attempts.is_empty());
    let row = service
        .db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM tenancy_dispatch_intents WHERE execution_id=?",
            [saved.receipt.execution_id.as_str().into()],
        ))
        .await?
        .context("count")?;
    ensure!(row.try_get::<i64>("", "n")? == 0);
    timeout(Duration::from_secs(2), async {
        while service
            .admission
            .ingress_for_test()
            .map_err(|e| anyhow::anyhow!("active {e:?}"))?
            .2
            != 0
        {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    ensure!(
        service
            .admission
            .receipt_capacity_for_test()
            .map_err(|e| anyhow::anyhow!("reserved {e:?}"))?
            .0
            == 0
    );
    ensure!(
        service.cleanup.sql_counts_for_test().0 == before.0 + 2,
        "expected one intent and one terminal cleanup transaction"
    );
    ensure!(
        service.admission.receipts_ready() && service.current().is_ok() && h.observed()?.is_empty()
    );
    let unrelated = "independent-unrelated".try_into()?;
    let lease = service
        .admission
        .acquire_until(
            &unrelated,
            &bitrouter_ai::tenancy::configuration::Limits {
                active_executions: 1,
                queued_work: 1,
                learning_jobs: 1,
            },
            Instant::now() + Duration::from_millis(100),
        )
        .await
        .map_err(|e| anyhow::anyhow!("unrelated admission {e:?}"))?;
    drop(lease);
    service.shutdown().await?;
    Ok(())
}
