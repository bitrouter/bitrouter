//! Model integration contracts independent of router and application state.
//!
//! Model semantics, wire codecs, selected-target transport and SSE framing.
//! Routing, account selection, application state and gateway policy live above
//! this crate. Selected-model HTTP calls use explicit effective credentials;
//! registered authentication and injected account transactions are explicit.
//! Catalog schema, explicit loading/refresh and injected persistence also live
//! here. The application supplies offline baselines, paths and activation policy.

#![forbid(unsafe_code)]

pub mod auth;
pub mod catalog;
pub mod client;
pub mod conversion;
pub mod decisions;
pub mod diagnostics;
pub mod error;
pub mod protocol;
pub mod providers;
pub mod stream;
pub mod target;
pub mod types;
