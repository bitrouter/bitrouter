//! Provider-specific request adaptation over caller-injected authentication.
//!
//! These integrations select no storage paths, accounts, catalogs or login UI.

pub mod anthropic;
pub mod antigravity;
pub mod claude_code;
pub mod codex;
pub mod copilot;
pub mod supergrok;

#[cfg(feature = "pkce")]
pub mod login;

#[cfg(feature = "hosted")]
pub mod hosted;
