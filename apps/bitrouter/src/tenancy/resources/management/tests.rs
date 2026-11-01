use super::*;
use anyhow::Result;
use serde_json::json;

fn route(endpoint: &str, headers: serde_json::Value) -> Result<compiler::RouteEnvelope> {
    let import: Import = serde_json::from_value(json!({
        "reference":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route","resource_id":"route","revision_id":"v1"},
        "definition":{"kind":"route","selector":"provider:model","policy":null,
            "routing":{"providers":{"provider":{"api_base":endpoint,"headers":headers,"models":[{"id":"model"}]}}},
            "credentials":[{"provider":"provider","account":"default","credential":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential","resource_id":"slot","revision_id":"v1"}}]
        }
    }))?;
    compiler::prepare_route(&import, None)
}

#[test]
fn approved_origin_is_exact_and_cannot_be_overridden_by_headers() -> Result<()> {
    let mut resources = Resources::with_key("installation".try_into()?, [43; 32])?;
    resources.approved_origins = vec!["https://example.com".into()];
    let caller = Caller::Credential("tenant-key".try_into()?);
    let allowed = route(
        "https://EXAMPLE.com/v1",
        json!({"anthropic-version":"2023-06-01"}),
    )?;
    assert!(resources.authorize_endpoints(&caller, &allowed).is_ok());
    // The shared upstream validator rejects plaintext non-loopback URLs before
    // retained-origin authorization. It must not be bypassed for this test.
    assert!(route("http://example.com/v1", json!({})).is_err());
    for endpoint in [
        "https://example.com:8443/v1",
        "https://sub.example.com/v1",
        "https://other.invalid/v1",
    ] {
        let denied = route(endpoint, json!({}))?;
        assert_eq!(
            resources.authorize_endpoints(&caller, &denied),
            Err(ErrorCode::Forbidden)
        );
    }
    for name in [
        "host",
        "authorization",
        "x-api-key",
        "x-forwarded-host",
        "connection",
        "cookie",
    ] {
        // The compiler may reject a reserved header before the network-authority
        // check; neither boundary may admit its use by a tenant route.
        let mut headers = serde_json::Map::new();
        headers.insert(name.into(), "attacker.invalid".into());
        if let Ok(envelope) = route("https://example.com/v1", headers.into()) {
            assert!(
                resources.authorize_endpoints(&caller, &envelope).is_err(),
                "header {name}"
            );
        }
    }
    Ok(())
}

#[test]
fn endpoints_never_embed_secrets_even_for_operator_authored_routes() -> Result<()> {
    let resources = Resources::with_key("installation".try_into()?, [43; 32])?;
    let operator = Caller::Installation("operator".try_into()?);
    for endpoint in [
        "https://user:secret@example.com/v1",
        "https://example.com/v1?key=secret",
        "https://example.com/v1#secret",
    ] {
        if let Ok(envelope) = route(endpoint, json!({})) {
            assert_eq!(
                resources.authorize_endpoints(&operator, &envelope),
                Err(ErrorCode::InvalidRequest)
            );
        }
    }
    Ok(())
}
