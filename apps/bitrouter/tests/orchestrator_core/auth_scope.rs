//! Account selectors participate in production Responses continuation identity.
//! https://developers.openai.com/api/reference/resources/responses/methods/create

use anyhow::{Context, Result};
use axum_test::TestServer;
use bitrouter_sdk::server::{AppState, build_router};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn configured_scopes_support_first_response_and_bind_continuations() -> Result<()> {
    for passthrough in [false, true] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"resp_scope_fixture", "object":"response", "status":"completed", "model":"served",
                "output":[{"id":"msg_scope", "type":"message", "role":"assistant", "status":"completed",
                    "content":[{"type":"output_text", "text":"ok", "annotations":[]}]}],
                "usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5}
            })))
            .mount(&upstream).await;
        let source = format!(
            r#"
inherit_defaults: false
registry:
  enabled: false
server:
  skip_auth: true
database:
  url: 'sqlite::memory:'
providers:
  scoped:
    api_base: {}/v1
    api_key: fixture-key
    headers:
      openai-organization:
        default: default-org
        passthrough: {passthrough}
      openai-project:
        default: default-project
        passthrough: {passthrough}
    models:
      - id: served
        api_protocol: responses
models:
  scoped:
    endpoints:
      - {{provider: scoped, service_id: served}}
"#,
            upstream.uri()
        );
        let home = tempfile::tempdir()?;
        let config_path = home.path().join("bitrouter.yaml");
        std::fs::write(&config_path, &source)?;
        let config = bitrouter_sdk::config::parse_with(&source, |_| None)?;
        let assembled =
            bitrouter::assemble::build_app_with_path(&config, Some(&config_path)).await?;
        let gateway = TestServer::builder()
            .http_transport()
            .try_build(build_router(AppState {
                language_model: assembled
                    .app
                    .language_model()
                    .context("missing pipeline")?
                    .clone(),
                mcp: assembled.app.mcp().cloned(),
                skip_auth: assembled.app.skip_auth(),
                metrics_renderer: assembled.app.metrics_renderer().cloned(),
                prompt_transforms: assembled.app.prompt_transforms().to_vec(),
            }))?;
        let first = gateway
            .post("/v1/responses")
            .add_header("openai-organization", "caller-org")
            .add_header("openai-project", "caller-project")
            .json(&json!({"model":"scoped","input":"hello","stream":false}))
            .await;
        assert_eq!(first.status_code().as_u16(), 200, "{}", first.text());
        let first: Value = first.json();
        let public_id = first["id"].as_str().context("missing public id")?;
        assert_ne!(public_id, "resp_scope_fixture");
        let followup = json!({"model":"scoped", "input":"continue", "stream":false, "previous_response_id":public_id});
        let same = gateway
            .post("/v1/responses")
            .add_header("openai-organization", "caller-org")
            .add_header("openai-project", "caller-project")
            .json(&followup)
            .await;
        assert_eq!(same.status_code().as_u16(), 200, "{}", same.text());
        let requests = upstream
            .received_requests()
            .await
            .context("missing requests")?;
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(request.headers["authorization"], "Bearer fixture-key");
            assert_eq!(
                request.headers["openai-organization"],
                if passthrough {
                    "caller-org"
                } else {
                    "default-org"
                }
            );
            assert_eq!(
                request.headers["openai-project"],
                if passthrough {
                    "caller-project"
                } else {
                    "default-project"
                }
            );
        }
        assert_eq!(
            requests[1].body_json::<Value>()?["previous_response_id"],
            "resp_scope_fixture"
        );
        if passthrough {
            for (organization, project) in [
                ("other-org", "caller-project"),
                ("caller-org", "other-project"),
            ] {
                let changed = gateway
                    .post("/v1/responses")
                    .add_header("openai-organization", organization)
                    .add_header("openai-project", project)
                    .json(&followup)
                    .await;
                assert_eq!(changed.status_code().as_u16(), 400, "{}", changed.text());
            }
            assert_eq!(
                upstream
                    .received_requests()
                    .await
                    .context("missing requests")?
                    .len(),
                2
            );
            let new_scope = gateway
                .post("/v1/responses")
                .add_header("openai-organization", "other-org")
                .add_header("openai-project", "other-project")
                .json(&json!({"model":"scoped","input":"new task","stream":false}))
                .await;
            assert_eq!(
                new_scope.status_code().as_u16(),
                200,
                "{}",
                new_scope.text()
            );
        }
    }
    Ok(())
}
