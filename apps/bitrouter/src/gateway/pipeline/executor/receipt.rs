//! Original-provider evidence at the existing actual-send boundary.
//! Canonical Usage is deliberately never an input: its absent fields can be zero-filled.

use super::{ApiProtocol, BitrouterError, Result, SseEvent, StreamPart};
use crate::tenancy::receipts::{evidence::Protocol, journal::AttemptPermit};
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
    let kind = body
        .get("type")
        .and_then(Value::as_str)
        .or(event.event.as_deref());
    let usage = match wire {
        ApiProtocol::ChatCompletions => body.get("usage"),
        ApiProtocol::Responses
            if matches!(
                kind,
                Some("response.completed" | "response.incomplete" | "response.failed")
            ) =>
        {
            body.get("response")
                .and_then(|response| response.get("usage"))
        }
        ApiProtocol::Messages if kind == Some("message_start") => {
            body.get("message").and_then(|message| message.get("usage"))
        }
        ApiProtocol::Messages if kind == Some("message_delta") => body.get("usage"),
        _ => None,
    };
    if let Some(usage) = usage {
        permit
            .observe_usage(protocol(wire)?, usage)
            .map_err(|_| unavailable())?;
    }
    Ok(())
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
