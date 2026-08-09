//! Capability ports: the traits the embedding binary injects so the
//! host-coupled behavior (resolving a route against this machine's config,
//! reading the installed-skills root) lives app-side while the crate keeps
//! ownership of the tool schemas and descriptions.
//!
//! Dependency inversion by design: this crate defines the traits in plain
//! `serde`/`serde_json` types, so it never grows a dependency on the app's
//! routing or storage crates. Each port's result JSON is built adapter-side;
//! the crate owns only the inputs.

pub mod routing;
pub mod skill_catalog;
pub mod skills;
