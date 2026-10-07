//! Product provider activation, configuration and permitted credential discovery.
//!
//! AI owns catalog metadata, login mechanisms and selected-target authentication.
//! This application bridge selects sources and projects the catalog into SDK config.

pub mod apply;
pub mod builtin;
pub mod claude_code;
pub mod entry;
pub mod import;
pub mod registry;

#[cfg(test)]
mod test_env;
