//! Model integration contracts independent of router and application state.
//!
//! Model semantics, wire codecs, selected-target transport and SSE framing.
//! Routing, account selection, application state and gateway policy live above
//! this crate. Selected-model HTTP calls use explicit effective credentials;
//! registered authentication and injected account transactions are explicit.
//! Published catalog schemas live here. Catalog fetching, persistence, retirement
//! policy and bundled baselines are application gateway implementations.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod auth;
pub mod catalog;
pub mod client;
pub mod conversion;
pub mod decisions;
pub mod diagnostics;
pub mod error;
pub mod event;
pub mod extension;
pub mod observe;
pub mod protocol;
pub mod providers;
pub mod stream;
pub mod target;
pub mod types;
