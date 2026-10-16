use bitrouter_ai::harness::api::ObservationError;
use bitrouter_ai::harness::thread::{ThreadObservation, ThreadView};
use bitrouter_ai::harness::turn::TurnSnapshot;
use futures_util::StreamExt;

use crate::error::{Error, Result, ServiceFailure};
use crate::stream::{EventStream, ServerEvent, SseDecoder};

pub(super) fn check_thread(actual: &str, expected: &str) -> Result<()> {
    if actual != expected {
        return Err(Error::Response(
            "harness response belongs to another Thread",
        ));
    }
    Ok(())
}

pub(super) fn check_instance(actual: &str, expected: &str) -> Result<()> {
    if actual != expected {
        return Err(Error::Response(
            "harness response belongs to another server instance",
        ));
    }
    Ok(())
}

pub(super) fn check_turn(turn: &TurnSnapshot, thread_id: &str, instance: &str) -> Result<()> {
    check_thread(&turn.thread_id, thread_id)?;
    check_instance(&turn.server_instance_id, instance)
}

pub(super) fn check_view(view: &ThreadView, thread_id: &str, instance: &str) -> Result<()> {
    check_thread(&view.thread.thread_id, thread_id)?;
    check_instance(&view.thread.server_instance_id, instance)?;
    if let Some(turn) = &view.latest_turn {
        check_turn(turn, thread_id, instance)?;
    }
    Ok(())
}

pub(super) fn decode(
    response: reqwest::Response,
    limit: usize,
    thread_id: String,
    instance: String,
) -> EventStream<ThreadObservation> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let mut chunks = response.bytes_stream();
    Box::pin(async_stream::try_stream! {
        let mut decoder = SseDecoder::new(limit);
        while let Some(chunk) = chunks.next().await {
            for byte in chunk.map_err(Error::transport)? {
                let Some(event) = decoder.accept(byte)? else { continue; };
                if event.event == "error" {
                    let body: ObservationError = serde_json::from_str(&event.data)
                        .map_err(|_| Error::Response("invalid harness observation error"))?;
                    Err(Error::HarnessObservation(Box::new(ServiceFailure {
                        status, headers: headers.clone(), body: Some(body), body_error: None,
                    })))?;
                }
                if event.event != "message" {
                    Err(Error::Response("unsupported harness observation event"))?;
                }
                let data: ThreadObservation = serde_json::from_str(&event.data)
                    .map_err(|_| Error::Response("invalid harness observation data"))?;
                match &data {
                    ThreadObservation::Snapshot { view, catchup, .. } => {
                        check_view(view, &thread_id, &instance)?;
                        for event in catchup {
                            check_thread(&event.thread_id, &thread_id)?;
                        }
                    }
                    // Historical committed events retain their original instance ID.
                    ThreadObservation::Event { event } => check_thread(&event.thread_id, &thread_id)?,
                    ThreadObservation::Live { event, .. } => {
                        check_thread(&event.thread_id, &thread_id)?;
                        check_instance(&event.server_instance_id, &instance)?;
                    }
                }
                yield ServerEvent { event: event.event, id: event.id, data };
            }
        }
        decoder.finish_attachment()?;
    })
}
