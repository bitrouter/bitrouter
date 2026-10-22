//! Host-owned background lifecycle. No request path triggers a registry fetch.
use super::{cache, fetch::NetworkPolicy, store::CatalogSnapshot};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

const FAILURE_RETRY: Duration = Duration::from_secs(60 * 60);

pub(crate) async fn run(
    reloader: Arc<crate::reload::AppReloader>,
    ready: Arc<tokio::sync::Notify>,
    stop: CancellationToken,
) {
    tokio::select! {_=stop.cancelled()=>return,_=ready.notified()=>{}}
    run_with_intervals(reloader, stop, cache::TTL, FAILURE_RETRY).await;
}

async fn run_with_intervals(
    reloader: Arc<crate::reload::AppReloader>,
    stop: CancellationToken,
    success: Duration,
    failure: Duration,
) {
    let client = reqwest::Client::new();
    loop {
        let (epoch, settings) = reloader.registry_settings();
        let Some(settings) = settings.filter(|s| s.enabled && s.auto_refresh) else {
            tokio::select! {_=stop.cancelled()=>return,_=reloader.registry_changed()=>continue}
        };
        let source = settings.source().to_owned();
        let mut catalog = cache::cached_catalog(&source);
        if catalog.snapshot().is_none() {
            match super::bundled::supplement(&settings, None) {
                Ok(Some(data)) => catalog.seed(CatalogSnapshot {
                    fetched_at: 0,
                    data,
                }),
                Ok(None) => {}
                Err(error) => tracing::warn!(%error,"registry bundle unavailable"),
            }
        }
        let wait = catalog
            .snapshot()
            .filter(|_| catalog.is_fresh(success))
            .map(|snapshot| {
                success.saturating_sub(Duration::from_secs(
                    super::unix_seconds().saturating_sub(snapshot.fetched_at),
                ))
            })
            .unwrap_or(Duration::ZERO);
        if !wait.is_zero() {
            tokio::select! {_=stop.cancelled()=>return,_=reloader.registry_changed()=>continue,_=tokio::time::sleep(wait)=>{}}
        }
        loop {
            let refreshed = tokio::select! {
                _=stop.cancelled()=>return,
                _=reloader.registry_changed()=>break,
                result=catalog.download(&client,NetworkPolicy::Allowed{request_timeout:Duration::from_secs(15)})=>result,
            };
            let next = match refreshed {
                Ok(snapshot) => loop {
                    match reloader
                        .publish_registry(epoch, &source, &snapshot, &mut catalog)
                        .await
                    {
                        Ok(durability) => {
                            tracing::info!(?durability, "registry refreshed; routing view updated");
                            break success;
                        }
                        Err(crate::reload::RegistryPublishError::Busy) => {
                            tokio::select! {_=stop.cancelled()=>return,_=reloader.registry_changed()=>break Duration::ZERO,_=tokio::time::sleep(Duration::from_secs(1))=>{}}
                        }
                        Err(crate::reload::RegistryPublishError::Superseded) => {
                            break Duration::ZERO;
                        }
                        Err(error) => {
                            tracing::warn!(%error,"registry routing publication rejected; retaining prior view");
                            break failure;
                        }
                    }
                },
                Err(error) => {
                    tracing::warn!(%error,"registry refresh failed; retaining admitted view; upgrade software if the contract is unsupported");
                    failure
                }
            };
            if next.is_zero() {
                break;
            }
            tokio::select! {_=stop.cancelled()=>return,_=reloader.registry_changed()=>break,_=tokio::time::sleep(next)=>{}}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::ExactJsonBody;
    use super::*;
    use crate::daemon::DaemonReloader;
    use crate::gateway::{
        model::client::HttpTimeouts,
        pipeline::{executor::HttpExecutor, routing::RoutingPrefs},
    };
    use crate::{
        config::routing_table::ConfigRoutingTable,
        policy::store::PolicyStore,
        reload::{AppReloader, ReloadSource},
    };
    use registry_json as serde_json;
    use serde_json::json;
    use std::sync::{
        RwLock,
        atomic::{AtomicUsize, Ordering},
    };
    use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::method};

    fn yaml(url: &str, auto: bool, marker: &str) -> String {
        format!(
            "registry:\n  url: '{url}'\n  auto_refresh: {auto}\npresets:\n  saved:\n    model: 'manual:{marker}'\nproviders:\n  manual:\n    api_base: 'https://manual.example/v1'\n    models: [{{id: '{marker}'}}]\n"
        )
    }
    async fn fixture(
        url: &str,
    ) -> anyhow::Result<(tempfile::TempDir, Arc<AppReloader>, Arc<ConfigRoutingTable>)> {
        let home = tempfile::tempdir()?;
        let path = home.path().join("bitrouter.yaml");
        std::fs::write(&path, yaml(url, true, "applied"))?;
        let baseline = crate::reload::load_configuration_baseline(
            &crate::paths::ConfigSource::File(path.clone()),
        )
        .await?;
        let raw = baseline.config().clone();
        let table = Arc::new(ConfigRoutingTable::from_config(raw));
        let executor = Arc::new(HttpExecutor::new(HttpTimeouts::default())?);
        let reloader = Arc::new(
            AppReloader::new(
                Arc::new(PolicyStore::new()),
                table.clone(),
                executor,
                ReloadSource::File(path),
            )
            .with_startup_configuration(baseline),
        );
        Ok((home, reloader, table))
    }
    async fn wait_model(table: &ConfigRoutingTable, native: &str) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if crate::config::routing_table::resolve_route_chain(
                    &table.snapshot_config(),
                    "org/model",
                    &RoutingPrefs::default(),
                )
                .ok()
                .is_some_and(|t| t.iter().any(|t| t.service_id == native))
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("registry worker did not publish expected route"))
    }
    #[tokio::test]
    async fn worker_waits_for_readiness_then_updates_without_inference() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).exact_json(super::super::tests::document("ready")),
            )
            .mount(&server)
            .await;
        let (_home, reloader, table) = fixture(&format!("{}/ready", server.uri())).await?;
        let ready = Arc::new(tokio::sync::Notify::new());
        let stop = CancellationToken::new();
        let _guard = stop.clone().drop_guard();
        let worker = tokio::spawn(run(reloader, ready.clone(), stop.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            server
                .received_requests()
                .await
                .is_some_and(|r| r.is_empty())
        );
        ready.notify_one();
        wait_model(&table, "ready").await?;
        stop.cancel();
        worker.await?;
        Ok(())
    }
    #[tokio::test]
    async fn periodic_refresh_retains_on_failure_and_does_not_apply_pending_disk_edits()
    -> anyhow::Result<()> {
        let server = MockServer::start().await;
        let state = Arc::new(RwLock::new((200, super::super::tests::document("one"))));
        let count = Arc::new(AtomicUsize::new(0));
        let response = state.clone();
        let observed = count.clone();
        Mock::given(method("GET"))
            .respond_with(move |_: &Request| {
                observed.fetch_add(1, Ordering::SeqCst);
                let state = response.read().unwrap_or_else(|p| p.into_inner());
                ResponseTemplate::new(state.0).exact_json(state.1.clone())
            })
            .mount(&server)
            .await;
        let source = format!("{}/periodic", server.uri());
        let (home, reloader, table) = fixture(&source).await?;
        let stop = CancellationToken::new();
        let _guard = stop.clone().drop_guard();
        let worker = tokio::spawn(run_with_intervals(
            reloader.clone(),
            stop.clone(),
            Duration::from_millis(150),
            Duration::from_millis(300),
        ));
        wait_model(&table, "one").await?;
        std::fs::write(
            home.path().join("bitrouter.yaml"),
            yaml(&source, true, "pending"),
        )?;
        *state.write().unwrap_or_else(|p| p.into_inner()) =
            (200, super::super::tests::document("two"));
        wait_model(&table, "two").await?;
        assert_eq!(
            table.snapshot_config().presets["saved"].model.as_deref(),
            Some("manual:applied")
        );
        *state.write().unwrap_or_else(|p| p.into_inner()) = (500, json!({"error":"unavailable"}));
        let current = count.load(Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(2), async {
            while count.load(Ordering::SeqCst) == current {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let failed = count.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(count.load(Ordering::SeqCst), failed);
        wait_model(&table, "two").await?;
        *state.write().unwrap_or_else(|p| p.into_inner()) =
            (200, super::super::tests::document("recovered"));
        wait_model(&table, "recovered").await?;
        reloader
            .reload()
            .await
            .map_err(|error| anyhow::anyhow!("{error}; report={:?}", reloader.reload_state()))?;
        assert_eq!(
            table.snapshot_config().presets["saved"].model.as_deref(),
            Some("manual:pending")
        );
        stop.cancel();
        worker.await?;
        Ok(())
    }

    #[tokio::test]
    async fn production_schedule_waits_24_hours_after_success_and_one_hour_after_failure()
    -> anyhow::Result<()> {
        let server = MockServer::start().await;
        let state = Arc::new(RwLock::new((200, super::super::tests::document("initial"))));
        let count = Arc::new(AtomicUsize::new(0));
        let response = state.clone();
        let observed = count.clone();
        Mock::given(method("GET"))
            .respond_with(move |_: &Request| {
                observed.fetch_add(1, Ordering::SeqCst);
                let state = response.read().unwrap_or_else(|p| p.into_inner());
                ResponseTemplate::new(state.0).exact_json(state.1.clone())
            })
            .mount(&server)
            .await;
        let (_home, reloader, table) = fixture(&format!("{}/clock", server.uri())).await?;
        let stop = CancellationToken::new();
        let _guard = stop.clone().drop_guard();
        let worker = tokio::spawn(run_with_intervals(
            reloader,
            stop.clone(),
            cache::TTL,
            FAILURE_RETRY,
        ));
        wait_model(&table, "initial").await?;
        tokio::time::sleep(Duration::from_millis(30)).await;
        *state.write().unwrap_or_else(|p| p.into_inner()) = (500, json!({}));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(24 * 60 * 60 - 1)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(2), async {
            while count.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        tokio::time::sleep(Duration::from_millis(30)).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(60 * 60 - 1)).await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        *state.write().unwrap_or_else(|p| p.into_inner()) =
            (200, super::super::tests::document("recovered"));
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::resume();
        wait_model(&table, "recovered").await?;
        assert_eq!(count.load(Ordering::SeqCst), 3);
        stop.cancel();
        worker.await?;
        Ok(())
    }
    #[tokio::test]
    async fn source_change_fences_a_late_response_and_disabled_refresh_removes_old_contribution()
    -> anyhow::Result<()> {
        let a = MockServer::start().await;
        let b = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(500))
                    .exact_json(super::super::tests::document("late-a")),
            )
            .mount(&a)
            .await;
        let (home, reloader, table) = fixture(&format!("{}/a", a.uri())).await?;
        let stop = CancellationToken::new();
        let _guard = stop.clone().drop_guard();
        let worker = tokio::spawn(run_with_intervals(
            reloader.clone(),
            stop.clone(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if a.received_requests().await.is_some_and(|r| !r.is_empty()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        std::fs::write(
            home.path().join("bitrouter.yaml"),
            yaml(&format!("{}/b", b.uri()), false, "new-source"),
        )?;
        reloader
            .reload()
            .await
            .map_err(|error| anyhow::anyhow!("{error}; report={:?}", reloader.reload_state()))?;
        tokio::time::sleep(Duration::from_millis(650)).await;
        assert!(b.received_requests().await.is_some_and(|r| r.is_empty()));
        assert!(!table.snapshot_config().providers.contains_key("fixture"));
        stop.cancel();
        worker.await?;
        Ok(())
    }
}
