//! A real storage fault cannot take the statement-refusal cleanup path.
use super::*;
use crate::tenancy::transaction::Disposition;

struct ReleaseGates {
    service: Service,
    plan: Arc<PlanGate>,
}
impl Drop for ReleaseGates {
    fn drop(&mut self) {
        self.plan.release.notify_one();
        self.service.cleanup.release_gate();
    }
}

#[tokio::test]
async fn readonly_required_observation_keeps_storage_fenced_after_rollback() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let (fixture, revision) =
            policy_fixture("https://example.invalid/v1", LearningMode::Observe).await?;
        let issued = fixture.issue("readonly-observation").await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = headers(
            &fixture,
            issued.secret.as_deref().context("secret")?,
            &revision,
        )?;
        let plan = Arc::new(PlanGate::default());
        let _release = ReleaseGates {
            service: fixture.service.clone(),
            plan: plan.clone(),
        };
        let control = Arc::new(Reject {
            early: false,
            entry_outcomes: AtomicUsize::new(0),
            plans: AtomicUsize::new(0),
            plan_gate: Some(plan.clone()),
        });
        let app = app(&fixture)?;
        let request_control = control.clone();
        let request = AbortOnDropHandle::new(tokio::spawn(async move {
            app.execute_native_controlled_with_headers(
                prompt(),
                CallerContext::local(),
                headers,
                request_control,
            )
            .await
        }));
        tokio::time::timeout(TIMEOUT, plan.entered.notified())
            .await
            .context("admitted invocation did not reach its policy plan")?;
        let proof = &fixture.service.cleanup;
        let budget = fixture.service.storage_budget();
        budget
            .reserve(0, true)
            .map_err(|error| anyhow::anyhow!("healthy observation budget: {error:?}"))?
            .complete()
            .map_err(|error| anyhow::anyhow!("healthy observation refund: {error:?}"))?;
        // The fixture has one SQLite connection. All admission work is done;
        // the next mutation is the required observation's actual INSERT.
        fixture
            .db
            .execute_unprepared("PRAGMA query_only=ON")
            .await?;
        let result: Result<()> = async {
            let gate = proof.hold_for(Phase::RollingBack, JobTarget::Settlement);
            plan.release.notify_one();
            tokio::time::timeout(TIMEOUT, gate.entered.notified())
                .await
                .context("readonly observation did not reach rollback")?;
            ensure!(proof.disposition() == Disposition::Pending);
            ensure!(fixture.service.current().is_err());
            #[cfg(unix)]
            {
                let (reserved, _, fenced) =
                    fixture
                        .service
                        .admission
                        .receipt_capacity_for_test()
                        .map_err(|error| anyhow::anyhow!("readonly capacity: {error:?}"))?;
                ensure!(reserved > 0 && fenced);
            }
            #[cfg(all(unix, any(feature = "otel-http", feature = "otel-grpc")))]
            {
                let held = budget
                    .calibration_observation(None)
                    .map_err(|error| anyhow::anyhow!("readonly reservation: {error:?}"))?;
                ensure!(held.fenced && held.physical_write_reserved > 0);
            }
            for maintenance in [false, true] {
                ensure!(matches!(
                    budget.reserve(0, maintenance),
                    Err(ErrorCode::StorageUnavailable)
                ));
            }
            let rollback_count = proof.sql_counts_for_test().1;
            proof.release_gate();
            ensure!(tokio::time::timeout(TIMEOUT, request).await??.is_err());
            ensure!(control.plans.load(Ordering::SeqCst) == 1);
            ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
            ensure!(proof.sql_counts_for_test().1 == rollback_count + 1);
            proof.ensure_clean()?;
            #[cfg(all(unix, any(feature = "otel-http", feature = "otel-grpc")))]
            {
                let settled = budget
                    .calibration_observation(None)
                    .map_err(|error| anyhow::anyhow!("readonly cleanup: {error:?}"))?;
                ensure!(settled.fenced && settled.physical_write_reserved == 0);
                ensure!(settled.completion_reserved > 0);
            }
            fixture
                .db
                .execute_unprepared("PRAGMA query_only=OFF")
                .await?;
            for maintenance in [false, true] {
                ensure!(matches!(
                    budget.reserve(0, maintenance),
                    Err(ErrorCode::StorageUnavailable)
                ));
            }
            ensure!(fixture.service.current().is_err());
            ensure!(!fixture.service.eval.healthy());
            ensure!(fixture.service.pipelines.close_and_drain().await.is_err());
            for table in ["eval_subjects", "tenancy_execution_receipts"] {
                let row = fixture
                    .db
                    .query_one(Statement::from_string(
                        fixture.db.get_database_backend(),
                        format!("SELECT COUNT(*) AS total FROM {table}"),
                    ))
                    .await?
                    .context("failed observation row count")?;
                ensure!(row.try_get::<i64>("", "total")? == 0);
            }
            Ok(())
        }
        .await;
        proof.release_gate();
        let restored = fixture.db.execute_unprepared("PRAGMA query_only=OFF").await;
        let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
        result?;
        restored?;
        ensure!(stopped?.is_err());
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
