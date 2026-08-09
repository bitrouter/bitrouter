//! Per-session ACP substrate — one session, one agent.
//!
//! The thin-proxy engine now lives in [`bitrouter_sdk::acp`]; what remains
//! here is the per-repo state directory helper plus transitional re-exports.
pub mod dotdir;

// ── transitional re-exports (dropped once every importer has moved) ─────────
// The thin-proxy modules moved into `bitrouter_sdk::acp` (issue #747). Each is
// re-exported here **as a module**, so the old
// `bitrouter_substrate::<module>::Item` paths keep resolving to the single
// source of truth in the SDK while importers migrate.
pub use bitrouter_sdk::acp::down;
pub use bitrouter_sdk::acp::engine;
pub use bitrouter_sdk::acp::executor;
pub use bitrouter_sdk::acp::permissions;
pub use bitrouter_sdk::acp::session;
pub use bitrouter_sdk::acp::telemetry;
pub use bitrouter_sdk::acp::translate;
pub use bitrouter_sdk::acp::turn;
pub use bitrouter_sdk::acp::up;
