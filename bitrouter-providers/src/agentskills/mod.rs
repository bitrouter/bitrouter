//! Agent Skills protocol adaptor — filesystem-backed skill registry.
//!
//! Implements the [agentskills.io](https://agentskills.io) client-side model:
//! skills are `SKILL.md` files on disk, discovered by scanning directories at
//! startup and installed from remote registries on demand. No database required.

pub(crate) mod catalog;
pub(crate) mod installer;
pub mod registry;
pub(crate) mod scanner;
