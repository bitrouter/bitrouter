//! Retired execution is rejected; historical catalog/credential bytes survive.

#[cfg(feature = "file-store")]
use bitrouter_ai::auth::file::snapshot::CredentialStore;
use bitrouter_ai::client::{HttpTimeouts, ModelClient};
use bitrouter_ai::protocol::{InboundAdapter, chat_completions::ChatCompletionsAdapter};
use bitrouter_ai::target::ModelTarget;
use bitrouter_ai::types::ApiProtocol;
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[test]
fn historical_protocol_and_credentials_remain_readable_but_uncallable() -> TestResult {
    #[cfg(feature = "file-store")]
    let (_directory, path, bytes) = {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("credentials.json");
        let bytes = serde_json::to_vec(
            &json!({"google-ai":{"work":{"type":"oauth","data":{"access_token":"fixture-access","refresh_token":"fixture-refresh","expires_at":10}}}}),
        )?;
        std::fs::write(&path, &bytes)?;
        let saved = CredentialStore::load(&path)?;
        assert!(saved.get_any("google-ai", "work").is_some());
        assert_eq!(std::fs::read(&path)?, bytes);
        (directory, path, bytes)
    };
    let protocol: ApiProtocol = serde_json::from_value(json!("generate_content"))?;
    assert_eq!(protocol, ApiProtocol::Custom("generate_content".into()));
    let selected = ModelTarget {
        provider_name: "historical".into(),
        service_id: "saved-model".into(),
        api_protocol: protocol,
        api_base: "http://127.0.0.1:1".into(),
        api_key: "fixture".into(),
        credential_priority: Default::default(),
        account_label: None,
        auth_scheme: Default::default(),
        compatibility: Default::default(),
    };
    assert!(
        ModelClient::new(HttpTimeouts::default())?
            .render_request(
                &selected,
                &ChatCompletionsAdapter.parse_request(
                    json!({"model":"fixture","messages":[{"role":"user","content":"saved"}]})
                )?,
                false
            )
            .is_err()
    );
    #[cfg(feature = "file-store")]
    assert_eq!(std::fs::read(&path)?, bytes);
    Ok(())
}
