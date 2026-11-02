//! Cancellation joins the original intent before queuing a terminal receipt.
use super::*;
use crate::tenancy::{
    Service,
    actor::Job,
    execution::deadline::Deadline,
    receipts::{
        ordered::{IntentJob, intent::Intent},
        owner::{Identity, Owner as ReceiptOwner},
        store::{self, format::Terminal},
    },
    transaction::Phase,
};
use bitrouter_ai::tenancy::{
    configuration::Limits,
    ordered::{AcceptedSelectionV2, GenerationProtocol},
};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::time::Duration;
use tokio::time::{Instant, timeout};

async fn service(path: &std::path::Path) -> Result<Service> {
    let db =
        crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rwc", path.display())).await?;
    crate::db::run_migrations(db.database()).await?;
    Service::open_owned_profile(db, &Default::default(), &Default::default()).await
}

async fn new_owner(service: &Service, id: &str) -> Result<Arc<ReceiptOwner>> {
    owner_with_deadline(service, id, None).await
}

async fn owner_with_deadline(
    service: &Service,
    id: &str,
    deadline: Option<Instant>,
) -> Result<Arc<ReceiptOwner>> {
    let installation = service
        .current()
        .map_err(|error| anyhow::anyhow!("authority {:?}", error.0.code))?
        .installation
        .clone();
    let execution: ExecutionId = id.try_into()?;
    let correlation = "ordered-cancellation".try_into()?;
    let tenant = "tenant".try_into()?;
    let journal = Journal::with_selection(
        Owner {
            installation: installation.clone(),
            execution: execution.clone(),
        },
        Some(&[0]),
    )?;
    let lease = service
        .admission
        .acquire_until(
            &tenant,
            &Limits {
                active_executions: 16,
                queued_work: 32,
                learning_jobs: 1,
            },
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .map_err(|error| anyhow::anyhow!("admission {error:?}"))?;
    lease
        .bind_execution(&execution)
        .map_err(|error| anyhow::anyhow!("execution registration {error:?}"))?;
    let observation = service.receipt_observer.timing(&correlation);
    Ok(ReceiptOwner::new(
        Identity {
            installation,
            tenant,
            principal: "principal".try_into()?,
            execution,
            correlation,
            revision: "revision".try_into()?,
            ordered: Some((
                AcceptedSelectionV2 {
                    generation_defaults_digest: None,
                    admission_mode: bitrouter_ai::tenancy::admission::AdmissionMode::Legacy,
                    route: reference(ResourceKind::Route)?,
                    inbound_protocol: GenerationProtocol::Messages,
                    candidate_indices: vec![0],
                },
                64,
            )),
        },
        journal,
        service.completions.clone(),
        Arc::new(lease),
        service.admission.clone(),
        service.cleanup.clone(),
        (
            Deadline::Execution(deadline),
            Default::default(),
            &service.execution_settings,
            observation,
        ),
    ))
}

#[tokio::test]
async fn ordered_queue_expiry_before_begin_retains_cancelled_cleanup_without_fencing() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let service = service(&directory.path().join("expired.sqlite")).await?;
    let blocker = new_owner(&service, "blocker").await?;
    let gate = service.cleanup.hold(Phase::Committing);
    let first = enqueue(&service, &blocker)?;
    timeout(Duration::from_secs(2), gate.entered.notified()).await?;
    let expired = owner_with_deadline(
        &service,
        "expired",
        Some(Instant::now() + Duration::from_millis(100)),
    )
    .await?;
    let ticket = service.pipelines.admit(expired.clone())?;
    let waiting = enqueue(&service, &expired)?;
    let begins = service.cleanup.sql_counts_for_test().0;
    tokio::time::sleep_until(expired.attachment_deadline + Duration::from_millis(10)).await;
    ensure!(service.cleanup.sql_counts_for_test().0 == begins);
    service.cleanup.release_gate();
    ack(first).await?;
    // The original caller still receives expiry, even though cleanup retains
    // the same identity. No later acknowledgement can grant a provider send.
    ensure!(timeout(Duration::from_secs(3), waiting).await??.is_err());
    drop(ticket);
    timeout(Duration::from_secs(3), async {
        while !expired.durable() {
            if let Some(error) = expired.failure() {
                anyhow::bail!("expired owner failed: {error:?}");
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let saved = store::lookup_terminal(
        &service.db,
        &expired.identity.installation,
        &expired.identity.tenant,
        expired.id(),
    )
    .await?
    .context("expired terminal")?;
    let Terminal::V2(saved) = saved else {
        anyhow::bail!("wrong receipt format");
    };
    ensure!(saved.receipt.attempts.is_empty() && saved.receipt.outcome == Outcome::Cancelled);
    ensure!(expired.cancelled.is_cancelled());
    ensure!(intent_count(&service, "expired").await? == 0);
    ensure!(service.cleanup.sql_counts_for_test().0 == begins + 2);
    ensure!(service.admission.receipts_ready() && service.current().is_ok());
    blocker
        .finish_or_join(Outcome::Cancelled)
        .await
        .map_err(|error| anyhow::anyhow!("blocker cleanup {error:?}"))?;
    blocker
        .release_active()
        .map_err(|error| anyhow::anyhow!("blocker release {error:?}"))?;
    service.shutdown().await?;
    ensure!(
        service
            .admission
            .ingress_for_test()
            .map_err(|error| anyhow::anyhow!("capacity {error:?}"))?
            .2
            == 0
    );
    Ok(())
}

type Ack = tokio::sync::oneshot::Receiver<Result<(), crate::tenancy::error::Failure>>;

fn enqueue(service: &Service, owner: &Arc<ReceiptOwner>) -> Result<Ack> {
    let stamp = service
        .startup
        .as_ref()
        .context("startup owner")?
        .bootstrap_stamp(&service.cleanup)?;
    owner
        .intent
        .set(Arc::new(Intent::new(&owner.identity, stamp)?))
        .map_err(|_| anyhow::anyhow!("intent already initialized"))?;
    let slot = service
        .reserve_ordered_intent()
        .map_err(|error| anyhow::anyhow!("intent queue {:?}", error.0.code))?;
    let (reply, wait) = tokio::sync::oneshot::channel();
    slot.send(Job::OrderedIntent(IntentJob {
        owner: owner.clone(),
        reply,
    }));
    Ok(wait)
}

async fn ack(wait: Ack) -> Result<()> {
    timeout(Duration::from_secs(3), wait)
        .await??
        .map_err(|error| anyhow::anyhow!("intent acknowledgement {:?}", error.0.code))
}

async fn intent_count(service: &Service, id: &str) -> Result<i64> {
    let row = service
        .db
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM tenancy_dispatch_intents WHERE execution_id=?",
            [id.into()],
        ))
        .await?
        .context("intent count")?;
    Ok(row.try_get("", "n")?)
}

#[tokio::test]
async fn abandoned_ordered_admission_waits_for_queued_intent_without_fencing() -> Result<()> {
    for cancel_token in [false, true] {
        let directory = tempfile::tempdir()?;
        let service = service(&directory.path().join("cancel.sqlite")).await?;
        let blocker = new_owner(&service, "blocker").await?;
        let gate = service.cleanup.hold(Phase::Committing);
        let first = enqueue(&service, &blocker)?;
        timeout(Duration::from_secs(2), gate.entered.notified()).await?;

        let cancelled = new_owner(&service, "cancelled").await?;
        let ticket = service.pipelines.admit(cancelled.clone())?;
        let abandoned = enqueue(&service, &cancelled)?;
        if cancel_token {
            cancelled.cancel();
        }
        drop(abandoned);
        drop(ticket);
        timeout(Duration::from_secs(2), async {
            while !cancelled.cleanup_started_for_test() {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        // The registry is actively cleaning up, but cannot overtake its intent
        // through the writer's higher-priority completion queue.
        ensure!(service.completions.capacity() == 128);
        ensure!(cancelled.failure().is_none());
        service.cleanup.release_gate();
        ack(first).await?;
        timeout(Duration::from_secs(3), async {
            while !cancelled.durable() {
                if let Some(error) = cancelled.failure() {
                    anyhow::bail!("cancelled owner failed: {error:?}");
                }
                tokio::task::yield_now().await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let saved = store::lookup_terminal(
            &service.db,
            &cancelled.identity.installation,
            &cancelled.identity.tenant,
            cancelled.id(),
        )
        .await?
        .context("cancelled terminal")?;
        let Terminal::V2(saved) = saved else {
            anyhow::bail!("wrong receipt format");
        };
        ensure!(saved.receipt.attempts.is_empty());
        ensure!(saved.receipt.outcome == Outcome::Cancelled);
        ensure!(intent_count(&service, "cancelled").await? == 0);
        ensure!(service.admission.receipts_ready() && service.current().is_ok());

        blocker
            .finish_or_join(Outcome::Cancelled)
            .await
            .map_err(|error| anyhow::anyhow!("blocker cleanup {error:?}"))?;
        blocker
            .release_active()
            .map_err(|error| anyhow::anyhow!("blocker release {error:?}"))?;
        drop(cancelled);
        timeout(Duration::from_secs(2), async {
            loop {
                let active = service
                    .admission
                    .ingress_for_test()
                    .map_err(|error| anyhow::anyhow!("capacity {error:?}"))?
                    .2;
                if active == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        service.shutdown().await?;
    }
    Ok(())
}
