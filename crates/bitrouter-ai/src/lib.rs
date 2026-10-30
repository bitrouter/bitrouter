//! Lightweight public contracts shared by BitRouter clients and extensions.
//!
//! Model/protocol data, capability definitions and observation vocabulary live
//! here. Optional Axum bindings adapt public payloads to HTTP without owning a
//! listener. Service execution, conversion and authentication remain with the
//! application. This crate initializes no runtime and makes no network calls.

#![forbid(unsafe_code)]

// Wire payloads may contain opaque replay proofs or provider diagnostics. Keep
// their default diagnostics as bounded as the existing model content types.
macro_rules! redacted_contract_debug {
    ($($name:ty),+ $(,)?) => {$(
        impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.debug_struct(stringify!($name)).finish_non_exhaustive()
            }
        }
    )+};
}

pub mod artifact;
#[cfg(feature = "axum")]
pub mod axum;
pub mod catalog;
pub mod control;
pub mod conversion;
pub mod core;
pub mod decisions;
pub mod error;
pub mod eval;
pub mod event;
pub mod extension;
pub mod gateway;
pub mod harness;
pub mod observe;
pub mod protocol;
pub mod routing;
pub mod stream;
pub mod tenancy;
pub mod types;
pub mod wire;
