//! Per-session ACP substrate — one session, one agent.
pub mod dotdir;
pub mod down;
pub mod engine;
pub mod executor;
pub mod permissions;
pub mod session;
pub mod telemetry;
pub mod turn;
pub mod up;

// ── transitional re-exports (dropped once every importer has moved) ─────────
// The thin-proxy modules are moving into `bitrouter_sdk::acp` one at a time
// (issue #747). Each moved module is re-exported here **as a module**, so the
// old `bitrouter_substrate::<module>::Item` paths keep resolving to the single
// source of truth in the SDK while importers migrate.
pub use bitrouter_sdk::acp::translate;
