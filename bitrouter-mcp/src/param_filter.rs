//! Parameter restriction types — re-exported from [`bitrouter_core::routers::admin`].
//!
//! All types and logic now live in core. This module provides re-exports
//! for backward compatibility within the MCP crate.

pub use bitrouter_core::routers::admin::{ParamRestrictions, ParamRule, ParamViolationAction};
