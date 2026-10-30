//! A later page is a new request even on an already initialized MCP worker.
use super::*;

#[tokio::test]
async fn native36_later_catalog_page_rechecks_authority_and_joins_the_open_worker() -> Result<()> {
    for change in ["allowed", "revoke", "scope"] {
        let provider = Provider::start(false).await?;
        let mut endpoint = Endpoint::start(true).await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let (fixture, route) = file_fixture(&provider, directory.path()).await?;
        let gate = fixture.service.hold_host(HostBoundary::Transport)?;
        let checked = tokio::time::timeout(Duration::from_secs(25), async {
            let (headers, key) = ready(&fixture, &route).await?;
            let app = Arc::new(
                host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone()),
            );
            let config = endpoint.config();
            let opener = AbortOnDropHandle::new(tokio::spawn(async move {
                NativeSession::open_authenticated(
                    app,
                    headers,
                    "later-page",
                    &workspace,
                    ToolMode::Coding,
                    &config,
                )
                .await
            }));
            let weak = observe(&gate).await?;
            endpoint.wire.entered.notified().await;
            ensure!(endpoint.wire.pages.load(Ordering::SeqCst) == 2);
            ensure!(!joined(&gate)? && fixture.service.host_slots_for_test() == 0);
            if change != "allowed" {
                change_authority(&fixture, &key, &route, change).await?;
            }
            // Before this point the test holds only weak witnesses. Keep the
            // completed state inspectable only after real custody is proved.
            let observed = weak.upgrade().context("catalog wait lost its MCP owner")?;
            ensure!(
                Arc::strong_count(&observed) > 1,
                "test became the sole MCP owner"
            );
            // Authority mutation must not discard the worker which owns the
            // outstanding second HTTP request; the third request is not admitted.
            ensure!(!joined(&gate)? && fixture.service.host_slots_for_test() == 0);
            ensure!(endpoint.wire.pages.load(Ordering::SeqCst) == 2);
            endpoint.wire.release.cancel();
            let opened = opener.await?;
            if change == "allowed" {
                let session = opened?;
                ensure!(
                    endpoint.wire.pages.load(Ordering::SeqCst) == 3,
                    "pagination probe did not reach its positive third page"
                );
                session.close().await?;
                ensure!(saved(&fixture).await?["journal"]["released"] == true);
            } else {
                ensure!(
                    opened.is_err(),
                    "{change}: unauthorized catalog became usable"
                );
                ensure!(
                    endpoint.wire.pages.load(Ordering::SeqCst) == 2,
                    "{change}: third page request escaped the current authority check"
                );
                let count = fixture
                    .db
                    .query_one(Statement::from_string(
                        DatabaseBackend::Sqlite,
                        "SELECT COUNT(*) AS count FROM bro_managed_sessions",
                    ))
                    .await?
                    .context("journal count")?
                    .try_get::<i64>("", "count")?;
                ensure!(count == 0, "rejected discovery created a native journal");
            }
            fixture.service.shutdown().await?;
            fixture.service.cleanup.ensure_clean()?;
            ensure!(joined(&gate)?, "actual worker join was not acknowledged");
            ensure!(fixture.service.host_slots_for_test() == 1);
            ensure!(provider.calls() == 0 && endpoint.wire.calls.load(Ordering::SeqCst) == 0);
            drop(observed);
            ensure!(
                gate.mcp
                    .get()
                    .context("owner observer")?
                    .upgrade()
                    .is_none()
            );
            Ok(())
        })
        .await
        .context("later catalog wait deadline")
        .and_then(|outcome| outcome);
        gate.release();
        endpoint.wire.release.cancel();
        let stopped = endpoint.finish().await;
        let cleaned = finish(&fixture, provider, checked).await;
        stopped?;
        cleaned?;
    }
    Ok(())
}
