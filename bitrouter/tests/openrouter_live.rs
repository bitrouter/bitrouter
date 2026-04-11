//! Live integration tests against a running BitRouter server with OpenRouter.
//!
//! These tests send real HTTP requests through all three API surfaces
//! (OpenAI, Anthropic, Google) to verify that each agent's expected
//! protocol successfully routes through BitRouter to OpenRouter and back.
//!
//! # Prerequisites
//!
//! 1. Start BitRouter with an OpenRouter provider:
//!    ```sh
//!    # bitrouter.yaml:
//!    #   server:
//!    #     listen: "127.0.0.1:18787"
//!    #   providers:
//!    #     openrouter:
//!    #       api_key: "sk-or-..."
//!    bitrouter serve --config-file bitrouter.yaml
//!    ```
//!
//! 2. Run the tests:
//!    ```sh
//!    BITROUTER_TEST_URL=http://127.0.0.1:18787 \
//!    cargo test --package bitrouter --test openrouter_live -- --ignored
//!    ```
//!
//! # What is tested
//!
//! | API Surface                     | Agents                                                   |
//! |---------------------------------|----------------------------------------------------------|
//! | `POST /v1/chat/completions`     | codex, openclaw, deepagents, goose, opencode, cline      |
//! | `POST /v1/messages`             | claude, openclaw, deepagents, goose                      |
//! | `POST /v1beta/models/..`        | gemini                                                   |

use std::time::Duration;

use serde_json::Value;

fn base_url() -> String {
    std::env::var("BITROUTER_TEST_URL").unwrap_or_else(|_| "http://127.0.0.1:18787".to_owned())
}

// ── OpenAI Chat Completions ────────────────────────────────────────────
//
// Tests the /v1/chat/completions surface used by codex, openclaw,
// deepagents, goose, opencode, cline, kilo.

#[tokio::test]
#[ignore = "requires a running BitRouter server with OpenRouter provider"]
async fn live_openai_chat_completions() {
    let url = base_url();
    let client = reqwest::Client::new();

    let res = client
        .post(format!("{url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .json(&serde_json::json!({
            "model": "openrouter:anthropic/claude-3.5-haiku",
            "messages": [{"role": "user", "content": "Say hello in exactly 3 words."}],
            "max_tokens": 30
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("send request");

    let status = res.status().as_u16();
    let body: Value = res.json().await.expect("parse JSON");

    assert_eq!(status, 200, "OpenAI chat completions failed: {body}");
    assert_eq!(body["object"], "chat.completion");
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .expect("response content");
    eprintln!("  [codex/openclaw/deepagents/goose/opencode/cline] response: {content}");
}

#[tokio::test]
#[ignore = "requires a running BitRouter server with OpenRouter provider"]
async fn live_openai_chat_streaming() {
    let url = base_url();
    let client = reqwest::Client::new();

    let res = client
        .post(format!("{url}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .json(&serde_json::json!({
            "model": "openrouter:anthropic/claude-3.5-haiku",
            "messages": [{"role": "user", "content": "Say hi."}],
            "stream": true,
            "max_tokens": 20
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("send streaming request");

    assert_eq!(res.status(), 200, "streaming should return 200");
    let content_type = res
        .headers()
        .get("content-type")
        .expect("content-type header")
        .to_str()
        .expect("utf8");
    assert!(
        content_type.contains("text/event-stream"),
        "streaming content-type should be SSE, got: {content_type}"
    );
    let body = res.text().await.expect("read body");
    assert!(
        body.contains("data: "),
        "streaming body should contain SSE data lines"
    );
    eprintln!("  [streaming] received {} bytes of SSE data", body.len());
}

// ── Anthropic Messages ─────────────────────────────────────────────────
//
// Tests the /v1/messages surface used by claude, openclaw, deepagents, goose.

#[tokio::test]
#[ignore = "requires a running BitRouter server with OpenRouter provider"]
async fn live_anthropic_messages() {
    let url = base_url();
    let client = reqwest::Client::new();

    let res = client
        .post(format!("{url}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "test")
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "openrouter:anthropic/claude-3.5-haiku",
            "max_tokens": 30,
            "messages": [{"role": "user", "content": "Say hello in exactly 3 words."}]
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("send request");

    let status = res.status().as_u16();
    let body: Value = res.json().await.expect("parse JSON");

    assert_eq!(status, 200, "Anthropic messages failed: {body}");
    assert_eq!(body["type"], "message");
    let content = body["content"][0]["text"].as_str().expect("response text");
    eprintln!("  [claude/openclaw/deepagents/goose] response: {content}");
}

#[tokio::test]
#[ignore = "requires a running BitRouter server with OpenRouter provider"]
async fn live_anthropic_streaming() {
    let url = base_url();
    let client = reqwest::Client::new();

    let res = client
        .post(format!("{url}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-api-key", "test")
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "openrouter:anthropic/claude-3.5-haiku",
            "max_tokens": 20,
            "stream": true,
            "messages": [{"role": "user", "content": "Say hi."}]
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("send streaming request");

    assert_eq!(res.status(), 200, "Anthropic streaming should return 200");
    let body = res.text().await.expect("read body");
    assert!(body.contains("event: "), "Anthropic SSE uses named events");
    eprintln!(
        "  [claude streaming] received {} bytes of SSE data",
        body.len()
    );
}

// ── Google Generative AI ───────────────────────────────────────────────
//
// Tests the /v1beta/models surface used by gemini.

#[tokio::test]
#[ignore = "requires a running BitRouter server with OpenRouter provider"]
async fn live_google_generate_content() {
    let url = base_url();
    let client = reqwest::Client::new();

    let res = client
        .post(format!(
            "{url}/v1beta/models/openrouter:google/gemini-2.5-flash:generateContent"
        ))
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "contents": [{
                "role": "user",
                "parts": [{"text": "Say hello in exactly 3 words."}]
            }],
            "generationConfig": {
                "maxOutputTokens": 30
            }
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("send request");

    let status = res.status().as_u16();
    let body: Value = res.json().await.expect("parse JSON");

    assert_eq!(status, 200, "Google generateContent failed: {body}");
    let content = body["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .expect("response text");
    eprintln!("  [gemini] response: {content}");
}
