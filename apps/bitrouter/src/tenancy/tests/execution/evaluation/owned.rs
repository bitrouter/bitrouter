//! The private completion proof distinguishes early failure from lost evidence.
mod closure;

use super::*;
use crate::gateway::{
    assembly::App,
    caller::CallerContext,
    error::{BitrouterError, Result as GatewayResult},
    pipeline::{
        native::{NativeExecutionControl, NativePlan, NativePlanAdmission},
        native_preparation::{NativePreparationWorkKind, NativePreparationWorkReport},
    },
};
use async_trait::async_trait;
use bitrouter_ai::{
    core::evidence::NativeAttemptReport,
    types::{Message, Prompt, Role},
};

struct Reject {
    early: bool,
    entry_outcomes: AtomicUsize,
    plans: AtomicUsize,
}
#[async_trait]
impl NativeExecutionControl for Reject {
    async fn after_preparation_work(
        &self,
        report: NativePreparationWorkReport,
    ) -> GatewayResult<()> {
        if report.work.kind == NativePreparationWorkKind::PreResolutionHook {
            self.entry_outcomes.fetch_add(1, Ordering::SeqCst);
            if self.early {
                return Err(BitrouterError::Forbidden(
                    "fixture rejects entry outcome".into(),
                ));
            }
        }
        Ok(())
    }
    async fn plan(&self, _plan: NativePlan) -> GatewayResult<NativePlanAdmission> {
        self.plans.fetch_add(1, Ordering::SeqCst);
        Err(BitrouterError::Forbidden(
            "fixture rejects completed policy plan".into(),
        ))
    }
    async fn before_attempt(&self, _request: &str, _index: u32) -> GatewayResult<()> {
        Err(BitrouterError::Forbidden(
            "fixture forbids provider dispatch".into(),
        ))
    }
    async fn after_attempt(&self, _report: NativeAttemptReport) {}
}
fn prompt() -> Prompt {
    Prompt {
        model: MODEL.into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "bounded observation fixture")],
        tools: Vec::new(),
        params: Default::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    }
}
fn app(fixture: &Fixture) -> Result<App> {
    let config = crate::config::parse_resolved("inherit_defaults: false")?;
    Ok(App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(
                    crate::config::routing_table::ConfigRoutingTable::from_config(config),
                ))
                .executor(fixture.service.executor.clone());
        })
        .build()?
        .with_tenancy(fixture.service.clone()))
}

#[tokio::test]
async fn rejected_entry_outcome_can_prove_no_decision_but_rejected_policy_plan_must_settle()
-> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        for early in [true, false] {
            let (fixture, revision) =
                policy_fixture("https://example.invalid/v1", LearningMode::Observe).await?;
            let issued = fixture.issue("observation-rejection").await?;
            fixture
                .service
                .dispatch_test_ready
                .store(true, Ordering::Release);
            let headers = headers(
                &fixture,
                issued.secret.as_deref().context("secret")?,
                &revision,
            )?;
            let execution = admitted(&fixture, &headers).await?;
            let store = execution.eval_store();
            drop(execution);
            let control = Arc::new(Reject {
                early,
                entry_outcomes: AtomicUsize::new(0),
                plans: AtomicUsize::new(0),
            });
            let outcome = app(&fixture)?
                .execute_native_controlled_with_headers(
                    prompt(),
                    CallerContext::local(),
                    headers,
                    control.clone(),
                )
                .await;
            ensure!(outcome.is_err());
            ensure!(control.entry_outcomes.load(Ordering::SeqCst) == 1);
            ensure!(control.plans.load(Ordering::SeqCst) == usize::from(!early));
            fixture.service.pipelines.close_and_drain().await?;
            ensure!(store.list_subjects().await?.len() == usize::from(!early));
            ensure!(fixture.service.current().is_ok());
            fixture.service.shutdown().await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn rolled_back_required_observation_is_retained_as_failed_without_retry() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let (fixture, revision) = policy_fixture("https://example.invalid/v1", LearningMode::Observe).await?;
        let issued = fixture.issue("observation-storage-failure").await?;
        fixture.service.dispatch_test_ready.store(true, Ordering::Release);
        fixture.db.execute_unprepared("CREATE TRIGGER reject_observation BEFORE INSERT ON eval_subjects BEGIN SELECT RAISE(ABORT, 'fixture required observation failure'); END").await?;
        let headers = headers(&fixture, issued.secret.as_deref().context("secret")?, &revision)?;
        let control = Arc::new(Reject { early: false, entry_outcomes: AtomicUsize::new(0), plans: AtomicUsize::new(0) });
        ensure!(app(&fixture)?.execute_native_controlled_with_headers(prompt(), CallerContext::local(), headers, control.clone()).await.is_err());
        ensure!(control.plans.load(Ordering::SeqCst) == 1);
        ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
        ensure!(fixture.service.cleanup.disposition() == crate::tenancy::transaction::Disposition::RolledBack);
        ensure!(fixture.service.current().is_err());
        fixture.db.execute_unprepared("DROP TRIGGER reject_observation").await?;
        // Restored storage does not acknowledge the failed obligation or retry it.
        ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
        ensure!(fixture.service.shutdown().await.is_err());
        let row = fixture.db.query_one(Statement::from_string(fixture.db.get_database_backend(), "SELECT COUNT(*) AS total FROM eval_subjects".to_owned())).await?.context("observation count")?;
        ensure!(row.try_get::<i64>("", "total")? == 0);
        Ok::<_, anyhow::Error>(())
    }).await?
}

async fn full_observation_lane(expire: bool) -> Result<()> {
    let (fixture, revision) =
        policy_fixture("https://example.invalid/v1", LearningMode::Observe).await?;
    let issued = fixture.issue("observation-queue").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        issued.secret.as_deref().context("secret")?,
        &revision,
    )?;
    let execution = admitted(&fixture, &headers).await?;
    let store = execution.eval_store();
    drop(execution);
    // Occupy the actual bounded ordinary lane without inventing management jobs.
    let mut reservations = Vec::new();
    for _ in 0..16 {
        reservations.push(
            fixture
                .service
                .commands
                .clone()
                .try_reserve_owned()
                .map_err(|_| anyhow::anyhow!("fixture failed to reserve ordinary queue"))?,
        );
    }
    ensure!(fixture.service.commands.capacity() == 0);
    let control = Arc::new(Reject {
        early: false,
        entry_outcomes: AtomicUsize::new(0),
        plans: AtomicUsize::new(0),
    });
    let app = app(&fixture)?;
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        app.execute_native_controlled_with_headers(
            prompt(),
            CallerContext::local(),
            headers,
            control,
        )
        .await
    }));
    tokio::time::timeout(TIMEOUT, fixture.service.eval.producer_waiting.notified()).await?;
    ensure!(!task.is_finished());
    ensure!(fixture.service.current().is_ok());
    {
        let mut drain = Box::pin(fixture.service.pipelines.close_and_drain());
        ensure!(futures::poll!(&mut drain).is_pending());
    }
    if expire {
        tokio::time::pause();
        tokio::time::advance(crate::tenancy::eval::WORK + Duration::from_millis(1)).await;
        // Recorder completion, rather than elapsed time, proves the failed wait.
        let outcome = tokio::time::timeout(Duration::from_secs(1), task).await?;
        tokio::time::resume();
        ensure!(outcome?.is_err());
        drop(reservations);
        ensure!(fixture.service.current().is_err());
        ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
        ensure!(store.list_subjects().await?.is_empty());
        ensure!(fixture.service.shutdown().await.is_err());
    } else {
        drop(reservations);
        ensure!(tokio::time::timeout(TIMEOUT, task).await??.is_err());
        tokio::time::timeout(TIMEOUT, fixture.service.pipelines.close_and_drain()).await??;
        ensure!(fixture.service.current().is_ok());
        ensure!(store.list_subjects().await?.len() == 1);
        fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn full_ordinary_lane_waits_for_required_observation_without_losing_clean_drain() -> Result<()>
{
    tokio::time::timeout(TIMEOUT, full_observation_lane(false)).await?
}

#[tokio::test]
async fn expired_required_queue_reservation_fences_and_cannot_report_clean_drain() -> Result<()> {
    full_observation_lane(true).await
}
