//! Product provider activation, configuration and permitted credential discovery.
//!
//! AI supplies catalog metadata. The application owns provider login, runtime
//! authentication, credential discovery and CLI installation, and projects
//! selected catalog sources into provider configuration.

pub mod apply;
pub mod builtin;
pub mod claude_code;
pub mod cli_install;
pub mod entry;
pub mod import;
pub mod registry;

#[cfg(test)]
mod test_env;
