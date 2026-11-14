//! Separate AP SDK process; its loopback server is a normal Core test process.
use bitrouter_ai::{
    stream::{ProtocolEnd, StreamItem},
    tenancy::{
        generate_content::{OrderedGenerateContentCapabilitiesV1, OrderedGenerateContentRequestV1},
        generation_defaults::GenerationDefaultsEnvelopeV2,
        ordered::OrderedDispatchBindingV2,
    },
};
use bitrouter_sdk::gateway::{GatewayClient, HttpVersion, TransportOptions, tenancy::Body};
use futures::StreamExt;
use serde::Deserialize;
use std::io::{Read, Write};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    endpoint: String,
    token: String,
    binding: OrderedDispatchBindingV2,
    capabilities: OrderedGenerateContentCapabilitiesV1,
    request: OrderedGenerateContentRequestV1,
    defaults: Option<GenerationDefaultsEnvelopeV2>,
    h2: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = String::new();
    std::io::stdin().read_to_string(&mut bytes)?;
    let input: Input = serde_json::from_str(&bytes)?;
    if !std::mem::needs_drop::<serde_json::Number>() {
        return Err("AP was not enabled".into());
    }
    let gateway = GatewayClient::with_options(
        &input.endpoint,
        None,
        TransportOptions {
            http_version: if input.h2 {
                HttpVersion::Http2PriorKnowledge
            } else {
                HttpVersion::Http1Only
            },
            request_timeout: Some(std::time::Duration::from_secs(30)),
            ..Default::default()
        },
    )?;
    let mut call = gateway
        .ordered_tenant(&input.token, &input.binding)?
        .with_generate_content(&input.capabilities)?;
    if let Some(defaults) = input.defaults {
        call = call.with_generation_defaults_envelope_v2(defaults)?;
    }
    let reply = call.generate_content_reply(&input.request).await?;
    let execution = reply
        .metadata
        .execution_id
        .ok_or("missing execution identity")?;
    let mut frames = Vec::new();
    let mut terminal = false;
    match reply.body {
        Body::Json(body) => {
            if input.request.stream {
                return Err("unexpected JSON".into());
            }
            frames.push(body);
        }
        Body::Stream(mut events) => {
            if !input.request.stream {
                return Err("unexpected SSE".into());
            }
            while let Some(item) = events.next().await {
                match item? {
                    StreamItem::Event(event) => {
                        if terminal {
                            return Err("event after end".into());
                        }
                        frames.push(event.data);
                    }
                    StreamItem::End(end) => {
                        if terminal || end != ProtocolEnd::GenerateContentFinished {
                            return Err("invalid end".into());
                        }
                        terminal = true;
                    }
                }
            }
            if !terminal {
                return Err("missing native EOF end".into());
            }
        }
    }
    let output = serde_json::json!({"execution_id":execution,"ap_enabled":true,"frames":frames,"stream_terminal":terminal});
    std::io::stdout().write_all(&serde_json::to_vec(&output)?)?;
    Ok(())
}
