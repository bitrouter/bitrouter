//! The service can inspect historical data and must refuse to adopt it.

use bitrouter::storage;

#[path = "database_capture/baseline_runtime.rs"]
mod baseline_runtime;
