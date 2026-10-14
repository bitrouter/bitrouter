//! Lightweight public contracts shared by BitRouter clients and extensions.
//!
//! Model/protocol data, capability definitions and observation vocabulary live
//! here. Service execution, conversion, authentication and transport live in the
//! application. This crate performs no I/O or runtime initialization.

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

pub mod catalog;
pub mod control;
pub mod conversion;
pub mod decisions;
pub mod error;
pub mod event;
pub mod extension;
pub mod gateway;
pub mod observe;
pub mod protocol;
pub mod routing;
pub mod types;
