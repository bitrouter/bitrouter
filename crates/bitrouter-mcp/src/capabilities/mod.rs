//! Capability ports: the traits the embedding binary injects so the
//! host-coupled behavior (reading the installed-skills root) lives app-side
//! while the crate keeps ownership of the tool schemas and descriptions.
//!
//! Ports whose action has been unified onto a shared report type move to
//! [`crate::actions`], next to the type they return — the routing port did, as
//! `actions::route`.
//!
//! Dependency inversion by design: this crate defines the traits in plain
//! `serde`/`serde_json` types, so it never grows a dependency on the app's
//! routing or storage crates. Each port's result JSON is built adapter-side;
//! the crate owns only the inputs.

pub mod skill_catalog;
pub mod skills;
