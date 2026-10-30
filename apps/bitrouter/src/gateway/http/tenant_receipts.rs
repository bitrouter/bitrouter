//! Tenant wire delivery has one bounded slot and no receiver-held execution.
use crate::{
    gateway::{
        error::{BitrouterError, Result},
        model::{
            protocol::{StreamEncoder, inbound_adapter_for},
            stream::SseFrame,
        },
        pipeline::execution::{DeliveryPermit, Pipeline, PreparedPipelineStream},
    },
    tenancy::{
        execution::{TenantExecution, lifecycle::Registration},
        receipts::output::{self, Sender},
    },
};
use axum::{
    body::{Body, Bytes},
    response::IntoResponse,
    response::Response,
};
use bitrouter_ai::types::ApiProtocol;
use futures::StreamExt;
use std::sync::Arc;
const CHUNK: usize = 64 * 1024;
const BODY: usize = 16 * 1024 * 1024;
struct Wire {
    bytes: Bytes,
    delivery: Option<DeliveryPermit>,
}

async fn frames(
    sender: &Sender<Wire>,
    frames: Vec<SseFrame>,
    mut delivery: Option<DeliveryPermit>,
) -> Result<()> {
    if delivery.is_some() && frames.is_empty() {
        return Err(BitrouterError::internal(
            "successful terminal encoded to zero wire frames",
        ));
    }
    let last = frames.len().checked_sub(1);
    let mut total = 0usize;
    for (index, frame) in frames.into_iter().enumerate() {
        let wire = frame
            .to_wire()
            .map_err(|_| BitrouterError::UpstreamInvalidResponse {
                message: "tenant wire encoding failed".into(),
                usage: None,
            })?;
        total = total
            .checked_add(wire.len())
            .ok_or(BitrouterError::UpstreamUnavailable)?;
        if total > BODY {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: "tenant wire expansion limit".into(),
                usage: None,
            });
        }
        let last_chunk = wire.len().saturating_sub(1) / CHUNK;
        for (chunk_index, chunk) in wire.as_bytes().chunks(CHUNK).enumerate() {
            // Copy rather than slicing a shared larger backing allocation.
            let delivery = if Some(index) == last && chunk_index == last_chunk {
                delivery.take()
            } else {
                None
            };
            sender
                .send(Wire {
                    bytes: Bytes::copy_from_slice(chunk),
                    delivery,
                })
                .await?;
        }
    }
    Ok(())
}
async fn failed(
    sender: &Sender<Wire>,
    encoder: &mut dyn StreamEncoder,
    execution: &TenantExecution,
    error: BitrouterError,
    delivery: Option<DeliveryPermit>,
) {
    let error = if let Some(delivery) = delivery {
        match delivery.fail(error.clone()).await {
            Ok(()) => error,
            Err(error) => error,
        }
    } else {
        match execution
            .terminal_receipt(bitrouter_ai::tenancy::execution::Outcome::Failed)
            .await
        {
            Ok(()) => error,
            Err(error) => error,
        }
    };
    let _ = frames(
        sender,
        encoder.encode_stream_error(&error.stream_error()),
        None,
    )
    .await;
}
pub(super) fn stream(
    pipeline: Arc<Pipeline>,
    prepared: PreparedPipelineStream,
    execution: Arc<TenantExecution>,
    registration: Arc<Registration>,
    inbound: ApiProtocol,
    request_id: String,
) -> Result<Response> {
    let adapter = inbound_adapter_for(&inbound).ok_or(BitrouterError::UpstreamUnavailable)?;
    let mut encoder = adapter.stream_encoder(&request_id, &prepared.model_id);
    let model_id = prepared.model_id;
    let mut parts = prepared.parts;
    let receipt = execution.receipt_owner()?.clone();
    let (sender, mut receiver) = output::channel(&receipt)?;
    let tasks = pipeline
        .owned_tasks()
        .cloned()
        .ok_or(BitrouterError::UpstreamUnavailable)?;
    let keepalive = pipeline.keepalive_interval();
    tasks.spawn_execution(async move {
        let _registration=registration;
        let work=async {
            loop {
                let item=tokio::select! {
                    item=parts.next()=>item,
                    ()=tokio::time::sleep(keepalive)=>{
                        if frames(&sender,vec![SseFrame::Comment("keepalive".into())],None).await.is_err(){return;}
                        continue;
                    }
                };
                let Some(item)=item else{break};
                let prepared=match item {Ok(part)=>part,Err(error)=>{failed(&sender,encoder.as_mut(),&execution,error,None).await;return;}};
                let mut delivery=prepared.delivery;
                let mut encoded=match encoder.encode(&prepared.part) {Ok(frames)=>frames,Err(error)=>{failed(&sender,encoder.as_mut(),&execution,error.into(),delivery).await;return;}};
                if delivery.is_some() {
                    match parts.next().await {
                        None=>{},
                        Some(Err(error))=>{failed(&sender,encoder.as_mut(),&execution,error,delivery).await;return;},
                        Some(Ok(_))=>{failed(&sender,encoder.as_mut(),&execution,BitrouterError::internal("prepared stream continued after terminal"),delivery).await;return;},
                    }
                    match encoder.finish(){Ok(finish)=>encoded.extend(finish),Err(error)=>{failed(&sender,encoder.as_mut(),&execution,error.into(),delivery).await;return;}}
                    if let Err(error)=frames(&sender,encoded,delivery.take()).await {failed(&sender,encoder.as_mut(),&execution,error,None).await;}
                    return;
                }
                if let Err(error)=frames(&sender,encoded,None).await {failed(&sender,encoder.as_mut(),&execution,error,None).await;return;}
            }
            match encoder.finish(){Ok(finish)=>{let _=frames(&sender,finish,None).await;},Err(error)=>failed(&sender,encoder.as_mut(),&execution,error.into(),None).await}
        };
        tokio::select!{biased;()=receipt.cancelled.cancelled()=>{},()=sender.closed()=>{},()=work=>{}}
        // Dropping the encoder and upstream receiver releases all buffered output.
    });
    let stream = async_stream::stream! {
        while let Some(wire)=receiver.recv().await {
            if let Some(delivery)=wire.delivery && let Err(error)=delivery.deliver().await {
                // A fresh error encoder retains no accumulated provider output.
                if let Some(adapter)=inbound_adapter_for(&inbound) {
                    let mut encoder=adapter.stream_encoder(&request_id,&model_id);
                    for frame in encoder.encode_stream_error(&error.stream_error()) {
                        let bytes=match frame.to_wire() { Ok(bytes)=>bytes, Err(_)=>{yield Err(std::io::Error::other("terminal error encoding failed"));return;} };
                        for part in bytes.as_bytes().chunks(CHUNK){yield Ok::<_,std::io::Error>(Bytes::copy_from_slice(part));}
                    }
                }
                return;
            }
            // Final delivery acknowledgement and this yield have no intervening await.
            yield Ok::<_,std::io::Error>(wire.bytes);
        }
    };
    Ok(bitrouter_ai::axum::response::Reply::sse_body(Body::from_stream(stream)).into_response())
}

/// Serialize before authorizing the JSON terminal. A bounded writer prevents
/// both an oversized success response and a silent truncation after activation.
pub(super) fn json(value: &serde_json::Value) -> Result<Response> {
    let reply = bitrouter_ai::axum::response::Reply::json(value, BODY).map_err(|_| {
        BitrouterError::UpstreamInvalidResponse {
            message: "tenant JSON output exceeds the bounded delivery buffer".into(),
            usage: None,
        }
    })?;
    Ok(reply.into_response())
}

pub(super) fn bounded_json(
    response: Response,
    pipeline: &Pipeline,
    execution: Arc<TenantExecution>,
    registration: Arc<Registration>,
) -> Result<Response> {
    let (head, body) = response.into_parts();
    let receipt = execution.receipt_owner()?.clone();
    let (sender, mut receiver) = output::channel(&receipt)?;
    let tasks = pipeline
        .owned_tasks()
        .cloned()
        .ok_or(BitrouterError::UpstreamUnavailable)?;
    tasks.spawn_execution(async move {
        let _registration = registration;
        let _execution = execution;
        let work = async move {
            let mut body = body.into_data_stream();
            let mut total = 0usize;
            while let Some(item) = body.next().await {
                let bytes = match item {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        let _ = sender
                            .send(Err(std::io::Error::other("tenant response body failed")))
                            .await;
                        return;
                    }
                };
                let Some(next) = total.checked_add(bytes.len()) else {
                    return;
                };
                total = next;
                if total > BODY {
                    let _ = sender
                        .send(Err(std::io::Error::other("tenant response body limit")))
                        .await;
                    return;
                }
                for chunk in bytes.chunks(CHUNK) {
                    if sender
                        .send(Ok(Bytes::copy_from_slice(chunk)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        };
        tokio::select! {biased;()=receipt.cancelled.cancelled()=>{},()=work=>{}}
    });
    let body = Body::from_stream(
        async_stream::stream! {while let Some(bytes)=receiver.recv().await{yield bytes;}},
    );
    Ok(Response::from_parts(head, body))
}

#[cfg(test)]
mod tests;
