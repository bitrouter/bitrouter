//! HTTP adapter and fixed-rule input checker used by the
//! `bitrouter-guardrails` executable.
//!
//! The adapter validates the strict request-check v1 envelope before invoking
//! a small synchronous [`adapter::CheckCallback`]. Callback work runs on
//! Tokio's blocking pool under a 32-permit semaphore. A caller or host timing
//! out does not forcibly cancel synchronous matching already running; the
//! permit stays with that work until it returns.
//!
//! This service is intentionally input-only. It scans the ordered text
//! projection supplied by the host: system/message text, reasoning, tool-call
//! arguments, textual or JSON-rendered tool results, and approval responses.
//! The projection excludes media, file ids, source metadata, provider output,
//! later tool-loop activity, and nested requests. `coverage` reports excluded
//! entry media, but an allow decision does not claim those surfaces were
//! inspected. Unlike the legacy in-process plugin, this service does not scan
//! output, redact text, or apply globally to direct-model requests.

#![forbid(unsafe_code)]

pub mod adapter;
pub mod checker;
pub mod startup;

/// Service package version returned in validated checker responses.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
