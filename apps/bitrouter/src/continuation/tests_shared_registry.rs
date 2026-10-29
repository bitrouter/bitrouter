use super::*;
use crate::continuation::tests::{static_authority, target};
use crate::gateway::pipeline::settlement::DeliveryAcknowledgement;
use tokio::sync::oneshot;
use tokio::time::{Duration, Instant, timeout};

async fn fixture() -> Result<(DatabaseConnection, ContinuationRegistry)> {
    let db = super::super::tests::isolated_database().await?;
    let authority = SharedContinuationDb::fixture(db.clone()).await?;
    let registry = ContinuationRegistry::new_shared(authority, 30, 32)?;
    Ok((db, registry))
}

async fn prepare(
    registry: &ContinuationRegistry,
    id: u64,
    request: &str,
    receipt: &RequiredFinalizationReceipt,
) -> Result<bool> {
    registry
        .bind_pending(
            "owner",
            request,
            ContinuationBinding {
                provider_response_id: "fixture-native-response",
                effective_model: "openai:gpt-5",
                effective_effort: None,
                causal_prefix_commitment: None,
                target: &target("fixture-static-key"),
                credential_authority: &static_authority("fixture-static-key"),
            },
            PendingBindAttempt {
                // Shared authority must never inherit this deliberately wrong clock.
                now: Utc::now() + TimeDelta::days(1_000),
                delivery_attempt_id: id,
                receipt: Some(receipt.clone()),
            },
        )
        .await
}

struct Sender {
    ready: oneshot::Receiver<PipelineResult<()>>,
    acknowledgement: oneshot::Sender<DeliveryAcknowledgement>,
    activation: oneshot::Receiver<PipelineResult<()>>,
    terminal: oneshot::Sender<()>,
    writer: terminal::Writer,
}

fn handshake() -> (RequiredDeliveryHandshake, Sender) {
    let (ready, ready_rx) = oneshot::channel();
    let (ack, ack_rx) = oneshot::channel();
    let (activation, activation_rx) = oneshot::channel();
    let (terminal, terminal_rx) = oneshot::channel();
    let (witness, writer) = terminal::pair();
    (
        RequiredDeliveryHandshake::new_with_terminal_witness(
            ready,
            ack_rx,
            activation,
            terminal_rx,
            witness,
        ),
        Sender {
            ready: ready_rx,
            acknowledgement: ack,
            activation: activation_rx,
            terminal,
            writer,
        },
    )
}

async fn join_fixture(
    db: DatabaseConnection,
    registry: ContinuationRegistry,
    result: Result<()>,
) -> Result<()> {
    // Force only joins the owned background worker; it never manufactures
    // successful compensation for retained fixture obligations.
    registry.force_owned().await?;
    drop(registry);
    db.close().await?;
    result
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn successful_sender_evidence_survives_cancelled_registry_waiter() -> Result<()> {
    let (db, registry) = fixture().await?;
    let result = async {
        let receipt = RequiredFinalizationReceipt::new();
        ensure!(prepare(&registry, 1, "terminal-cancel", &receipt).await?);
        let (delivery, sender) = handshake();
        let task_registry = registry.clone();
        let task_receipt = receipt.clone();
        let task = tokio::spawn(async move {
            task_registry
                .activate_pending(1, Some(&task_receipt), &delivery)
                .await
        });
        timeout(Duration::from_secs(5), sender.ready).await???;
        sender
            .acknowledgement
            .send(DeliveryAcknowledgement::Delivered)
            .map_err(|_| anyhow::anyhow!("acknowledgement receiver disappeared"))?;
        timeout(Duration::from_secs(5), sender.activation).await???;
        ensure!(matches!(
            registry
                .resolve("owner", "terminal-cancel", Utc::now())
                .await?,
            ContinuationResolution::Missing
        ));
        // No await between the sender's terminal event and abort: the receiver
        // has not resumed. Real DeliveryPermit writer semantics are separately
        // tested in pipeline::execution::stream_outcome_tests.
        sender
            .terminal
            .send(())
            .map_err(|_| anyhow::anyhow!("terminal receiver disappeared"))?;
        sender.writer.complete(true);
        task.abort();
        ensure!(task.await.is_err_and(|error| error.is_cancelled()));
        let drain = registry
            .drain_owned_until(Instant::now() + Duration::from_secs(5))
            .await?;
        ensure!(drain.is_clean(), "successful publication retained an owner");
        ensure!(matches!(
            registry
                .resolve(
                    "owner",
                    "terminal-cancel",
                    Utc::now() + TimeDelta::days(10_000)
                )
                .await?,
            ContinuationResolution::Active(_)
        ));
        Ok(())
    }
    .await;
    join_fixture(db, registry, result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn failed_terminal_signal_removes_only_the_owned_generation() -> Result<()> {
    let (db, registry) = fixture().await?;
    let result = async {
        let receipt = RequiredFinalizationReceipt::new();
        prepare(&registry, 2, "negative-terminal", &receipt).await?;
        let (delivery, sender) = handshake();
        let task_registry = registry.clone();
        let task_receipt = receipt.clone();
        let task = tokio::spawn(async move {
            task_registry
                .activate_pending(2, Some(&task_receipt), &delivery)
                .await
        });
        timeout(Duration::from_secs(5), sender.ready).await???;
        sender
            .acknowledgement
            .send(DeliveryAcknowledgement::Delivered)
            .map_err(|_| anyhow::anyhow!("acknowledgement receiver disappeared"))?;
        timeout(Duration::from_secs(5), sender.activation).await???;
        sender.writer.complete(false);
        sender
            .terminal
            .send(())
            .map_err(|_| anyhow::anyhow!("terminal receiver disappeared"))?;
        ensure!(!timeout(Duration::from_secs(5), task).await???);
        ensure!(!registry.has_pending_publications());
        ensure!(matches!(
            registry
                .resolve("owner", "negative-terminal", Utc::now())
                .await?,
            ContinuationResolution::Missing
        ));
        Ok(())
    }
    .await;
    join_fixture(db, registry, result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn existing_active_is_non_owning_but_missing_preparation_is_an_error() -> Result<()> {
    let (db, registry) = fixture().await?;
    let result = async {
        let shared = registry.shared()?;
        let peer = SharedNative::new(shared.native.authority.fixture_peer().await?);
        let row = super::super::tests::bind(&peer, "established").await?.row;
        let fence = super::super::tests::publication(&row);
        peer.transition(&fence, ForwardTransition::Delivering)
            .await?;
        peer.transition(&fence, ForwardTransition::Active).await?;
        let receipt = RequiredFinalizationReceipt::new();
        ensure!(!prepare(&registry, 3, "established", &receipt).await?);
        let pending = shared
            .get(3, Some(&receipt))
            .context("existing preparation missing")?;
        ensure!(matches!(pending.disposition, Disposition::Existing(_)));
        registry.rollback_pending(3, Some(&receipt)).await?;
        ensure!(!registry.has_pending_publications());
        ensure!(matches!(
            registry.resolve("owner", "established", Utc::now()).await?,
            ContinuationResolution::Active(_)
        ));
        let (delivery, _sender) = handshake();
        ensure!(
            registry
                .activate_pending(3, Some(&receipt), &delivery)
                .await
                .is_err()
        );
        Ok(())
    }
    .await;
    join_fixture(db, registry, result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cancelled_rollback_retains_receipt_matched_cleanup_before_lock_wait() -> Result<()> {
    let (db, registry) = fixture().await?;
    let result = async {
        let receipt = RequiredFinalizationReceipt::new();
        prepare(&registry, 4, "rollback-wait", &receipt).await?;
        let shared = registry.shared()?;
        let pending = shared
            .get(4, Some(&receipt))
            .context("preparation missing")?;
        let wrong = RequiredFinalizationReceipt::new();
        registry.rollback_pending(4, Some(&wrong)).await?;
        ensure!(
            shared
                .get(4, Some(&receipt))
                .context("receipt isolation failed")?
                .reason
                .is_none()
        );
        let lock = registry.pending_identity_lock(&pending.intent.identity);
        let guard = lock.lock().await;
        let mut rollback = Box::pin(registry.rollback_pending(4, Some(&receipt)));
        std::future::poll_fn(|cx| {
            assert!(rollback.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(rollback);
        ensure!(
            shared
                .get(4, Some(&receipt))
                .context("cancelled marker lost")?
                .reason
                == Some(Reason::RollbackRequired)
        );
        drop(guard);
        ensure!(!registry.shared_reconcile_pending().await);
        ensure!(!registry.has_pending_publications());
        ensure!(matches!(
            registry
                .resolve("owner", "rollback-wait", Utc::now())
                .await?,
            ContinuationResolution::Missing
        ));
        Ok(())
    }
    .await;
    join_fixture(db, registry, result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn uncertain_reserved_intents_require_confirmed_producer_close() -> Result<()> {
    let (db, registry) = fixture().await?;
    let result = async {
        let shared = registry.shared()?;
        for (id, request, inserted) in [(5, "unknown-absent", false), (6, "unknown-inserted", true)]
        {
            let pending = Pending {
                intent: shared.native.intent("owner", request)?,
                receipt: RequiredFinalizationReceipt::new(),
                disposition: Disposition::Reserved,
                reason: Some(Reason::Uncertain),
                terminal: None,
            };
            shared.lock().pending.insert(id, pending.clone());
            if inserted {
                // Deliberately discard the acknowledged result to exercise the
                // retained-intent branch; this is not a database wire-fault test.
                shared
                    .native
                    .bind(
                        "owner",
                        request,
                        &ContinuationBinding {
                            provider_response_id: "fixture-native-response",
                            effective_model: "openai:gpt-5",
                            effective_effort: None,
                            causal_prefix_commitment: None,
                            target: &target("fixture-static-key"),
                            credential_authority: &static_authority("fixture-static-key"),
                        },
                        &pending.intent.generation,
                        PUBLICATION_PROVISIONAL,
                        registry.inner.retention,
                    )
                    .await?;
            }
        }
        ensure!(!registry.shared_reconcile_pending().await);
        ensure!(shared.markers().reserved == 2);
        let receipt = shared
            .lock()
            .pending
            .get(&5)
            .context("reserved fixture disappeared")?
            .receipt
            .clone();
        ensure!(registry.rollback_pending(5, Some(&receipt)).await.is_err());
        ensure!(shared.markers().reserved == 2);
        let drain = registry
            .drain_owned_until(Instant::now() + Duration::from_secs(5))
            .await?;
        ensure!(drain.is_clean());
        ensure!(shared.native.authority.confirm_closed().await?.is_some());
        ensure!(matches!(
            registry
                .resolve("owner", "unknown-inserted", Utc::now())
                .await?,
            ContinuationResolution::Missing
        ));
        ensure!(
            prepare(
                &registry,
                7,
                "after-close",
                &RequiredFinalizationReceipt::new()
            )
            .await
            .is_err()
        );
        Ok(())
    }
    .await;
    join_fixture(db, registry, result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn retiring_active_without_terminal_evidence_keeps_retention() -> Result<()> {
    let (db, registry) = fixture().await?;
    let result = async {
        let receipt = RequiredFinalizationReceipt::new();
        prepare(&registry, 8, "active-unknown", &receipt).await?;
        let shared = registry.shared()?;
        let pending = shared
            .get(8, Some(&receipt))
            .context("preparation missing")?;
        let fence = pending.forward()?;
        shared
            .native
            .transition(&fence, ForwardTransition::Delivering)
            .await?;
        shared
            .native
            .transition(&fence, ForwardTransition::Active)
            .await?;
        let drain = registry
            .drain_owned_until(Instant::now() + Duration::from_secs(5))
            .await?;
        ensure!(drain.retained == Some(owned::Retained::NoProgress) && drain.markers.total == 1);
        let row = super::super::tests::read(&shared.native, &pending.intent.identity).await?;
        ensure!(row.publication_state == PUBLICATION_ACTIVE);
        // A separate producer has no local unresolved-delivery visibility mask.
        let peer = SharedNative::new(shared.native.authority.fixture_peer().await?);
        ensure!(matches!(
            peer.resolve("owner", "active-unknown").await?,
            ContinuationResolution::Active(_)
        ));
        registry.rollback_pending(8, Some(&receipt)).await?;
        ensure!(!registry.has_pending_publications());
        Ok(())
    }
    .await;
    join_fixture(db, registry, result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cancelled_renewal_observation_is_refreshed_without_reopening_cleanup() -> Result<()> {
    let (db, registry) = fixture().await?;
    let result = async {
        let receipt = RequiredFinalizationReceipt::new();
        prepare(&registry, 9, "renew-observation", &receipt).await?;
        let shared = registry.shared()?;
        let pending = shared
            .get(9, Some(&receipt))
            .context("preparation missing")?;
        let lock = registry.pending_identity_lock(&pending.intent.identity);
        let guard = lock.lock().await;
        shared.update(9, &pending, |p| {
            p.disposition = Disposition::Owned { lease: None }
        });
        let refreshed = registry.shared_renew(9, &pending).await?;
        ensure!(!refreshed.lease_until.is_empty());
        shared
            .native
            .claim_cleanup(Authority::Cancelled(&pending.intent))
            .await?;
        shared.update(9, &pending, |p| {
            p.disposition = Disposition::Owned { lease: None }
        });
        ensure!(registry.shared_renew(9, &pending).await.is_err());
        ensure!(
            shared
                .get(9, Some(&receipt))
                .context("marker disappeared")?
                .reason
                == Some(Reason::Uncertain)
        );
        drop(guard);
        ensure!(!registry.shared_reconcile_pending().await);
        ensure!(!registry.has_pending_publications());
        Ok(())
    }
    .await;
    join_fixture(db, registry, result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn expired_drain_does_not_close_empty_producer_or_start_database_work() -> Result<()> {
    let (db, registry) = fixture().await?;
    let result = async {
        let shared = registry.shared()?;
        let expired = Instant::now() - Duration::from_secs(1);
        for _ in 0..2 {
            let drain = registry.drain_owned_until(expired).await?;
            ensure!(drain.worker.joined && drain.retained == Some(owned::Retained::Deadline));
            ensure!(shared.native.authority.confirm_closed().await?.is_none());
        }
        let drain = registry
            .drain_owned_until(Instant::now() + Duration::from_secs(5))
            .await?;
        ensure!(drain.is_clean());
        ensure!(shared.native.authority.confirm_closed().await?.is_some());
        Ok(())
    }
    .await;
    join_fixture(db, registry, result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn uncertain_owned_bind_cannot_demote_live_prepublication_without_failure() -> Result<()> {
    let (db, registry) = fixture().await?;
    let result = async {
        let receipt = RequiredFinalizationReceipt::new();
        prepare(&registry, 10, "uncertain-live", &receipt).await?;
        let shared = registry.shared()?;
        let pending = shared
            .get(10, Some(&receipt))
            .context("preparation missing")?;
        let lock = registry.pending_identity_lock(&pending.intent.identity);
        let guard = lock.lock().await;
        shared.update(10, &pending, |p| p.retire(Reason::Uncertain));
        let row = super::super::tests::read(&shared.native, &pending.intent.identity).await?;
        registry.shared_reconcile_one(10, &pending).await?;
        ensure!(shared.get(10, Some(&receipt)).is_some());
        ensure!(super::super::tests::read(&shared.native, &pending.intent.identity).await? == row);
        super::super::tests::fixture_lease(&shared.native, &row, -1).await?;
        registry.shared_reconcile_one(10, &pending).await?;
        ensure!(!registry.has_pending_publications());
        drop(guard);
        Ok(())
    }
    .await;
    join_fixture(db, registry, result).await
}
