//! Application BitRouter Cloud account persistence, assembly and settings.

/// Account credential persistence and schema.
pub mod credentials;
/// Account credential resolution for hosted requests.
pub mod manager;
/// Account OAuth configuration resolution.
pub mod settings;

mod transaction;
