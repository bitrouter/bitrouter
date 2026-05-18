//! CLI-side authentication helpers (interactive OAuth flows).
//!
//! Token persistence read by the runtime router lives in the `bitrouter`
//! crate under `bitrouter::auth::token_store`.

pub mod oauth;
