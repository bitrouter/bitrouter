//! Lightweight public contracts shared by BitRouter, BRO and the HTTP SDK.
//!
//! Public DTOs and intrinsic validation live here. Request execution, extension
//! callbacks, HTTP adapters and telemetry belong to the application. This crate
//! initializes no runtime and makes no network calls.

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
pub mod control;
pub mod decisions;
pub mod eval;
pub mod gateway;
pub mod policy;
pub mod protocol;
pub mod routing;
pub mod stream;
pub mod tenancy;
pub mod types;
