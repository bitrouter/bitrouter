//! Real TLS HTTP exchanges; keys and certificates are fixture-only public data.
use bitrouter_registry::feed::Transport;
use registry_json as serde_json;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::{TlsAcceptor, rustls};

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;
struct Fixture {
    url: String,
    root: Vec<u8>,
    requests: Arc<Mutex<Vec<String>>>,
    job: tokio::task::JoinHandle<TestResult>,
}
async fn fixture(
    replies: Vec<(u16, String)>,
) -> Result<Fixture, Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("https://127.0.0.1:{}/models", listener.local_addr()?.port());
    // Private ephemeral CA; a short-lived leaf also obeys macOS trust policy.
    // https://docs.rs/rcgen/0.14/rcgen/struct.CertificateParams.html
    let now = time::OffsetDateTime::now_utc();
    let mut root_params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    root_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Registry fixture root");
    root_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    root_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    root_params.not_before = now - time::Duration::days(1);
    root_params.not_after = now + time::Duration::days(365);
    let root_key = rcgen::KeyPair::generate()?;
    let root = root_params.self_signed(&root_key)?.der().to_vec();
    let issuer = rcgen::Issuer::from_params(&root_params, &root_key);
    let mut params = rcgen::CertificateParams::new(vec!["127.0.0.1".into()])?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Registry fixture server");
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(30);
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    let signing_key = rcgen::KeyPair::generate()?;
    let cert = params.signed_by(&signing_key, &issuer)?.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
    // Workspace feature unification can enable both Rustls crypto providers.
    // https://docs.rs/rustls/0.23/rustls/struct.ServerConfig.html#method.builder_with_provider
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(vec![cert], key.into())?;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let job = tokio::spawn(async move {
        for (status, body) in replies {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).await?;
                if count == 0 || request.len() > 65536 {
                    return Err("invalid fixture HTTP request".into());
                }
                request.extend_from_slice(&buffer[..count]);
            }
            recorded
                .lock()
                .map_err(|_| "poisoned fixture requests")?
                .push(String::from_utf8(request)?);
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nLocation: https://example.com/forbidden\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.shutdown().await?;
        }
        Ok(())
    });
    Ok(Fixture {
        url,
        root,
        requests,
        job,
    })
}
fn transport(root: &[u8]) -> Result<Transport, Box<dyn std::error::Error + Send + Sync>> {
    Ok(Transport::new([reqwest::Certificate::from_der(root)?])?)
}
fn feed(url: &str) -> Value {
    json!({"id":"list","kind":"v1_models","writes":["models"],"request":{"url":url,"auth":{"kind":"header","env":"FIXTURE_KEY","header":"x-api-key"}},"pagination":{"kind":"cursor","cursor_path":"/next","has_more_path":"/more","parameter":"after"},"inventory":{"mode":"snapshot","scope":"public","complete":true},"response":{"items_path":"/data","mapping":{"/id":"/provider_model_id"},"defaults":{"status":"active","interfaces":[]}}})
}

#[tokio::test]
async fn paginated_authenticated_listing_is_complete_before_admission() -> TestResult {
    let fixture = fixture(vec![
        (
            200,
            json!({"data":[{"id":"Native/A"}],"more":true,"next":"opaque /?%"}).to_string(),
        ),
        (
            200,
            json!({"data":[{"id":"Native/B"}],"more":false}).to_string(),
        ),
    ])
    .await?;
    let observation = transport(&fixture.root)?
        .observe(&feed(&fixture.url), |name| {
            (name == "FIXTURE_KEY").then(|| "fixture-token".into())
        })
        .await?;
    assert_eq!(observation.models.len(), 2);
    fixture.job.await??;
    let requests = fixture.requests.lock().map_err(|_| "requests lock")?;
    assert!(requests[0].starts_with("GET /models HTTP/1.1"));
    assert!(requests[1].starts_with("GET /models?after=opaque+%2F%3F%25 HTTP/1.1"));
    assert!(
        requests
            .iter()
            .all(|r| r.contains("x-api-key: fixture-token\r\n")
                && !r.to_lowercase().contains("authorization:"))
    );
    Ok(())
}
#[tokio::test]
async fn later_page_auth_failure_and_partial_or_redirect_response_never_form_inventory()
-> TestResult {
    for status in [401, 206, 302] {
        let fixture = fixture(vec![
            (
                200,
                json!({"data":[{"id":"A"}],"more":true,"next":"two"}).to_string(),
            ),
            (status, json!({"data":[],"more":false}).to_string()),
        ])
        .await?;
        let error = transport(&fixture.root)?
            .observe(&feed(&fixture.url), |_| Some("fixture-token".into()))
            .await
            .err()
            .ok_or("partial response accepted")?;
        assert!(!error.to_string().contains("fixture-token"));
        fixture.job.await??;
        assert_eq!(
            fixture.requests.lock().map_err(|_| "requests lock")?.len(),
            2
        );
    }
    Ok(())
}
#[tokio::test]
async fn missing_secret_and_invalid_inventory_do_not_send_any_request() -> TestResult {
    let fixture = fixture(vec![]).await?;
    let config = feed(&fixture.url);
    let error = transport(&fixture.root)?
        .observe(&config, |_| None)
        .await
        .err()
        .ok_or("missing secret accepted")?;
    assert_eq!(error.code, "missing_secret");
    let mut invalid = config;
    invalid["inventory"]["allow_empty"] = json!("true");
    assert!(
        transport(&fixture.root)?
            .observe(&invalid, |_| Some("fixture-token".into()))
            .await
            .is_err()
    );
    fixture.job.await??;
    assert!(
        fixture
            .requests
            .lock()
            .map_err(|_| "requests lock")?
            .is_empty()
    );
    Ok(())
}
