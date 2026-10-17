//! Aggregate admission narrows existing Core routes without changing identity.
use super::*;
use bitrouter_sdk::language_model::native::{
    NativeAttemptReport, NativeExecutionControl, NativePlan, NativePlanAdmission,
};

struct Admission {
    indices: Vec<u32>,
    observed: Mutex<Vec<u32>>,
}

#[async_trait]
impl NativeExecutionControl for Admission {
    async fn plan(&self, _: NativePlan) -> bitrouter_sdk::Result<NativePlanAdmission> {
        Ok(NativePlanAdmission {
            route_indices: self.indices.clone(),
        })
    }

    async fn before_attempt(&self, _: &str, index: u32) -> bitrouter_sdk::Result<()> {
        if self.indices.contains(&index) {
            Ok(())
        } else {
            Err(bitrouter_sdk::BitrouterError::bad_request(
                "fixture host did not admit this route",
            ))
        }
    }

    async fn after_attempt(&self, report: NativeAttemptReport) {
        self.observed.lock().await.push(report.attempt_index);
    }
}

#[tokio::test]
async fn narrowed_routes_preserve_core_order_and_fallback_report_identity() -> TestResult {
    let admission = Arc::new(Admission {
        indices: vec![1, 2],
        observed: Mutex::new(Vec::new()),
    });
    let mut harness = Harness::new(None, None);
    harness.model_control = Some(admission.clone());
    let table = StaticRoutingTable::new();
    table.insert(
        "fixture-model",
        vec![target("first"), target("second"), target("third")],
    );
    let executor = Arc::new(RecordingExecutor {
        mock: MockExecutor::new(vec![
            MockResponse::Error(bitrouter_sdk::BitrouterError::Upstream {
                status: 503,
                message: "fixture fallback".into(),
            }),
            output(vec![text("host-admitted fallback")]),
        ]),
        agent_once: Mutex::new(Default::default()),
        prompts: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .build()?;
    let session = bind_app(Arc::new(app), Arc::new(harness)).await?;
    session
        .start("start", session.head().await.state_revision, input())
        .await?;
    let state = session.drive().await?;
    assert_eq!(
        state.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let step = &state.root_turn().ok_or("missing root turn")?.steps[0];
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert_eq!(*admission.observed.lock().await, [1, 2]);
    assert_eq!(
        step.attempts
            .iter()
            .map(|attempt| attempt.index)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    let providers = step
        .attempts
        .iter()
        .map(|attempt| {
            attempt
                .receipt
                .as_ref()
                .map(|receipt| receipt.report.route.provider.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(providers, [Some("second"), Some("third")]);
    assert!(
        step.decision.as_ref().ok_or("missing decision")?.routes[0]
            .rejection_reasons
            .iter()
            .any(|reason| reason == "host_admission_rejected")
    );
    session.disconnect().await;
    Ok(())
}

#[tokio::test]
async fn host_cannot_reorder_duplicate_or_invent_routes() -> TestResult {
    for indices in [vec![1, 0], vec![0, 0], vec![2], vec![]] {
        let mut harness = Harness::new(None, None);
        harness.model_control = Some(Arc::new(Admission {
            indices,
            observed: Mutex::new(Vec::new()),
        }));
        let (session, executor, _) = setup(
            vec![output(vec![text("must not dispatch")])],
            Arc::new(harness),
            true,
        )
        .await?;
        session
            .start("start", session.head().await.state_revision, input())
            .await?;
        let _ = session.drive().await;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        session.disconnect().await;
    }
    Ok(())
}
