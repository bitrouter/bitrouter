use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::fetch::{FetchError, NetworkPolicy};
use super::store::{CatalogSnapshot, CatalogStore, Durability, MemoryCatalogStore, StoreError};
use super::{Catalog, CatalogError, types};

fn providers() -> Value {
    json!({"data":[{
        "name":"fixture-provider", "status":"active", "api_base":"https://fixture.invalid/v1",
        "auth":{"kind":"bearer", "env":"FIXTURE_API_KEY"},
        "models":[{"id":"fixture/model", "provider_model_id":"upstream-model",
            "api_protocol":["responses","openai"], "capabilities":["tools"],
            "pricing":{"input_tokens":{"no_cache":1.25}}}]
    }]})
}

fn old_snapshot() -> Result<CatalogSnapshot, Box<dyn std::error::Error>> {
    let provider: types::Envelope<types::RegistryProvider> = serde_json::from_value(providers())?;
    Ok(CatalogSnapshot {
        fetched_at: 0,
        data: types::RegistryData {
            providers: provider.data,
            canonical: vec![types::CanonicalModel {
                id: "old/model".into(),
                ..Default::default()
            }],
        },
    })
}

fn allowed() -> NetworkPolicy {
    NetworkPolicy::Allowed {
        request_timeout: Duration::from_secs(3),
    }
}

async fn artifacts(server: &MockServer, provider: Value, model: Value) {
    Mock::given(method("GET"))
        .and(path("/providers.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(provider))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(model))
        .mount(server)
        .await;
}

#[tokio::test]
async fn empty_catalog_load_and_offline_refresh_perform_no_discovery()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    let mut catalog = Catalog::new(server.uri());
    assert!(catalog.snapshot().is_none());
    catalog.load()?;
    assert!(matches!(
        catalog
            .refresh(&reqwest::Client::new(), NetworkPolicy::Offline)
            .await,
        Err(CatalogError::Fetch(FetchError::Offline))
    ));
    assert!(catalog.snapshot().is_none());
    assert!(
        server
            .received_requests()
            .await
            .ok_or("missing request inventory")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn refresh_publishes_the_complete_pair_and_reports_memory_durability()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    artifacts(
        &server,
        providers(),
        json!({"data":[{"id":"fixture/model"}]}),
    )
    .await;
    let store = Arc::new(MemoryCatalogStore::default());
    let mut catalog = Catalog::with_store(format!("{}/", server.uri()), store.clone());
    assert_eq!(
        catalog.refresh(&reqwest::Client::new(), allowed()).await?,
        Durability::Memory
    );
    let data = &catalog.snapshot().ok_or("missing refreshed catalog")?.data;
    assert_eq!(data.canonical[0].id, "fixture/model");
    assert_eq!(
        data.providers[0].models[0].api_protocol.to_vec(),
        [
            types::RegistryProtocol::Responses,
            types::RegistryProtocol::Openai
        ]
    );
    assert_eq!(
        data.providers[0].models[0].capabilities,
        [crate::types::Capability::Tools]
    );
    assert!(catalog.is_fresh(Duration::from_secs(60)));
    let mut reader = Catalog::with_store(server.uri(), store);
    reader.load()?;
    assert_eq!(
        reader
            .snapshot()
            .ok_or("missing saved snapshot")?
            .data
            .canonical[0]
            .id,
        "fixture/model"
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .ok_or("missing request inventory")?
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn second_artifact_failure_retains_prior_data_timestamp_and_storage()
-> Result<(), Box<dyn std::error::Error>> {
    for bad_json in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/providers.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(providers()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models.json"))
            .respond_with(if bad_json {
                ResponseTemplate::new(200).set_body_string("incomplete json")
            } else {
                ResponseTemplate::new(503).set_body_string("not a usable catalog")
            })
            .mount(&server)
            .await;
        let store = Arc::new(MemoryCatalogStore::default());
        store.save(&server.uri(), &old_snapshot()?)?;
        let mut catalog = Catalog::with_store(server.uri(), store.clone());
        catalog.load()?;
        assert!(!catalog.is_fresh(Duration::from_secs(60)));
        assert!(matches!(
            catalog.refresh(&reqwest::Client::new(), allowed()).await,
            Err(CatalogError::Fetch(_))
        ));
        assert_eq!(
            catalog.snapshot().ok_or("prior catalog lost")?.fetched_at,
            0
        );
        assert_eq!(
            catalog
                .snapshot()
                .ok_or("prior catalog lost")?
                .data
                .canonical[0]
                .id,
            "old/model"
        );
        assert_eq!(
            store
                .load(&server.uri())?
                .ok_or("prior stored catalog lost")?
                .data
                .canonical[0]
                .id,
            "old/model"
        );
    }
    Ok(())
}

struct FailingStore {
    memory: MemoryCatalogStore,
    fail_save: AtomicBool,
}
impl CatalogStore for FailingStore {
    fn load(&self, source: &str) -> Result<Option<CatalogSnapshot>, StoreError> {
        self.memory.load(source)
    }
    fn save(&self, source: &str, snapshot: &CatalogSnapshot) -> Result<Durability, StoreError> {
        if self.fail_save.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        self.memory.save(source, snapshot)
    }
}

#[tokio::test]
async fn persistence_failure_is_explicit_and_does_not_publish_or_erase_the_snapshot()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    artifacts(
        &server,
        providers(),
        json!({"data":[{"id":"fixture/model"}]}),
    )
    .await;
    let store = Arc::new(FailingStore {
        memory: MemoryCatalogStore::default(),
        fail_save: AtomicBool::new(false),
    });
    store.save(&server.uri(), &old_snapshot()?)?;
    let mut catalog = Catalog::with_store(server.uri(), store.clone());
    catalog.load()?;
    store.fail_save.store(true, Ordering::SeqCst);
    assert!(matches!(
        catalog.refresh(&reqwest::Client::new(), allowed()).await,
        Err(CatalogError::Storage(StoreError::Unavailable))
    ));
    assert_eq!(
        catalog
            .snapshot()
            .ok_or("lost prior snapshot")?
            .data
            .canonical[0]
            .id,
        "old/model"
    );
    store.fail_save.store(false, Ordering::SeqCst);
    assert_eq!(
        catalog.refresh(&reqwest::Client::new(), allowed()).await?,
        Durability::Memory
    );
    assert_eq!(
        catalog
            .snapshot()
            .ok_or("missing replacement")?
            .data
            .canonical[0]
            .id,
        "fixture/model"
    );
    Ok(())
}

#[tokio::test]
async fn invalid_refresh_retains_data_and_empty_storage_stays_unavailable_on_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    let mut duplicate = providers();
    duplicate["data"]
        .as_array_mut()
        .ok_or("missing provider list")?
        .push(providers()["data"][0].clone());
    artifacts(&server, duplicate, json!({"data":[{"id":"fixture/model"}]})).await;
    let store = Arc::new(MemoryCatalogStore::default());
    store.save(&server.uri(), &old_snapshot()?)?;
    let mut catalog = Catalog::with_store(server.uri(), store);
    catalog.load()?;
    assert!(matches!(
        catalog.refresh(&reqwest::Client::new(), allowed()).await,
        Err(CatalogError::InvalidData)
    ));
    assert_eq!(
        catalog.snapshot().ok_or("lost prior data")?.data.canonical[0].id,
        "old/model"
    );
    let mut empty = Catalog::new(server.uri());
    assert!(matches!(
        empty.refresh(&reqwest::Client::new(), allowed()).await,
        Err(CatalogError::InvalidData)
    ));
    assert!(empty.snapshot().is_none());
    Ok(())
}

#[test]
fn explicit_store_load_is_source_bound_and_rejects_invalid_replacements()
-> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(MemoryCatalogStore::default());
    store.save("first", &old_snapshot()?)?;
    let mut catalog = Catalog::with_store("first", store.clone());
    assert!(catalog.snapshot().is_none());
    catalog.load()?;
    let mut other = Catalog::with_store("second", store.clone());
    other.load()?;
    assert!(other.snapshot().is_none());
    let mut invalid = old_snapshot()?;
    invalid
        .data
        .canonical
        .push(invalid.data.canonical[0].clone());
    store.save("first", &invalid)?;
    assert!(matches!(catalog.load(), Err(CatalogError::InvalidData)));
    assert_eq!(
        catalog
            .snapshot()
            .ok_or("lost valid snapshot")?
            .data
            .canonical
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn injected_client_requests_remain_bounded() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/providers.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(providers()),
        )
        .mount(&server)
        .await;
    let mut catalog = Catalog::new(server.uri());
    let result = catalog
        .refresh(
            &reqwest::Client::new(),
            NetworkPolicy::Allowed {
                request_timeout: Duration::from_millis(50),
            },
        )
        .await;
    assert!(
        matches!(result,Err(CatalogError::Fetch(FetchError::Network(error))) if error.is_timeout())
    );
    assert!(catalog.snapshot().is_none());
    let requests = server
        .received_requests()
        .await
        .ok_or("missing request inventory")?;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/providers.json");
    Ok(())
}

#[tokio::test]
async fn cancellation_after_observed_fetch_keeps_the_prior_snapshot()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    let admitted = Arc::new(tokio::sync::Notify::new());
    let observer = admitted.clone();
    Mock::given(method("GET"))
        .and(path("/providers.json"))
        .respond_with(move |_: &wiremock::Request| {
            observer.notify_one();
            ResponseTemplate::new(200)
                .set_body_json(providers())
                .set_delay(Duration::from_secs(2))
        })
        .mount(&server)
        .await;
    let store = Arc::new(MemoryCatalogStore::default());
    store.save(&server.uri(), &old_snapshot()?)?;
    let mut catalog = Catalog::with_store(server.uri(), store.clone());
    catalog.load()?;
    let client = reqwest::Client::new();
    let mut refresh = Box::pin(catalog.refresh(&client, allowed()));
    tokio::select! {
        _ = admitted.notified() => {},
        result = &mut refresh => return Err(format!("refresh ended before cancellation: {result:?}").into()),
    }
    drop(refresh);
    assert_eq!(
        catalog
            .snapshot()
            .ok_or("lost prior snapshot")?
            .data
            .canonical[0]
            .id,
        "old/model"
    );
    assert_eq!(
        store
            .load(&server.uri())?
            .ok_or("lost stored snapshot")?
            .data
            .canonical[0]
            .id,
        "old/model"
    );
    let requests = server
        .received_requests()
        .await
        .ok_or("missing observed fetch")?;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/providers.json");
    Ok(())
}
