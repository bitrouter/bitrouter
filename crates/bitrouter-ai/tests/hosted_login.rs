#![cfg(feature = "hosted-login")]

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

mod hosted {
    use super::TestResult;
    use bitrouter_ai::providers::hosted::{flow, metadata::AsMetadata};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn inputs(server: &MockServer) -> (AsMetadata, flow::LoginParams) {
        (
            AsMetadata {
                issuer: Some(server.uri()),
                device_authorization_endpoint: format!("{}/device", server.uri()),
                token_endpoint: format!("{}/token", server.uri()),
                revocation_endpoint: Some(format!("{}/revoke", server.uri())),
            },
            flow::LoginParams {
                authorization_server: server.uri(),
                client_id: "explicit-client".into(),
                scope: "explicit-scope".into(),
            },
        )
    }

    #[tokio::test]
    async fn hosted_expiration_prevents_a_poll_after_the_deadline() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/device"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code":"device", "user_code":"user",
                "verification_uri":"https://selected.invalid/verify",
                "expires_in":1, "interval":10
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token":"too-late", "expires_in":3600
            })))
            .expect(0)
            .mount(&server)
            .await;
        let (metadata, params) = inputs(&server);
        let result =
            flow::run_device_flow(&reqwest::Client::new(), &metadata, &params, |_| {}).await;
        assert!(result.is_err());
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn hosted_http_failure_cannot_return_a_token_or_echo_secrets() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(path("/token"))
            .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "access_token":"response-secret", "error_description":"another-secret"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (metadata, params) = inputs(&server);
        let result =
            flow::poll_token_endpoint(&reqwest::Client::new(), &metadata, &params, "device").await;
        let error = result.err().ok_or("HTTP failure returned a token")?;
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("response-secret"));
        assert!(!diagnostic.contains("another-secret"));
        Ok(())
    }

    #[tokio::test]
    async fn hosted_login_and_revoke_keep_full_envelope_and_explicit_inputs() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(path("/device"))
            .and(wiremock::matchers::body_string_contains(
                "client_id=explicit-client",
            ))
            .and(wiremock::matchers::body_string_contains(
                "scope=explicit-scope",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code":"device", "user_code":"user",
                "verification_uri":"https://selected.invalid/verify", "expires_in":10, "interval":1
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/token"))
            .and(wiremock::matchers::body_string_contains("device_code=device"))
            .and(wiremock::matchers::body_string_contains("client_id=explicit-client"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token":"AT", "refresh_token":"RT", "expires_in":3600,
                "refresh_token_expires_in":7200, "scope":"granted-scope", "token_type":"Bearer",
                "namespace_id":"issued-namespace", "id_token":"e30.eyJzdWIiOiJzdWJqZWN0In0.signature"
            }))).expect(1).mount(&server).await;
        let ready = std::sync::atomic::AtomicUsize::new(0);
        let (metadata, params) = inputs(&server);
        let client = reqwest::Client::new();
        let tokens = flow::run_device_flow(&client, &metadata, &params, |device| {
            assert_eq!(device.user_code, "user");
            ready.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
        .await?;
        assert_eq!(ready.load(std::sync::atomic::Ordering::SeqCst), 1);
        let credential = flow::credentials_from_token_set(tokens, &params);
        assert_eq!(credential.access_token, "AT");
        assert_eq!(credential.refresh_token.as_deref(), Some("RT"));
        assert_eq!(credential.scope, "granted-scope");
        assert_eq!(credential.namespace_id.as_deref(), Some("issued-namespace"));
        assert_eq!(credential.subject.as_deref(), Some("subject"));
        assert_eq!(credential.authorization_server, params.authorization_server);
        assert_eq!(credential.client_id, params.client_id);
        assert!(credential.refresh_token_expires_at.is_some());
        Mock::given(path("/revoke"))
            .and(wiremock::matchers::body_string_contains("token=RT"))
            .and(wiremock::matchers::body_string_contains(
                "client_id=explicit-client",
            ))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        flow::revoke(
            &client,
            &format!("{}/revoke", server.uri()),
            &params.client_id,
            "RT",
            "refresh_token",
        )
        .await?;
        server.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn hosted_expiration_and_cancellation_bound_in_flight_polling() -> TestResult {
        for cancel in [false, true] {
            let server = MockServer::start().await;
            Mock::given(path("/device"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "device_code":"device", "user_code":"user",
                    "verification_uri":"https://selected.invalid/verify", "expires_in":2, "interval":1
                }))).expect(1).mount(&server).await;
            let observed = std::sync::Arc::new(tokio::sync::Notify::new());
            let response_observed = observed.clone();
            Mock::given(path("/token"))
                .respond_with(move |_request: &wiremock::Request| {
                    response_observed.notify_one();
                    ResponseTemplate::new(200)
                        .set_delay(std::time::Duration::from_secs(5))
                        .set_body_json(serde_json::json!({"access_token":"too-late"}))
                })
                .expect(1)
                .mount(&server)
                .await;
            let (metadata, params) = inputs(&server);
            let task = tokio::spawn(async move {
                flow::run_device_flow(&reqwest::Client::new(), &metadata, &params, |_| {}).await
            });
            tokio::time::timeout(std::time::Duration::from_secs(3), observed.notified()).await?;
            if cancel {
                task.abort();
                assert!(
                    task.await
                        .err()
                        .ok_or("cancelled flow completed")?
                        .is_cancelled()
                );
            } else {
                assert!(task.await?.is_err());
            }
            server.verify().await;
        }
        Ok(())
    }
}
