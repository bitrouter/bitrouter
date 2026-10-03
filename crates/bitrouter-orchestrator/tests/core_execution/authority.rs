//! Authority can expire after an intent was accepted but before its ACK returns.

use super::*;

#[tokio::test]
async fn authority_revoked_during_ack_fences_model_and_tool_dispatch() -> TestResult {
    for barrier in ["model.attempt.intent", "model.output.applied"] {
        let harness = Arc::new(Harness::new(None, Some(barrier)));
        let (session, executor, _) =
            setup(vec![output(vec![call("pending")])], harness.clone(), false).await?;
        session.start("input", 1, input()).await?;
        let driver = tokio::spawn({
            let session = session.clone();
            async move { session.drive().await }
        });
        tokio::time::timeout(Duration::from_secs(60), harness.seen.acquire())
            .await??
            .forget();
        let prior = session.head().await;
        harness.dispatch_allowed.store(false, Ordering::SeqCst);
        harness.hold_enabled.store(false, Ordering::SeqCst);
        harness.resume.add_permits(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), driver)
                .await??
                .is_err()
        );
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            usize::from(barrier == "model.output.applied")
        );
        assert!(harness.sent.lock().await.is_empty());
        let head = session.head().await;
        assert!(head.state_revision > prior.state_revision);
        assert_eq!(head, harness.store.lock().await.head);
        // The accepted intent/output survives. A denied dispatch neither
        // erases that checkpoint nor silently reopens the execution gate.
        assert!(session.drive().await.is_err());
        assert_eq!(session.head().await, head);
    }
    Ok(())
}
