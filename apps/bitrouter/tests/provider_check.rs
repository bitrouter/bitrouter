//! Model-list checks for a configured custom provider.
//!
//! These cover the public check: local and online endpoints, protocol auth,
//! unresolved references, and a result that does not rewrite configuration.

use anyhow::{Result, anyhow, ensure};
use bitrouter::config::parse_with;
use bitrouter::providers::check::{check_document, check_loaded};

fn lookup_none(name: &str) -> Option<String> {
    let _ = name;
    None
}

async fn catalog(server: &wiremock::MockServer, body: serde_json::Value) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/models"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn requests(server: &wiremock::MockServer) -> Result<Vec<wiremock::Request>> {
    server
        .received_requests()
        .await
        .ok_or_else(|| anyhow!("wiremock server dropped its requests"))
}

fn header<'a>(request: &'a wiremock::Request, name: &str) -> Result<&'a str> {
    request
        .headers
        .get(name)
        .ok_or_else(|| anyhow!("missing {name}"))?
        .to_str()
        .map_err(anyhow::Error::from)
}

#[tokio::test]
async fn local_chat_list_reports_models_without_changing_config_or_generating() -> Result<()> {
    let server = wiremock::MockServer::start().await;
    catalog(
        &server,
        serde_json::json!({"data": [{"id": "live-a"}, {"id": "live-b"}]}),
    )
    .await;
    let yaml = format!(
        "providers:\n  localbox:\n    api_base: {base}/v1\n    api_key: secret-key\n    auto_discover: true\n    headers:\n      X-Lab: bench\n    models:\n      - id: stale\n",
        base = server.uri()
    );
    let path =
        std::env::temp_dir().join(format!("bitrouter-provider-check-{}", std::process::id()));
    std::fs::create_dir_all(&path)?;
    let file = path.join("bitrouter.yaml");
    std::fs::write(&file, &yaml)?;
    let before = std::fs::read(&file)?;
    let config = parse_with(&yaml, lookup_none)?;
    let report = check_loaded(&config, "localbox", None).await?;
    let after_config = parse_with(&yaml, lookup_none)?;
    std::fs::write(&file, std::fs::read_to_string(&file)?)?;
    let after = std::fs::read(&file)?;
    std::fs::remove_dir_all(&path)?;

    ensure!(before == after, "check rewrote the config file");
    let original = config
        .providers
        .get("localbox")
        .ok_or_else(|| anyhow!("missing provider"))?;
    let reread = after_config
        .providers
        .get("localbox")
        .ok_or_else(|| anyhow!("missing provider"))?;
    ensure!(original.active);
    ensure!(reread.active);
    ensure!(original.models.len() == 1 && original.models[0].id == "stale");
    ensure!(reread.models[0].id == "stale");
    ensure!(original.auto_discover);
    ensure!(report.connections.len() == 1);
    let connection = &report.connections[0];
    ensure!(connection.id == "localbox");
    ensure!(connection.answered);
    ensure!(connection.protocol == "chat_completions");
    ensure!(connection.models == ["live-a".to_string(), "live-b".to_string()]);
    ensure!(connection.error.is_none());
    let encoded = serde_json::to_string(&report)?;
    ensure!(!encoded.contains("secret-key"));

    let seen = requests(&server).await?;
    ensure!(seen.len() == 1, "auto_discover must not add a second probe");
    ensure!(seen[0].method == http::Method::GET);
    ensure!(seen[0].url.path() == "/v1/models");
    ensure!(header(&seen[0], "authorization")? == "Bearer secret-key");
    ensure!(header(&seen[0], "x-lab")? == "bench");
    Ok(())
}

#[tokio::test]
async fn messages_and_responses_use_that_protocols_auth_on_the_same_list() -> Result<()> {
    let messages = wiremock::MockServer::start().await;
    let responses = wiremock::MockServer::start().await;
    catalog(
        &messages,
        serde_json::json!({"data": [{"id": "claude-local"}]}),
    )
    .await;
    catalog(
        &responses,
        serde_json::json!({"data": [{"id": "gpt-local"}]}),
    )
    .await;
    let yaml = format!(
        "providers:\n  \
         localmsg:\n    api_base: {messages}/v1\n    api_key: msg-key\n    api_protocol:\n      - \"*\": messages\n  \
         localresp:\n    api_base: {responses}/v1\n    api_key: resp-key\n    api_protocol:\n      - \"*\": responses\n",
        messages = messages.uri(),
        responses = responses.uri()
    );
    let msg = check_document(&yaml, "localmsg", None, lookup_none).await?;
    let resp = check_document(&yaml, "localresp", None, lookup_none).await?;
    ensure!(
        msg.connections[0].answered && msg.connections[0].models == ["claude-local".to_string()]
    );
    ensure!(msg.connections[0].protocol == "messages");
    ensure!(resp.connections[0].answered && resp.connections[0].protocol == "responses");
    let msg_reqs = requests(&messages).await?;
    let resp_reqs = requests(&responses).await?;
    ensure!(header(&msg_reqs[0], "x-api-key")? == "msg-key");
    ensure!(header(&msg_reqs[0], "anthropic-version")? == "2023-06-01");
    ensure!(msg_reqs[0].headers.get("authorization").is_none());
    ensure!(header(&resp_reqs[0], "authorization")? == "Bearer resp-key");
    ensure!(msg_reqs.iter().chain(resp_reqs.iter()).all(|request| {
        request.method == http::Method::GET && request.url.path() == "/v1/models"
    }));
    Ok(())
}

#[tokio::test]
async fn online_https_and_local_http_share_the_check() -> Result<()> {
    let server = wiremock::MockServer::start().await;
    catalog(&server, serde_json::json!({"data": [{"id": "llama"}]})).await;
    let yaml = format!(
        "providers:\n  \
         laptop:\n    api_base: {base}/v1\n    api_key: local-key\n    timeouts:\n      connect_secs: 1\n      read_secs: 1\n  \
         online:\n    api_base: https://example.invalid/v1\n    api_key: online-key\n    timeouts:\n      connect_secs: 1\n      read_secs: 1\n",
        base = server.uri()
    );
    let local = check_document(&yaml, "laptop", None, lookup_none).await?;
    let remote = check_document(&yaml, "online", None, lookup_none).await?;
    ensure!(local.connections[0].answered && local.connections[0].protocol == "chat_completions");
    ensure!(!remote.connections[0].answered);
    ensure!(remote.connections[0].protocol == "chat_completions");
    ensure!(remote.connections[0].models.is_empty());
    let error = remote.connections[0].error.clone().unwrap_or_default();
    ensure!(
        error.contains("could not reach") || error.contains("timed out"),
        "{error}"
    );
    ensure!(!error.contains("online-key"));
    Ok(())
}

#[tokio::test]
async fn unresolved_credential_fails_before_any_request() -> Result<()> {
    let server = wiremock::MockServer::start().await;
    catalog(
        &server,
        serde_json::json!({"data": [{"id": "should-not-see"}]}),
    )
    .await;
    let yaml = format!(
        "providers:\n  localbox:\n    api_base: {base}/v1\n    api_key: ${{CHECK_KEY}}\n",
        base = server.uri()
    );
    let report = check_document(&yaml, "localbox", None, lookup_none).await?;
    ensure!(!report.connections[0].answered);
    ensure!(report.connections[0].models.is_empty());
    let error = report.connections[0].error.clone().unwrap_or_default();
    ensure!(error.contains("${CHECK_KEY}"), "{error}");
    ensure!(requests(&server).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn another_providers_unresolved_reference_does_not_block_this_check() -> Result<()> {
    let server = wiremock::MockServer::start().await;
    catalog(&server, serde_json::json!({"data": [{"id": "ready"}]})).await;
    let yaml = format!(
        "providers:\n  other:\n    api_base: ${{OTHER_BASE}}\n    api_key: ${{OTHER_KEY}}\n  localbox:\n    api_base: {base}/v1\n    api_key: secret-key\n",
        base = server.uri()
    );
    let report = check_document(&yaml, "localbox", None, lookup_none).await?;
    ensure!(report.connections[0].answered);
    ensure!(report.connections[0].models == ["ready".to_string()]);
    ensure!(requests(&server).await?.len() == 1);
    Ok(())
}

#[tokio::test]
async fn a_non_list_or_error_status_does_not_count_as_answered_or_unroutable() -> Result<()> {
    let bad = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"models": ["nope"]})),
        )
        .mount(&bad)
        .await;
    let denied = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(401).set_body_string("secret-key rejected"))
        .mount(&denied)
        .await;
    let yaml = format!(
        "providers:\n  badlist:\n    api_base: {bad}/v1\n    api_key: secret-key\n    active: true\n    models:\n      - id: kept\n  denied:\n    api_base: {denied}/v1\n    api_key: secret-key\n    active: true\n",
        bad = bad.uri(),
        denied = denied.uri()
    );
    let config = parse_with(&yaml, lookup_none)?;
    let bad_report = check_loaded(&config, "badlist", None).await?;
    let denied_report = check_loaded(&config, "denied", None).await?;
    ensure!(!bad_report.connections[0].answered && bad_report.connections[0].models.is_empty());
    ensure!(!denied_report.connections[0].answered);
    let denied_error = denied_report.connections[0]
        .error
        .clone()
        .unwrap_or_default();
    ensure!(denied_error.contains("401"), "{denied_error}");
    ensure!(!denied_error.contains("secret-key"));
    let kept = config
        .providers
        .get("badlist")
        .ok_or_else(|| anyhow!("missing"))?;
    ensure!(kept.active && kept.models[0].id == "kept");
    Ok(())
}

#[tokio::test]
async fn an_empty_catalog_is_an_answer_and_a_protocol_without_one_is_not_dialed() -> Result<()> {
    let server = wiremock::MockServer::start().await;
    catalog(&server, serde_json::json!({"data": []})).await;
    let quiet = wiremock::MockServer::start().await;
    let yaml = format!(
        "providers:\n  empty:\n    api_base: {server}/v1\n    api_key: secret-key\n  widget:\n    api_base: {quiet}/v1\n    api_key: secret-key\n    api_protocol:\n      - \"*\": widget\n",
        server = server.uri(),
        quiet = quiet.uri()
    );
    let empty = check_document(&yaml, "empty", None, lookup_none).await?;
    let widget = check_document(&yaml, "widget", None, lookup_none).await?;
    ensure!(empty.connections[0].answered && empty.connections[0].models.is_empty());
    ensure!(!widget.connections[0].answered);
    let error = widget.connections[0].error.clone().unwrap_or_default();
    ensure!(error.contains("widget"), "{error}");
    ensure!(requests(&quiet).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn accounts_are_separate_connections_and_one_label_limits_the_check() -> Result<()> {
    let work = wiremock::MockServer::start().await;
    let home = wiremock::MockServer::start().await;
    catalog(&work, serde_json::json!({"data": [{"id": "work-model"}]})).await;
    catalog(&home, serde_json::json!({"data": [{"id": "home-model"}]})).await;
    let yaml = format!(
        "providers:\n  groqbox:\n    api_base: https://api.groq.com/openai/v1\n    accounts:\n      - label: work\n        api_key: work-key\n        api_base: {work}/v1\n      - label: home\n        api_key: home-key\n        api_base: {home}/v1\n",
        work = work.uri(),
        home = home.uri()
    );
    let both = check_document(&yaml, "groqbox", None, lookup_none).await?;
    ensure!(both.connections.len() == 2);
    ensure!(
        both.connections[0].id == "groqbox/work"
            && both.connections[0].models == ["work-model".to_string()]
    );
    ensure!(
        both.connections[1].id == "groqbox/home"
            && both.connections[1].models == ["home-model".to_string()]
    );
    ensure!(header(&requests(&work).await?[0], "authorization")? == "Bearer work-key");
    ensure!(header(&requests(&home).await?[0], "authorization")? == "Bearer home-key");
    let only = check_document(&yaml, "groqbox", Some("home"), lookup_none).await?;
    ensure!(only.connections.len() == 1 && only.connections[0].id == "groqbox/home");
    ensure!(
        requests(&work).await?.len() == 1,
        "the filtered check dialed the other account"
    );
    ensure!(requests(&home).await?.len() == 2);
    Ok(())
}

#[tokio::test]
async fn the_protocol_endpoint_and_provider_timeout_are_the_ones_the_gateway_uses() -> Result<()> {
    let server = wiremock::MockServer::start().await;
    catalog(&server, serde_json::json!({"data": [{"id": "routed"}]})).await;
    let yaml = format!(
        "providers:\n  localbox:\n    api_base: http://127.0.0.1:1/v1\n    api_key: secret-key\n    api_protocol:\n      - \"*\": chat_completions\n    protocol_endpoints:\n      chat_completions: {base}/v1\n    timeouts:\n      connect_secs: 1\n      read_secs: 1\n",
        base = server.uri()
    );
    let started = std::time::Instant::now();
    let report = check_document(&yaml, "localbox", None, lookup_none).await?;
    ensure!(
        report.connections[0].answered,
        "{:?}",
        report.connections[0].error
    );
    ensure!(report.connections[0].endpoint == format!("{}/v1", server.uri()));
    ensure!(requests(&server).await?.len() == 1);
    ensure!(started.elapsed() < std::time::Duration::from_secs(5));
    Ok(())
}

#[tokio::test]
async fn a_closed_endpoint_fails_within_the_provider_timeout() -> Result<()> {
    let yaml = "\
providers:
  localbox:
    api_base: http://127.0.0.1:1/v1
    api_key: secret-key
    active: true
    timeouts:
      connect_secs: 1
      read_secs: 1
";
    let started = std::time::Instant::now();
    let config = parse_with(yaml, lookup_none)?;
    let report = check_loaded(&config, "localbox", None).await?;
    ensure!(started.elapsed() < std::time::Duration::from_secs(5));
    ensure!(!report.connections[0].answered);
    ensure!(
        config
            .providers
            .get("localbox")
            .is_some_and(|provider| provider.active)
    );
    Ok(())
}

#[tokio::test]
async fn a_missing_provider_is_rejected_without_a_dial() -> Result<()> {
    let server = wiremock::MockServer::start().await;
    let yaml = format!(
        "providers:\n  localbox:\n    api_base: {base}/v1\n",
        base = server.uri()
    );
    let error = check_document(&yaml, "missing", None, lookup_none)
        .await
        .err()
        .ok_or_else(|| anyhow!("missing provider was accepted"))?;
    ensure!(error.to_string().contains("missing"));
    ensure!(requests(&server).await?.is_empty());
    Ok(())
}
