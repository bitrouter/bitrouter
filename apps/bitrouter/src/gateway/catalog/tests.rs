use super::fetch::NetworkPolicy;
use super::store::{CatalogSnapshot, CatalogStore, Durability, StoreError};
use super::*;
use registry_json as serde_json;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

pub(super) trait ExactJsonBody {
    fn exact_json(self, value: Value) -> Self;
}
impl ExactJsonBody for ResponseTemplate {
    fn exact_json(self, value: Value) -> Self {
        self.set_body_raw(value.to_string(), "application/json")
    }
}

pub(crate) fn document(native: &str) -> Value {
    json!({"version":1,"spec_revision":bitrouter_registry::document::SPEC_REVISION,"as_of":"2026-10-22T00:00:00Z","data":{"models":[],"providers":[{"name":"fixture","status":"active","billing":"usage","api_base":"https://provider.example/v1","auth":{"kind":"none"},"models":[{"provider_model_id":native,"canonical_id":"org/model","status":"active","interfaces":[{"protocol":"messages","operation":"generation","capabilities":["tools"],"limits":{"max_output_tokens":128}},{"protocol":"chat_completions","operation":"generation","limits":{"max_output_tokens":32}}]}]}]}})
}
fn allowed() -> NetworkPolicy {
    NetworkPolicy::Allowed {
        request_timeout: Duration::from_secs(3),
    }
}

#[tokio::test]
async fn single_document_refresh_is_isolated_and_failed_candidates_keep_the_prior_view()
-> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/registry.json"))
        .respond_with(ResponseTemplate::new(200).exact_json(document("first")))
        .mount(&server)
        .await;
    let source = format!("{}/registry.json", server.uri());
    let mut catalog = Catalog::new(&source);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "authorization",
        reqwest::header::HeaderValue::from_static("Bearer inference-secret"),
    );
    let contaminated = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;
    assert_eq!(
        catalog.refresh(&contaminated, allowed()).await?,
        Durability::Memory
    );
    let prior = catalog
        .snapshot()
        .ok_or_else(|| anyhow::anyhow!("no admitted snapshot"))?
        .clone();
    let requests = server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("requests unavailable"))?;
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("authorization"));
    for status in [206, 302, 401, 500] {
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).exact_json(document("bad")))
            .mount(&server)
            .await;
        assert!(catalog.refresh(&contaminated, allowed()).await.is_err());
        assert_eq!(
            catalog.snapshot().map(|s| s.fetched_at),
            Some(prior.fetched_at)
        );
        assert_eq!(
            catalog.snapshot().and_then(|s| s.data.document.as_ref()),
            prior.data.document.as_ref()
        );
    }
    for mut value in [document("future"), document("old")] {
        server.reset().await;
        if value["data"]["providers"][0]["models"][0]["provider_model_id"] == "future" {
            value["version"] = json!(2);
        } else {
            value["as_of"] = json!("2026-10-21T00:00:00Z");
        }
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).exact_json(value))
            .mount(&server)
            .await;
        assert!(catalog.refresh(&contaminated, allowed()).await.is_err());
        assert_eq!(
            catalog.snapshot().and_then(|s| s.data.document.as_ref()),
            prior.data.document.as_ref()
        );
    }
    Ok(())
}

struct BrokenStore;
impl CatalogStore for BrokenStore {
    fn load(&self, _: &str) -> Result<Option<CatalogSnapshot>, StoreError> {
        Ok(None)
    }
    fn save(&self, _: &str, _: &CatalogSnapshot) -> Result<Durability, StoreError> {
        Err(StoreError::Unavailable)
    }
}
#[tokio::test]
async fn persistence_failure_keeps_admitted_memory_and_future_check_is_due() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).exact_json(document("memory")))
        .mount(&server)
        .await;
    let mut catalog = Catalog::with_store(server.uri(), Arc::new(BrokenStore));
    assert_eq!(
        catalog.refresh(&reqwest::Client::new(), allowed()).await?,
        Durability::Memory
    );
    assert!(catalog.is_fresh(Duration::from_secs(86400)));
    let mut snapshot = catalog
        .snapshot()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no snapshot"))?;
    snapshot.fetched_at = u64::MAX;
    catalog.seed(snapshot);
    assert!(!catalog.is_fresh(Duration::from_secs(86400)));
    assert!(
        catalog
            .refresh(&reqwest::Client::new(), NetworkPolicy::Offline)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn default_bundle_is_a_whole_alternative_and_local_loading_never_waits_for_http()
-> anyhow::Result<()> {
    use crate::config::RegistryConfig;
    let bundled = super::bundled::supplement(&RegistryConfig::default(), None)?
        .ok_or_else(|| anyhow::anyhow!("no bundle"))?;
    assert!(bundled.document.is_some());
    let remote = super::decode::from_slice(&serde_json::to_vec(
        &json!({"version":1,"spec_revision":bitrouter_registry::document::SPEC_REVISION,"as_of":"2026-10-22T00:00:00Z","data":{"models":[],"providers":[]}}),
    )?)?;
    assert!(
        super::bundled::supplement(&RegistryConfig::default(), Some(remote))?
            .is_some_and(|d| d.providers.is_empty())
    );
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .exact_json(document("late")),
        )
        .mount(&server)
        .await;
    let custom = RegistryConfig {
        url: Some(format!("{}/never-seen", server.uri())),
        ..Default::default()
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(100), super::bundled::load(&custom))
            .await?
            .is_none()
    );
    assert!(
        super::bundled::supplement(
            &RegistryConfig {
                enabled: false,
                ..Default::default()
            },
            None
        )?
        .is_none()
    );
    assert!(
        server
            .received_requests()
            .await
            .is_some_and(|r| r.is_empty())
    );
    Ok(())
}
