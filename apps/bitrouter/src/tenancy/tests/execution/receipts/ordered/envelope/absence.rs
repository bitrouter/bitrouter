//! Request-scoped configuration selection across actual provider wires.
use super::*;
use bitrouter_ai::tenancy::generation_defaults::{
    ConfigurationDefaults, GenerationDefaultsEnvelopeV2,
};
mod isolation;
mod matrix;
#[cfg(unix)]
mod predecessor;
mod refusals;

fn values(
    mode: ConfigurationDefaults,
    canonical: Value,
    supplemental: Value,
) -> Result<GenerationDefaultsEnvelopeV2> {
    let mut value =
        json!({"schema_version":2,"configuration_defaults":mode,"supplemental":supplemental});
    if !canonical.is_null() {
        value["canonical"] = canonical;
    }
    Ok(serde_json::from_value(value)?)
}

fn defaults(supplemental: Value) -> Result<GenerationDefaultsEnvelopeV2> {
    values(
        ConfigurationDefaults::PreserveAbsence,
        json!({"schema_version":1,"max_tokens":23}),
        supplemental,
    )
}

pub(in crate::tenancy::tests::execution::receipts::ordered) async fn invoke(
    harness: &OrderedHarness,
    binding: &OrderedDispatchBindingV2,
    defaults: GenerationDefaultsEnvelopeV2,
    protocol: GenerationProtocol,
    stream: bool,
    version: HttpVersion,
    fields: Value,
) -> Result<ExecutionId> {
    let client = GatewayClient::with_options(
        &harness.endpoint,
        None,
        TransportOptions {
            http_version: version,
            ..Default::default()
        },
    )?;
    let call = client
        .ordered_tenant(harness.token()?, binding)?
        .with_generation_defaults_envelope_v2(defaults)?;
    let mut request = match protocol {
        GenerationProtocol::Responses => json!({"model":MODEL,"input":"hello","stream":stream}),
        _ => json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream}),
    };
    request
        .as_object_mut()
        .context("request")?
        .extend(fields.as_object().context("fields")?.clone());
    macro_rules! send {
        ($method:ident,$request:ty,$end:expr) => {{
            let reply = call
                .$method(&serde_json::from_value::<$request>(request)?)
                .await?;
            ensure!(reply.metadata.status == 200);
            let id = reply.metadata.execution_id.context("execution identity")?;
            match reply.body {
                Body::Json(body) => {
                    ensure!(!stream);
                    ensure!(serde_json::to_string(&body)?.contains("receipt reply"));
                }
                Body::Stream(mut events) => {
                    let mut terminal = None;
                    let mut text_seen = false;
                    while let Some(event) = events.next().await {
                        let event = event?;
                        if let StreamItem::Event(ref value) = event {
                            text_seen |=
                                serde_json::to_string(&value.data)?.contains("receipt reply");
                        }
                        if let StreamItem::End(end) = event {
                            ensure!(terminal.is_none());
                            terminal = Some(end);
                        }
                    }
                    ensure!(terminal == Some($end) && text_seen);
                }
            }
            id
        }};
    }
    Ok(match protocol {
        GenerationProtocol::ChatCompletions => {
            send!(chat_reply, ChatRequest, ProtocolEnd::ChatDone)
        }
        GenerationProtocol::Messages => {
            send!(messages_reply, MessagesRequest, ProtocolEnd::MessagesStop)
        }
        GenerationProtocol::Responses => send!(
            responses_reply,
            ResponsesRequest,
            ProtocolEnd::ResponsesCompleted
        ),
    })
}
