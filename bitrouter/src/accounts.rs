//! Account-service re-exports for `bitrouter-cli`.
//!
//! Re-surfaces the request/response DTOs the CLI uses to talk to the
//! `/admin/keys/virtual` endpoint, so it does not need a direct
//! dependency on `bitrouter-accounts`.

pub use bitrouter_accounts::service::{CreateVirtualKeyRequest, CreateVirtualKeyResponse};
