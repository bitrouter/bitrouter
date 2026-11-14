use super::*;
use crate::gateway::model::{
    protocol::{InboundAdapter, inbound_adapter_for},
    target::{AuthScheme, CredentialPriority},
};
use serde_json::json;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

fn target() -> ModelTarget {
    ModelTarget {
        provider_name: "paired".into(),
        service_id: "native/model?not-a-path=1".into(),
        api_protocol: PairedTransport.protocol(),
        api_base: "https://paired.invalid/v1/private/".into(),
        api_key: "bridge-key".into(),
        credential_priority: CredentialPriority::Explicit,
        account_label: None,
        auth_scheme: AuthScheme::Bearer,
        compatibility: Default::default(),
    }
}

#[test]
fn paired_envelope_separates_model_mode_and_native_fields() -> TestResult {
    let source = json!({
        "contents":[{"role":"user","parts":[{"text":"hello","thoughtSignature":"proof"}]}],
        "cachedContent":"cachedContents/native-selector",
        "generationConfig":{"temperature":0.25}
    });
    let mut input = source.clone();
    input["model"] = "retained-route".into();
    input["stream"] = true.into();
    let mut prompt = GenerateContentAdapter.parse_request(input)?;
    prompt.model = target().service_id;
    let encoded = Adapter.render_request(&prompt)?;
    let envelope: OrderedGenerateContentRequestV1 = serde_json::from_value(encoded)?;
    envelope.validate()?;
    assert_eq!(envelope.model, "native/model?not-a-path=1");
    assert!(envelope.stream);
    assert_eq!(serde_json::to_value(envelope.request)?, source);
    assert!(inbound_adapter_for(&Adapter.protocol()).is_none());
    assert!(inbound_adapter_for(&super::super::protocol()).is_none());
    Ok(())
}

#[tokio::test]
async fn paired_transport_has_one_exact_endpoint_and_private_mime() -> TestResult {
    let target = target();
    let endpoint = "https://paired.invalid/v1/private/generate-content";
    assert_eq!(PairedTransport.endpoint_url(&target, false), endpoint);
    assert_eq!(PairedTransport.endpoint_url(&target, true), endpoint);
    let request = reqwest::Request::new(reqwest::Method::POST, endpoint.parse()?);
    let request = PairedTransport.authorise(request, &target).await?;
    assert_eq!(request.url().as_str(), endpoint);
    assert_eq!(request.headers()[reqwest::header::CONTENT_TYPE], MIME);
    assert_eq!(
        request.headers()[reqwest::header::AUTHORIZATION],
        "Bearer bridge-key"
    );
    assert!(!request.headers().contains_key("x-goog-api-key"));
    assert!(request.headers()[reqwest::header::AUTHORIZATION].is_sensitive());
    let mut invalid = target;
    invalid.api_key = "invalid\nkey".into();
    assert!(PairedTransport.authorise(request, &invalid).await.is_err());
    Ok(())
}
