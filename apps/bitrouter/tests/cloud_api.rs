//! Binary-level coverage for `bitrouter cloud login --api-key` and
//! `bitrouter cloud api` protocol endpoints.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use bitrouter_cloud_sdk::auth::credentials::{Credentials, CredentialsStore, StoredCredential};
use chrono::{Duration, Utc};
use tempfile::TempDir;
use wiremock::matchers::{body_string, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const API_KEY: &str = "brk_test.fixture";

fn credentials_path(data_home: &Path) -> PathBuf {
    data_home.join("bitrouter/account-credentials.json")
}

fn save_api_key(data_home: &Path, base_url: &str) {
    let mut store = CredentialsStore::load(credentials_path(data_home)).unwrap();
    store
        .save(StoredCredential::api_key(
            API_KEY.to_owned(),
            base_url.to_owned(),
        ))
        .unwrap();
}

fn save_oauth(data_home: &Path, base_url: &str) {
    let mut store = CredentialsStore::load(credentials_path(data_home)).unwrap();
    store
        .save(Credentials {
            access_token: "oauth-integration-token".to_owned(),
            refresh_token: Some("oauth-refresh-token".to_owned()),
            expires_at: Utc::now() + Duration::hours(1),
            refresh_token_expires_at: None,
            token_type: "Bearer".to_owned(),
            scope: "inference:invoke".to_owned(),
            client_id: "bitrouter-cli".to_owned(),
            authorization_server: base_url.to_owned(),
            namespace_id: Some("ns-integration".to_owned()),
            subject: Some("user-integration".to_owned()),
        })
        .unwrap();
}

fn run_cli(data_home: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bitrouter"));
    command
        .args(args)
        .env("XDG_DATA_HOME", data_home)
        .env_remove("BITROUTER_OAUTH_AS")
        .env_remove("BITROUTER_OAUTH_CLIENT_ID")
        .env_remove("BITROUTER_OAUTH_SCOPE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(input) = stdin {
        command.stdin(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        return child.wait_with_output().unwrap();
    }
    command.output().unwrap()
}

#[tokio::test]
async fn api_key_login_and_models_request_share_credentials() {
    let server = MockServer::start().await;
    let data_home = TempDir::new().unwrap();
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", format!("Bearer {API_KEY}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("{\"object\":\"list\",\"data\":[]}", "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let login = run_cli(
        data_home.path(),
        &[
            "cloud",
            "login",
            "--api-key",
            API_KEY,
            "--oauth-as",
            &server.uri(),
        ],
        None,
    );

    assert!(
        login.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&login.stderr)
    );
    let login_stdout = String::from_utf8(login.stdout).unwrap();
    let login_stderr = String::from_utf8(login.stderr).unwrap();
    assert!(login_stdout.contains("\"authentication\": \"api_key\""));
    assert!(!login_stdout.contains(API_KEY));
    assert!(!login_stderr.contains(API_KEY));

    let whoami = run_cli(data_home.path(), &["cloud", "whoami"], None);
    assert!(
        whoami.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&whoami.stderr)
    );
    assert!(String::from_utf8_lossy(&whoami.stdout).contains("\"authentication\": \"api_key\""));
    assert!(
        !whoami
            .stdout
            .windows(API_KEY.len())
            .any(|value| value == API_KEY.as_bytes())
    );
    assert!(
        !whoami
            .stderr
            .windows(API_KEY.len())
            .any(|value| value == API_KEY.as_bytes())
    );

    let models = run_cli(data_home.path(), &["cloud", "api", "/v1/models"], None);

    assert!(
        models.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&models.stderr)
    );
    assert_eq!(models.stdout, b"{\"object\":\"list\",\"data\":[]}");
    assert!(models.stderr.is_empty());
}

#[tokio::test]
async fn both_credential_types_cover_generation_protocol_matrix() {
    let request = b"{\n  \"model\": \"test/model\",\n  \"input\": \"hello\"\n}\n";
    let cases = [
        (
            "/v1/chat/completions",
            "{\"kind\":\"chat_completions\"}",
            "application/json",
        ),
        (
            "/v1/messages",
            "{\"kind\":\"messages\"}",
            "application/json",
        ),
        (
            "/v1/responses",
            "{\"kind\":\"responses\"}",
            "application/json",
        ),
        (
            "/v1beta/models/google/gemini-2.5-flash:generateContent",
            "{\"kind\":\"generateContent\"}",
            "application/json",
        ),
        (
            "/v1beta/models/google/gemini-2.5-flash:streamGenerateContent",
            "data: {\"text\":\"hello\"}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ),
    ];
    for oauth in [false, true] {
        let server = MockServer::start().await;
        let data_home = TempDir::new().unwrap();
        let expected_bearer = if oauth {
            save_oauth(data_home.path(), &server.uri());
            "oauth-integration-token"
        } else {
            save_api_key(data_home.path(), &server.uri());
            API_KEY
        };

        for (endpoint, response, content_type) in cases {
            Mock::given(method("POST"))
                .and(path(endpoint))
                .and(header("authorization", format!("Bearer {expected_bearer}")))
                .and(body_string(String::from_utf8(request.to_vec()).unwrap()))
                .respond_with(ResponseTemplate::new(200).set_body_raw(response, content_type))
                .expect(1)
                .mount(&server)
                .await;

            let output = run_cli(
                data_home.path(),
                &["cloud", "api", endpoint, "--input", "-"],
                Some(request),
            );

            assert!(
                output.status.success(),
                "oauth={oauth} endpoint={endpoint} stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, response.as_bytes(), "endpoint={endpoint}");
            assert!(output.stderr.is_empty());
        }
    }
}

#[tokio::test]
async fn fresh_oauth_credential_authenticates_without_discovery() {
    let server = MockServer::start().await;
    let data_home = TempDir::new().unwrap();
    save_oauth(data_home.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer oauth-integration-token"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("oauth-ok", "text/plain"))
        .expect(1)
        .mount(&server)
        .await;

    let output = run_cli(data_home.path(), &["cloud", "api", "/v1/models"], None);

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"oauth-ok");
    assert!(output.stderr.is_empty());
}

#[tokio::test]
async fn http_error_keeps_body_and_returns_non_zero_without_leaking_key() {
    let server = MockServer::start().await;
    let data_home = TempDir::new().unwrap();
    save_api_key(data_home.path(), &server.uri());
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_raw("{\"error\":\"rate_limited\"}", "application/json"),
        )
        .mount(&server)
        .await;

    let output = run_cli(data_home.path(), &["cloud", "api", "/v1/models"], None);

    assert!(!output.status.success());
    assert_eq!(output.stdout, b"{\"error\":\"rate_limited\"}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("429"), "{stderr}");
    assert!(!stderr.contains(API_KEY));
}
