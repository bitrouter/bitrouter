//! Native routing keeps a durable run identity across model steps.

use super::*;

#[derive(Clone, Default)]
struct Identities(Arc<Mutex<Vec<http::HeaderMap>>>);

#[async_trait]
impl ObserveHook for Identities {
    async fn after_phase(&self, _: Phase, _: &PipelineContext) {}
    async fn on_stream_part(&self, _: &StreamContext, _: &StreamPart) {}
    async fn on_request_end(&self, _: &PipelineContext, _: &RequestOutcome) {}
    async fn on_hop_start(&self, ctx: &PipelineContext, _: &RoutingTarget) {
        self.0.lock().await.push(ctx.headers().clone());
    }
}

#[tokio::test]
async fn native_routing_identity_is_stable_within_a_run_and_changes_for_new_work() -> TestResult {
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let identities = Identities::default();
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(vec![
                    output(vec![call("read")]),
                    output(vec![text("done")]),
                    output(vec![text("next")]),
                ])))
                .observe_hook(identities.clone());
        })
        .build()?;
    let harness = Arc::new(Harness::new(None, None));
    let session = bind_app(Arc::new(app), harness.clone()).await?;
    session.start("first", 1, input()).await?;
    let waiting = session.drive().await?;
    let run_id = waiting.run.as_ref().ok_or("missing run")?.run_id.clone();
    let tool = harness
        .sent
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("missing tool")?;
    session.tool_result("read-result", result(&tool)).await?;
    assert_eq!(
        session.drive().await?.run.map(|run| run.status),
        Some(RunStatus::Completed)
    );
    session
        .start("second", session.head().await.state_revision, input())
        .await?;
    let next = session.drive().await?;
    let headers = identities.0.lock().await;
    assert_eq!(headers.len(), 3);
    assert_eq!(headers[0]["x-bitrouter-parent-session-id"], run_id);
    assert_eq!(
        headers[0]["x-bitrouter-parent-session-id"],
        headers[1]["x-bitrouter-parent-session-id"]
    );
    assert_eq!(
        headers[0]["x-bitrouter-agent-session-id"],
        headers[1]["x-bitrouter-agent-session-id"]
    );
    assert_ne!(
        headers[0]["x-bitrouter-parent-session-id"],
        headers[2]["x-bitrouter-parent-session-id"]
    );
    assert_eq!(
        headers[2]["x-bitrouter-parent-session-id"],
        next.run.as_ref().ok_or("missing next run")?.run_id
    );
    Ok(())
}

#[tokio::test]
async fn explicit_embedding_workflow_identity_survives_native_routing() -> TestResult {
    for canonical in [false, true] {
        let table = StaticRoutingTable::new();
        table.insert("fixture-model", vec![target("first")]);
        let identities = Identities::default();
        let app = App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(table))
                    .executor(Arc::new(MockExecutor::new(vec![output(vec![text(
                        "done",
                    )])])))
                    .observe_hook(identities.clone());
            })
            .build()?;
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "x-bitrouter-workflow-session",
            http::HeaderValue::from_static("outer-workflow"),
        );
        if canonical {
            headers.insert(
                "x-bitrouter-parent-session-id",
                http::HeaderValue::from_static("explicit-parent"),
            );
            headers.insert(
                "x-bitrouter-agent-session-id",
                http::HeaderValue::from_static("explicit-agent"),
            );
        }
        let session = bind_app_with_headers(
            Arc::new(app),
            Arc::new(Harness::new(None, None)),
            Limits::default(),
            Vec::new(),
            headers,
        )
        .await?;
        session.start("input", 1, input()).await?;
        session.drive().await?;
        let observed = identities.0.lock().await;
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0]["x-bitrouter-parent-session-id"],
            if canonical {
                "explicit-parent"
            } else {
                "outer-workflow"
            }
        );
        assert_eq!(
            observed[0]["x-bitrouter-workflow-session"],
            "outer-workflow"
        );
        if canonical {
            assert_eq!(
                observed[0]["x-bitrouter-agent-session-id"],
                "explicit-agent"
            );
        }
    }
    Ok(())
}
