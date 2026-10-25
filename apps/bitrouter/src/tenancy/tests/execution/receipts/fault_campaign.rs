//! Finite receipt-consumer faults. No public proof or synthetic owner is used.
use super::{Fixture, MODEL, draft, headers, store};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome},
    identity::ExecutionId,
};
use futures::FutureExt;
use sea_orm::{ConnectionTrait, Statement};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

mod fixture;
#[cfg(unix)]
mod unknown;
mod wire;

const WAIT: Duration = Duration::from_secs(10);
const WIRE_LIMIT: usize = 256 * 1024;
