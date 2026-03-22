//! Integration test: bitrouter A2aClient ↔ real A2A v0.3.0 agent.
//!
//! Requires an A2A agent running at localhost:10999.
//! Run: cargo test -p bitrouter-a2a --features client --test a2a_v030_integration -- --ignored

use bitrouter_a2a::client::a2a_client::{A2aClient, SendMessageResult};
use bitrouter_a2a::request::{SendMessageConfiguration, SendMessageRequest};

const AGENT_URL: &str = "http://localhost:10999";

#[tokio::test]
#[ignore]
async fn discover_v030_agent_card() {
    let client = A2aClient::new();
    let card = client.discover(AGENT_URL).await.expect("discovery failed");

    println!("Agent: {}", card.name);
    println!("Protocol: {}", card.protocol_version);
    println!("URL: {}", card.url);

    assert_eq!(card.protocol_version, "0.3.0");
    assert!(!card.name.is_empty());
    assert!(!card.url.is_empty());
    assert!(!card.skills.is_empty());
}

#[tokio::test]
#[ignore]
async fn resolve_endpoint_from_v030_card() {
    let client = A2aClient::new();
    let card = client.discover(AGENT_URL).await.expect("discovery failed");

    let endpoint = A2aClient::resolve_endpoint(&card);
    assert!(!endpoint.is_empty());
    println!("Resolved endpoint: {endpoint}");
}

#[tokio::test]
#[ignore]
async fn send_text_and_parse_response() {
    let client = A2aClient::new();
    let card = client.discover(AGENT_URL).await.expect("discovery failed");
    let endpoint = A2aClient::resolve_endpoint(&card);

    let result = client
        .send_text(&endpoint, "Use the echo tool to echo: integration test")
        .await
        .expect("send_text failed");

    match result {
        SendMessageResult::Task(task) => {
            println!("Task: id={}, state={:?}", task.id, task.status.state);
            println!("  context_id: {}", task.context_id);
            for artifact in &task.artifacts {
                let name = artifact.name.as_deref().unwrap_or("(unnamed)");
                println!("  artifact '{name}': {:?}", artifact.parts);
            }
            assert!(!task.id.is_empty());
            assert!(!task.context_id.is_empty());
        }
        SendMessageResult::Message(msg) => {
            println!("Message: role={:?}, parts={:?}", msg.role, msg.parts);
        }
    }
}

#[tokio::test]
#[ignore]
async fn send_message_with_configuration() {
    let client = A2aClient::new();
    let card = client.discover(AGENT_URL).await.expect("discovery failed");
    let endpoint = A2aClient::resolve_endpoint(&card);

    let request = SendMessageRequest {
        message: A2aClient::text_message("Use the add tool to compute 100 + 200"),
        configuration: Some(SendMessageConfiguration {
            accepted_output_modes: Some(vec!["text/plain".to_string()]),
            push_notification_config: None,
            history_length: Some(0),
            blocking: Some(true),
        }),
        metadata: None,
    };

    let result = client
        .send_message(&endpoint, request)
        .await
        .expect("send_message failed");

    match &result {
        SendMessageResult::Task(task) => {
            println!("Task state: {:?}", task.status.state);
            for artifact in &task.artifacts {
                println!("  artifact: {:?}", artifact.parts);
            }
        }
        SendMessageResult::Message(msg) => {
            println!("Message: {:?}", msg.parts);
        }
    }
}
