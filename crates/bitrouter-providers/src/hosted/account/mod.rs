//! BitRouter Cloud account credentials, OAuth flows, and resolution.

/// Account credential persistence and schema.
pub mod credentials;
/// OAuth device and refresh token flows.
pub mod flow;
/// Account credential resolution for hosted requests.
pub mod manager;
/// OAuth authorization-server metadata discovery.
pub mod metadata;
/// Account OAuth configuration resolution.
pub mod settings;
