//! Original-provider evidence at the existing actual-send boundary.
//! Canonical Usage is deliberately never an input: its absent fields can be zero-filled.

use super::{ApiProtocol, BitrouterError, Result, SseEvent, StreamPart};
use crate::tenancy::receipts::{evidence::Protocol, journal::AttemptPermit};
use bitrouter_ai::tenancy::usage_evidence::Source;
use serde_json::Value;

fn unavailable() -> BitrouterError {
    BitrouterError::Forbidden("tenant receipt evidence is unavailable".into())
}

fn protocol(protocol: &ApiProtocol) -> Result<Protocol> {
    match protocol {
        ApiProtocol::ChatCompletions => Ok(Protocol::ChatCompletions),
        ApiProtocol::Responses => Ok(Protocol::Responses),
        ApiProtocol::Messages => Ok(Protocol::Messages),
        ApiProtocol::Decisions => Ok(Protocol::Decisions),
        _ => Err(unavailable()),
    }
}

pub(super) fn status(permit: &mut Option<AttemptPermit>, status: u16) -> Result<()> {
    if let Some(permit) = permit {
        permit.http_status(status).map_err(|_| unavailable())?;
    }
    Ok(())
}

pub(super) fn json(
    permit: &mut Option<AttemptPermit>,
    wire: &ApiProtocol,
    body: &Value,
) -> Result<()> {
    if let Some(permit) = permit {
        match body.get("usage") {
            Some(usage) => permit.observe_usage(protocol(wire)?, usage),
            None => permit.incomplete_usage(),
        }
        .map_err(|_| unavailable())?;
    }
    Ok(())
}

pub(super) fn json_text(
    permit: &mut Option<AttemptPermit>,
    wire: &ApiProtocol,
    text: &str,
) -> Result<()> {
    if permit.is_some()
        && let Ok(body) = serde_json::from_str(text)
    {
        json(permit, wire, &body)?;
    }
    Ok(())
}

pub(super) fn error_json_text(
    permit: &mut Option<AttemptPermit>,
    wire: &ApiProtocol,
    text: &str,
) -> Result<()> {
    if let Some(permit) = permit {
        match serde_json::from_str::<Value>(text) {
            Ok(body) => match body.get("usage") {
                Some(usage) => {
                    permit.observe_usage_frame(protocol(wire)?, Source::HttpError, usage)
                }
                None => permit.incomplete_usage(),
            },
            Err(_) => permit.observe_usage_frame(protocol(wire)?, Source::HttpError, &Value::Null),
        }
        .map_err(|_| unavailable())?;
    }
    Ok(())
}

/// Call only after the existing stateful decoder accepted this event's identity.
/// No arbitrary event gets a usage interpretation merely for containing that key.
pub(super) fn event(
    permit: &mut Option<AttemptPermit>,
    wire: &ApiProtocol,
    event: &SseEvent,
) -> Result<()> {
    let Some(permit) = permit else {
        return Ok(());
    };
    let Ok(body) = serde_json::from_str::<Value>(&event.data) else {
        return Ok(());
    };
    if let Some((source, usage)) = event_usage(wire, event.event.as_deref(), &body) {
        permit
            .observe_usage_frame(protocol(wire)?, source, usage)
            .map_err(|_| unavailable())?;
    }
    Ok(())
}

fn event_usage<'a>(
    wire: &ApiProtocol,
    label: Option<&str>,
    body: &'a Value,
) -> Option<(Source, &'a Value)> {
    let kind = body.get("type").and_then(Value::as_str);
    // Messages requires named events, whereas Responses also accepts the SSE
    // default event. A JSON usage key must never turn an ignored ping into usage.
    // https://platform.claude.com/docs/en/build-with-claude/streaming
    // https://developers.openai.com/api/reference/resources/responses
    if matches!(wire, ApiProtocol::Messages) && (label.is_none() || label != kind) {
        return None;
    }
    let explicit = label.filter(|label| *label != "message");
    if matches!(wire, ApiProtocol::Responses)
        && explicit
            .zip(kind)
            .is_some_and(|(label, kind)| label != kind)
    {
        return None;
    }
    let kind = kind.or(explicit);
    let (source, usage) = match wire {
        ApiProtocol::ChatCompletions => (Source::ChatChunk, body.get("usage")),
        ApiProtocol::Responses
            if matches!(
                kind,
                Some("response.completed" | "response.incomplete" | "response.failed")
            ) =>
        {
            (
                Source::ResponseTerminal,
                body.get("response")
                    .and_then(|response| response.get("usage")),
            )
        }
        ApiProtocol::Messages if kind == Some("message_start") => (
            Source::MessageStart,
            body.get("message").and_then(|message| message.get("usage")),
        ),
        ApiProtocol::Messages if kind == Some("message_delta") => {
            (Source::MessageDelta, body.get("usage"))
        }
        _ => return None,
    };
    usage.map(|usage| (source, usage))
}

pub(super) fn complete(permit: &mut Option<AttemptPermit>, successful: bool) -> Result<()> {
    if let Some(permit) = permit {
        permit
            .validated_completion(successful)
            .map_err(|_| unavailable())?;
    }
    Ok(())
}

pub(super) fn batch(
    permit: &Option<AttemptPermit>,
    parts: &[StreamPart],
    mut terminal: bool,
) -> Result<()> {
    if permit.is_none() {
        return Ok(());
    }
    for part in parts {
        if terminal && !matches!(part, StreamPart::Usage { .. }) {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: "upstream sent content after its terminal".into(),
                usage: None,
            });
        }
        terminal |= part.is_terminal();
    }
    Ok(())
}

pub(super) fn successful(part: &StreamPart) -> bool {
    match part {
        StreamPart::Finish {
            reason: bitrouter_ai::types::FinishReason::Error(_),
        } => false,
        StreamPart::ResponseCompleted { status, .. } => status != "failed",
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_requires_matching_eligible_event_and_json_type() {
        let delta = json!({"type":"message_delta","usage":{"output_tokens":999}});
        for label in [
            None,
            Some("ping"),
            Some("message"),
            Some("message_start"),
            Some("unknown"),
        ] {
            assert!(
                event_usage(&ApiProtocol::Messages, label, &delta).is_none(),
                "{label:?}"
            );
        }
        assert_eq!(
            event_usage(&ApiProtocol::Messages, Some("message_delta"), &delta),
            Some((Source::MessageDelta, &delta["usage"]))
        );
        let missing_type = json!({"usage":{"output_tokens":999}});
        assert!(
            event_usage(&ApiProtocol::Messages, Some("message_delta"), &missing_type).is_none()
        );
        let start = json!({"type":"message_start","message":{"usage":{"input_tokens":0}}});
        assert!(event_usage(&ApiProtocol::Messages, Some("message_delta"), &start).is_none());
        assert_eq!(
            event_usage(&ApiProtocol::Messages, Some("message_start"), &start),
            Some((Source::MessageStart, &start["message"]["usage"]))
        );
    }

    #[test]
    fn responses_rejects_contradictory_labels_and_accepts_sse_default() {
        let body = json!({"type":"response.completed","response":{"usage":{"output_tokens":0}}});
        for label in [
            Some("ping"),
            Some("response.failed"),
            Some("response.incomplete"),
        ] {
            assert!(event_usage(&ApiProtocol::Responses, label, &body).is_none());
        }
        for label in [None, Some("message"), Some("response.completed")] {
            assert_eq!(
                event_usage(&ApiProtocol::Responses, label, &body),
                Some((Source::ResponseTerminal, &body["response"]["usage"]))
            );
        }
    }
}
