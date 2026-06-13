//! Payment gate traits shared between the SDK and payment plugins.

pub mod gate;

pub use gate::{PaymentGate, PaymentGateResult, PaymentRouteRequest};
