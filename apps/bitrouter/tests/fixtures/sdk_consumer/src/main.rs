//! Independent acceptance client: no filesystem, local IPC or server imports.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::{
    artifact::DistributionProfile,
    control::{ControlScope, PROTOCOL_VERSION, action::Action},
    decisions::DecisionRequest,
    protocol::{
        chat_completions::ChatRequest, messages::MessagesRequest, responses::ResponsesRequest,
    },
};
use bitrouter_sdk::{
    control::{ControlClient, ControlOptions},
    error::Error,
    gateway::{GatewayClient, TransportOptions},
};
use futures_util::StreamExt;
use serde_json::json;

const TEXT: &str = "hello from packaged gateway";
const INVALID: &str = "brvk_unregistered-packaged-acceptance-credential";

fn environment(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    ensure!(!value.is_empty(), "{name} must not be empty");
    Ok(value)
}

fn gateway(endpoint: &str, credential: Option<&str>) -> Result<GatewayClient> {
    Ok(GatewayClient::with_options(
        endpoint,
        credential,
        TransportOptions {
            connect_timeout: Duration::from_secs(2),
            read_timeout: Some(Duration::from_secs(10)),
            request_timeout: Some(Duration::from_secs(15)),
            ..TransportOptions::default()
        },
    )?)
}

fn control(endpoint: &str, credential: &str) -> Result<ControlClient> {
    Ok(ControlClient::with_options(
        endpoint,
        credential,
        ControlOptions {
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(10),
            ..ControlOptions::default()
        },
    )?)
}

fn chat(model: &str, stream: bool, prompt: &str) -> Result<ChatRequest> {
    Ok(serde_json::from_value(json!({
        "model": model, "stream": stream,
        "messages": [{"role": "user", "content": prompt}],
        "stream_options": {"include_usage": true}
    }))?)
}

fn messages(stream: bool) -> Result<MessagesRequest> {
    Ok(serde_json::from_value(json!({
        "model": "fixture-messages", "stream": stream, "max_tokens": 32,
        "messages": [{"role": "user", "content": "messages acceptance"}]
    }))?)
}

fn responses(stream: bool) -> Result<ResponsesRequest> {
    Ok(serde_json::from_value(json!({
        "model": "fixture-responses", "stream": stream,
        "input": "responses acceptance", "max_output_tokens": 32
    }))?)
}

fn gateway_failure<T>(
    result: bitrouter_sdk::error::Result<T>,
    status: u16,
    code: &str,
) -> Result<()> {
    let Err(Error::Service(failure)) = result else {
        bail!("expected public gateway HTTP {status}");
    };
    ensure!(failure.status == status, "wrong gateway HTTP status");
    ensure!(failure.body_error.is_none(), "undecodable public error");
    let body = serde_json::to_value(failure.body.context("missing public error")?)?;
    ensure!(body["error"]["code"] == code, "wrong public error code");
    ensure!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    Ok(())
}

async fn acceptance() -> Result<()> {
    let endpoint = environment("BITROUTER_SDK_INFERENCE_ENDPOINT")?;
    let credential = environment("BITROUTER_SDK_INFERENCE_CREDENTIAL")?;
    let control_endpoint = environment("BITROUTER_SDK_CONTROL_ENDPOINT")?;
    let control_credential = environment("BITROUTER_SDK_CONTROL_CREDENTIAL")?;
    ensure!(endpoint != control_endpoint && credential != control_credential);
    let client = gateway(&endpoint, Some(&credential))?;
    let operator = control(&control_endpoint, &control_credential)?;

    ensure!(client.health().await?.status == "ok");
    let models = client.models().await?;
    ensure!(models.object == "list");
    for name in [
        "fixture-chat",
        "fixture-messages",
        "fixture-responses",
        "fixture-decisions",
    ] {
        ensure!(
            models.data.iter().any(|model| model.id == name),
            "missing model {name}"
        );
    }

    let capabilities = operator.capabilities().await?;
    ensure!(capabilities.protocol == "bitrouter-control");
    ensure!(capabilities.protocol_version == PROTOCOL_VERSION);
    ensure!(capabilities.scopes == [ControlScope::Read]);
    ensure!(
        capabilities
            .server_instance_id
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    );
    ensure!(operator.can_action(Action::Status).await?);
    ensure!(!operator.can_action(Action::Reload).await?);
    let artifact = capabilities
        .artifact
        .context("missing packaged build identity")?;
    ensure!(artifact.profile == DistributionProfile::Gateway && artifact.features.is_empty());
    ensure!(
        artifact
            .source_revision
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    );
    let status = operator.status().await?;
    ensure!(status.running && status.pid.is_some());
    ensure!(
        status.socket.is_none(),
        "remote status disclosed a local IPC path"
    );
    ensure!(status.daemon_artifact.as_ref() == Some(&artifact));

    let reply = serde_json::to_value(
        client
            .chat_completions(&chat("fixture-chat", false, "chat acceptance")?)
            .await?,
    )?;
    ensure!(reply["choices"][0]["message"]["content"] == TEXT);
    ensure!(reply["choices"][0]["finish_reason"] == "stop");
    ensure!(reply["usage"]["total_tokens"] == 18);
    let mut stream = client
        .stream_chat_completions(&chat("fixture-chat", true, "chat acceptance")?)
        .await?;
    let (mut text, mut finished, mut usage) = (String::new(), false, false);
    while let Some(event) = stream.next().await {
        let value = serde_json::to_value(event?.data)?;
        if let Some(delta) = value["choices"][0]["delta"]["content"].as_str() {
            text.push_str(delta);
        }
        finished |= value["choices"][0]["finish_reason"] == "stop";
        usage |= value["usage"]["total_tokens"] == 18;
    }
    ensure!(text == TEXT && finished && usage, "incomplete Chat result");

    let reply = serde_json::to_value(client.messages(&messages(false)?).await?)?;
    ensure!(reply["content"][0]["text"] == TEXT);
    ensure!(reply["stop_reason"] == "end_turn");
    ensure!(reply["usage"]["input_tokens"] == 11 && reply["usage"]["output_tokens"] == 7);
    let mut stream = client.stream_messages(&messages(true)?).await?;
    let (mut text, mut stopped, mut input_usage, mut output_usage) =
        (String::new(), false, false, false);
    while let Some(event) = stream.next().await {
        let value = serde_json::to_value(event?.data)?;
        if value["type"] == "content_block_delta"
            && let Some(delta) = value["delta"]["text"].as_str()
        {
            text.push_str(delta);
        }
        // The gateway may publish final input usage on message_delta after
        // opening message_start before the provider's usage is available.
        input_usage |=
            value["message"]["usage"]["input_tokens"] == 11 || value["usage"]["input_tokens"] == 11;
        output_usage |= value["usage"]["output_tokens"] == 7;
        stopped |= value["type"] == "message_stop";
    }
    ensure!(
        text == TEXT && stopped && input_usage && output_usage,
        "incomplete Messages result"
    );

    let reply = serde_json::to_value(client.responses(&responses(false)?).await?)?;
    ensure!(reply["status"] == "completed");
    ensure!(reply["output"][0]["content"][0]["text"] == TEXT);
    ensure!(reply["usage"]["total_tokens"] == 18);
    let mut stream = client.stream_responses(&responses(true)?).await?;
    let (mut text, mut completed, mut usage) = (String::new(), false, false);
    while let Some(event) = stream.next().await {
        let value = serde_json::to_value(event?.data)?;
        if value["type"] == "response.output_text.delta"
            && let Some(delta) = value["delta"].as_str()
        {
            text.push_str(delta);
        }
        completed |=
            value["type"] == "response.completed" && value["response"]["status"] == "completed";
        usage |= value["response"]["usage"]["total_tokens"] == 18;
    }
    ensure!(
        text == TEXT && completed && usage,
        "incomplete Responses result"
    );

    let request: DecisionRequest = serde_json::from_value(json!({
        "model": "fixture-decisions", "input": "decisions acceptance",
        "questions": [{"type": "predicate", "instructions": "Is this a fixture?"}]
    }))?;
    let decision = serde_json::to_value(client.decisions(&request).await?)?;
    ensure!(decision["answers"][0]["type"] == "predicate");
    ensure!(decision["answers"][0]["name"].is_null());
    ensure!(decision["answers"][0]["probability"] == 0.75);
    ensure!(decision["usage"]["total_tokens"] == 18);

    // These are SDK POSTs, so open health/model discovery cannot mask missing auth.
    for token in [None, Some(INVALID), Some(control_credential.as_str())] {
        gateway_failure(
            gateway(&endpoint, token)?
                .chat_completions(&chat("fixture-chat", false, "denied")?)
                .await,
            401,
            "authentication_error",
        )?;
    }
    for token in [INVALID, credential.as_str()] {
        let result = control(&control_endpoint, token)?.capabilities().await;
        let Err(Error::Control(failure)) = result else {
            bail!("non-control credential was not rejected by control HTTP");
        };
        ensure!(failure.status == 401 && failure.body_error.is_none());
        ensure!(failure.body.context("missing control error")?.error.code == "unauthorized");
    }
    gateway_failure(
        client
            .chat_completions(&chat("missing-acceptance-model", false, "public error")?)
            .await,
        404,
        "not_found",
    )?;

    // The upstream socket fails after a content event, without a terminal marker.
    let mut stream = client
        .stream_chat_completions(&chat("fixture-chat", true, "interrupt acceptance")?)
        .await?;
    let (mut text, mut failed) = (String::new(), false);
    while let Some(event) = stream.next().await {
        match event {
            Ok(event) => {
                let value = serde_json::to_value(event.data)?;
                if let Some(delta) = value["choices"][0]["delta"]["content"].as_str() {
                    text.push_str(delta);
                }
                ensure!(
                    value["choices"][0]["finish_reason"] != "stop",
                    "interruption became success"
                );
            }
            Err(Error::Stream(_) | Error::IncompleteStream) => {
                failed = true;
                break;
            }
            Err(Error::Transport(error)) if !error.is_timeout() => {
                failed = true;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        text == "partial" && failed,
        "interrupted stream lost partial content or failure"
    );
    ensure!(
        stream.next().await.is_none(),
        "failed SDK stream did not terminate"
    );
    ensure!(client.health().await?.status == "ok");
    ensure!(operator.status().await?.running);
    println!(
        "{}",
        json!({"accepted": true, "scenario": "packaged-gateway-sdk-v1", "artifact": artifact})
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), acceptance())
        .await
        .context("SDK consumer exceeded its 90 second deadline")?
}
